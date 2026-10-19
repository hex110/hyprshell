//! One-shot window thumbnails via `ext-image-copy-capture-v1`.
//!
//! Windows are matched to their `ext-foreign-toplevel-list` handle through the Hyprland
//! `stableId`, which the compositor also uses as the toplevel identifier. Every call opens its
//! own wayland connection, so it is meant to run on a worker thread.

use anyhow::Context;
use rustix::event::{PollFd, PollFlags, poll};
use std::fs::{File, OpenOptions};
use std::os::fd::AsFd;
use std::os::unix::fs::FileExt;
use std::time::{Duration, Instant};
use tracing::{debug, trace, warn};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_buffer, wl_registry, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, WEnum, event_created_child};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_handle_v1::{
    self, ExtForeignToplevelHandleV1,
};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_list_v1::{
    self, ExtForeignToplevelListV1,
};
use wayland_protocols::ext::image_capture_source::v1::client::ext_foreign_toplevel_image_capture_source_manager_v1::ExtForeignToplevelImageCaptureSourceManagerV1;
use wayland_protocols::ext::image_capture_source::v1::client::ext_image_capture_source_v1::ExtImageCaptureSourceV1;
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_frame_v1::{
    self, ExtImageCopyCaptureFrameV1,
};
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_manager_v1::{
    ExtImageCopyCaptureManagerV1, Options,
};
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_session_v1::{
    self, ExtImageCopyCaptureSessionV1,
};

/// A captured frame, little-endian BGRA/BGRX (`wl_shm` Argb8888/Xrgb8888).
#[derive(Debug)]
pub struct Thumbnail {
    pub stable_id: String,
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub has_alpha: bool,
    pub data: Vec<u8>,
}

#[derive(Default)]
struct Target {
    stable_id: String,
    max_size: (u32, u32),
    handle: Option<ExtForeignToplevelHandleV1>,
    session: Option<ExtImageCopyCaptureSessionV1>,
    size: Option<(u32, u32)>,
    format: Option<wl_shm::Format>,
    session_done: bool,
    started: bool,
    file: Option<File>,
    ready: bool,
    failed: bool,
}

#[derive(Default)]
struct State {
    toplevels: Vec<(ExtForeignToplevelHandleV1, String)>,
    targets: Vec<Target>,
}

/// Capture the windows with the given Hyprland stable ids. Windows that can't be captured are
/// left out of the result. Each frame is box-filtered down to fit the max size given with its id,
/// so the toolkit never sees a full-resolution texture. Gives up after `timeout`.
pub fn capture_windows(
    requests: &[(String, (u32, u32))],
    timeout: Duration,
) -> anyhow::Result<Vec<Thumbnail>> {
    let deadline = Instant::now() + timeout;
    let conn = Connection::connect_to_env().context("connecting to wayland")?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
    let qh = queue.handle();
    let mut state = State::default();

    let list: ExtForeignToplevelListV1 = globals
        .bind(&qh, 1..=1, ())
        .context("compositor has no ext-foreign-toplevel-list")?;
    let source_manager: ExtForeignToplevelImageCaptureSourceManagerV1 = globals
        .bind(&qh, 1..=1, ())
        .context("compositor has no ext-foreign-toplevel-image-capture-source")?;
    let copy_manager: ExtImageCopyCaptureManagerV1 = globals
        .bind(&qh, 1..=1, ())
        .context("compositor has no ext-image-copy-capture")?;
    let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).context("no wl_shm")?;

    // two roundtrips: announce toplevels, then their identifiers
    queue.roundtrip(&mut state)?;
    queue.roundtrip(&mut state)?;
    trace!(
        "toplevel identifiers: {:?}",
        state.toplevels.iter().map(|(_, id)| id).collect::<Vec<_>>()
    );

    for (id, max_size) in requests {
        let mut target = Target {
            stable_id: id.clone(),
            max_size: *max_size,
            ..Target::default()
        };
        if let Some((handle, _)) = state.toplevels.iter().find(|(_, ident)| ident == id) {
            let idx = state.targets.len();
            let source = source_manager.create_source(handle, &qh, ());
            let session = copy_manager.create_session(&source, Options::empty(), &qh, idx);
            source.destroy();
            target.handle = Some(handle.clone());
            target.session = Some(session);
        } else {
            debug!("no toplevel handle for stable id {id}");
            target.failed = true;
        }
        state.targets.push(target);
    }

    // wait for buffer constraints, start a copy as soon as a session is ready
    while !state.targets.iter().all(|t| t.ready || t.failed) {
        for idx in 0..state.targets.len() {
            start_capture(&mut state, idx, &qh, &shm)?;
        }
        if !dispatch_timeout(&mut queue, &mut state, &conn, deadline)? {
            warn!("thumbnail capture timed out");
            break;
        }
    }

    trace!(
        "capture states: {:?}",
        state
            .targets
            .iter()
            .map(|t| (
                &t.stable_id,
                t.size,
                t.format,
                t.session_done,
                t.started,
                t.ready,
                t.failed
            ))
            .collect::<Vec<_>>()
    );
    let mut out = Vec::new();
    for target in &state.targets {
        if !target.ready {
            continue;
        }
        let (Some((width, height)), Some(file), Some(format)) =
            (target.size, target.file.as_ref(), target.format)
        else {
            continue;
        };
        let stride = width as usize * 4;
        let mut data = vec![0u8; stride * height as usize];
        if let Err(e) = file.read_exact_at(&mut data, 0) {
            warn!("reading thumbnail for {}: {e}", target.stable_id);
            continue;
        }
        let (width, height, stride, data) =
            downscale(&data, width, height, stride, target.max_size);
        out.push(Thumbnail {
            stable_id: target.stable_id.clone(),
            width,
            height,
            stride,
            has_alpha: format == wl_shm::Format::Argb8888,
            data,
        });
    }

    for target in &state.targets {
        if let Some(session) = &target.session {
            session.destroy();
        }
    }
    list.stop();
    let _ = conn.flush();
    Ok(out)
}

/// Area-average downscale of 4-byte pixels to fit inside `max`, never upscales.
#[allow(clippy::cast_sign_loss)] // the ratio is in (0, 1], so the scaled sizes are positive
fn downscale(
    src: &[u8],
    width: u32,
    height: u32,
    stride: usize,
    max: (u32, u32),
) -> (u32, u32, usize, Vec<u8>) {
    let ratio = (f64::from(max.0) / f64::from(width))
        .min(f64::from(max.1) / f64::from(height))
        .min(1.0);
    if ratio >= 1.0 {
        return (width, height, stride, src.to_vec());
    }
    let nw = ((f64::from(width) * ratio) as u32).max(1);
    let nh = ((f64::from(height) * ratio) as u32).max(1);
    let mut out = vec![0u8; nw as usize * nh as usize * 4];
    for dy in 0..nh {
        let sy0 = (u64::from(dy) * u64::from(height) / u64::from(nh)) as usize;
        let sy1 = (((u64::from(dy) + 1) * u64::from(height) / u64::from(nh)) as usize).max(sy0 + 1);
        for dx in 0..nw {
            let sx0 = (u64::from(dx) * u64::from(width) / u64::from(nw)) as usize;
            let sx1 =
                (((u64::from(dx) + 1) * u64::from(width) / u64::from(nw)) as usize).max(sx0 + 1);
            let mut acc = [0u32; 4];
            for sy in sy0..sy1 {
                let row = &src[sy * stride..];
                for sx in sx0..sx1 {
                    for (c, a) in acc.iter_mut().enumerate() {
                        *a += u32::from(row[sx * 4 + c]);
                    }
                }
            }
            let n = ((sy1 - sy0) * (sx1 - sx0)) as u32;
            let o = (dy as usize * nw as usize + dx as usize) * 4;
            for c in 0..4 {
                out[o + c] = (acc[c] / n) as u8;
            }
        }
    }
    (nw, nh, nw as usize * 4, out)
}

fn start_capture(
    state: &mut State,
    idx: usize,
    qh: &QueueHandle<State>,
    shm: &wl_shm::WlShm,
) -> anyhow::Result<()> {
    let target = &mut state.targets[idx];
    if target.started || target.failed || !target.session_done {
        return Ok(());
    }
    let (Some((width, height)), Some(format), Some(session)) =
        (target.size, target.format, target.session.as_ref())
    else {
        // constraints done but no usable shm format
        target.failed = true;
        return Ok(());
    };
    let width = i32::try_from(width).context("capture width")?;
    let height = i32::try_from(height).context("capture height")?;
    let stride = width.checked_mul(4).context("capture stride")?;
    let len = stride.checked_mul(height).context("capture size")?;

    let runtime = std::env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR not set")?;
    let path = std::path::Path::new(&runtime)
        .join(format!("hyprshell-thumb-{}-{idx}", std::process::id()));
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)
        .context("creating shm file")?;
    std::fs::remove_file(&path).context("unlinking shm file")?;
    file.set_len(u64::try_from(len)?)?;

    let pool = shm.create_pool(file.as_fd(), len, qh, ());
    let buffer = pool.create_buffer(0, width, height, stride, format, qh, ());
    pool.destroy();

    let frame = session.create_frame(qh, idx);
    frame.attach_buffer(&buffer);
    frame.damage_buffer(0, 0, width, height);
    frame.capture();

    target.file = Some(file);
    target.started = true;
    Ok(())
}

/// Dispatch events until something new arrived. Returns false on timeout.
fn dispatch_timeout(
    queue: &mut EventQueue<State>,
    state: &mut State,
    conn: &Connection,
    deadline: Instant,
) -> anyhow::Result<bool> {
    if queue.dispatch_pending(state)? > 0 {
        return Ok(true);
    }
    conn.flush()?;
    let Some(guard) = queue.prepare_read() else {
        queue.dispatch_pending(state)?;
        return Ok(true);
    };
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Ok(false);
    }
    let fd = guard.connection_fd();
    let mut fds = [PollFd::new(&fd, PollFlags::IN)];
    let timeout_ms = i32::try_from(remaining.as_millis()).unwrap_or(i32::MAX);
    let n = poll(&mut fds, timeout_ms)?;
    if n == 0 {
        return Ok(false);
    }
    guard.read()?;
    queue.dispatch_pending(state)?;
    Ok(true)
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

macro_rules! ignore_events {
    ($($proxy:ty),* $(,)?) => {$(
        impl Dispatch<$proxy, ()> for State {
            fn event(
                _: &mut Self,
                _: &$proxy,
                _: <$proxy as wayland_client::Proxy>::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
            }
        }
    )*};
}

ignore_events!(
    wl_shm::WlShm,
    wl_shm_pool::WlShmPool,
    wl_buffer::WlBuffer,
    ExtForeignToplevelImageCaptureSourceManagerV1,
    ExtImageCaptureSourceV1,
    ExtImageCopyCaptureManagerV1,
);

impl Dispatch<ExtForeignToplevelListV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtForeignToplevelListV1,
        event: ext_foreign_toplevel_list_v1::Event,
        _data: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_foreign_toplevel_list_v1::Event::Toplevel { toplevel } = event {
            state.toplevels.push((toplevel, String::new()));
        }
    }

    event_created_child!(State, ExtForeignToplevelListV1, [
        ext_foreign_toplevel_list_v1::EVT_TOPLEVEL_OPCODE => (ExtForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ExtForeignToplevelHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &ExtForeignToplevelHandleV1,
        event: ext_foreign_toplevel_handle_v1::Event,
        _data: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_foreign_toplevel_handle_v1::Event::Identifier { identifier } = event
            && let Some((_, ident)) = state.toplevels.iter_mut().find(|(h, _)| h == proxy)
        {
            *ident = identifier;
        }
    }
}

impl Dispatch<ExtImageCopyCaptureSessionV1, usize> for State {
    fn event(
        state: &mut Self,
        _: &ExtImageCopyCaptureSessionV1,
        event: ext_image_copy_capture_session_v1::Event,
        idx: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(target) = state.targets.get_mut(*idx) else {
            return;
        };
        match event {
            ext_image_copy_capture_session_v1::Event::BufferSize { width, height } => {
                target.size = Some((width, height));
            }
            ext_image_copy_capture_session_v1::Event::ShmFormat { format } => {
                // prefer alpha-less, it needs no premultiplication handling
                if let WEnum::Value(f @ (wl_shm::Format::Xrgb8888 | wl_shm::Format::Argb8888)) =
                    format
                    && (target.format.is_none() || f == wl_shm::Format::Xrgb8888)
                {
                    target.format = Some(f);
                }
            }
            ext_image_copy_capture_session_v1::Event::Done => target.session_done = true,
            ext_image_copy_capture_session_v1::Event::Stopped => target.failed = true,
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, usize> for State {
    fn event(
        state: &mut Self,
        frame: &ExtImageCopyCaptureFrameV1,
        event: ext_image_copy_capture_frame_v1::Event,
        idx: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(target) = state.targets.get_mut(*idx) else {
            return;
        };
        match event {
            ext_image_copy_capture_frame_v1::Event::Ready => {
                target.ready = true;
                frame.destroy();
            }
            ext_image_copy_capture_frame_v1::Event::Failed { reason } => {
                debug!("capture of {} failed: {reason:?}", target.stable_id);
                target.failed = true;
                frame.destroy();
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::downscale;

    fn solid(width: u32, height: u32, pixel: [u8; 4]) -> Vec<u8> {
        pixel
            .iter()
            .copied()
            .cycle()
            .take(width as usize * height as usize * 4)
            .collect()
    }

    #[test]
    fn downscale_never_upscales() {
        let src = solid(4, 2, [1, 2, 3, 255]);
        let (w, h, stride, data) = downscale(&src, 4, 2, 16, (100, 100));
        assert_eq!((w, h, stride), (4, 2, 16));
        assert_eq!(data, src);
    }

    #[test]
    fn downscale_fits_box_and_keeps_aspect() {
        let src = solid(400, 200, [10, 20, 30, 255]);
        let (w, h, stride, data) = downscale(&src, 400, 200, 1600, (100, 100));
        assert_eq!((w, h, stride), (100, 50, 400));
        assert_eq!(data.len(), 100 * 50 * 4);
        let pattern = [10, 20, 30, 255];
        assert!(data.iter().zip(pattern.iter().cycle()).all(|(a, b)| a == b));
    }

    #[test]
    fn downscale_averages_blocks() {
        // left column black, right column white, squeezed into one pixel
        let src = [0, 0, 0, 255, 200, 200, 200, 255];
        let (w, h, _, data) = downscale(&src, 2, 1, 8, (1, 1));
        assert_eq!((w, h), (1, 1));
        assert_eq!(data, [100, 100, 100, 255]);
    }
}
