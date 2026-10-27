//! Native Wayland backend: registry/output discovery and wlr-screencopy capture.
//! Each call opens a connection to the *target's* socket; no env fallback across targets.
use crate::coords::{LogicalRect, OutputInfo};
use std::{collections::HashMap, os::unix::net::UnixStream, path::PathBuf, sync::{Arc, Mutex}};
use wayland_client::{
    globals::{registry_queue_init, GlobalListContents},
    protocol::{wl_output, wl_registry, wl_shm, wl_shm_pool},
    Connection, Dispatch, QueueHandle,
};
use wayland_protocols::xdg::xdg_output::zv1::client::{zxdg_output_manager_v1, zxdg_output_v1};
use wayland_protocols_wlr::screencopy::v1::client::{zwlr_screencopy_frame_v1, zwlr_screencopy_manager_v1};

#[derive(Clone, Debug)]
pub struct TargetSpec { pub id: String, pub socket: PathBuf, pub session_bus: Option<String>, pub kind: String }
impl TargetSpec {
    pub fn host() -> Result<Self, String> {
        let display = std::env::var("WAYLAND_DISPLAY").map_err(|_| "WAYLAND_DISPLAY not set for host target".to_string())?;
        let runtime = std::env::var("XDG_RUNTIME_DIR").map_err(|_| "XDG_RUNTIME_DIR not set".to_string())?;
        let socket = if display.starts_with('/') { PathBuf::from(display) } else { PathBuf::from(runtime).join(display) };
        Ok(Self { id: "host".into(), socket, session_bus: std::env::var("DBUS_SESSION_BUS_ADDRESS").ok(), kind: "host".into() })
    }
}

#[derive(Default, Clone)]
struct RawOutput { name: Option<String>, make: Option<String>, model: Option<String>, mode_w: i32, mode_h: i32, scale: i32, transform: String, transform_code: i32, xdg_name: Option<String>, logical_pos: Option<(i32, i32)>, logical_size: Option<(i32, i32)> }

#[derive(Default)]
struct DiscState { outputs: HashMap<u32, RawOutput>, order: Vec<u32> }
impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for DiscState {
    fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<wl_output::WlOutput, u32> for DiscState {
    fn event(state: &mut Self, _: &wl_output::WlOutput, event: wl_output::Event, data: &u32, _: &Connection, _: &QueueHandle<Self>) {
        let o = state.outputs.entry(*data).or_default();
        match event {
            wl_output::Event::Geometry { make, model, transform, .. } => { o.make = Some(make); o.model = Some(model); if let wayland_client::WEnum::Value(t) = transform { o.transform_code = t as i32; o.transform = format!("{t:?}"); } }
            wl_output::Event::Mode { flags, width, height, .. } => {
                // Only the Current mode describes the active physical size; compositors
                // (e.g. Weston) also send the other advertised modes.
                let current = match flags { wayland_client::WEnum::Value(f) => f.contains(wl_output::Mode::Current), _ => false };
                if current { o.mode_w = width; o.mode_h = height; }
            }
            wl_output::Event::Scale { factor } => o.scale = factor,
            wl_output::Event::Name { name } => o.name = Some(name),
            _ => {}
        }
    }
}
impl Dispatch<zxdg_output_v1::ZxdgOutputV1, u32> for DiscState {
    fn event(state: &mut Self, _: &zxdg_output_v1::ZxdgOutputV1, event: zxdg_output_v1::Event, data: &u32, _: &Connection, _: &QueueHandle<Self>) {
        let o = state.outputs.entry(*data).or_default();
        match event {
            zxdg_output_v1::Event::LogicalPosition { x, y } => o.logical_pos = Some((x, y)),
            zxdg_output_v1::Event::LogicalSize { width, height } => o.logical_size = Some((width, height)),
            zxdg_output_v1::Event::Name { name } => o.xdg_name = Some(name),
            _ => {}
        }
    }
}
wayland_client::delegate_noop!(DiscState: ignore zxdg_output_manager_v1::ZxdgOutputManagerV1);
wayland_client::delegate_noop!(DiscState: ignore wl_shm::WlShm);
wayland_client::delegate_noop!(DiscState: ignore zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1);

pub struct Discovery { pub globals: Vec<(String, String, u32)>, pub outputs: Vec<OutputInfo>, /// Registry global name per output (parallel to `outputs`), so capture binds the exact output.
    pub output_names: Vec<u32> }

pub fn connect(spec: &TargetSpec) -> Result<Connection, String> {
    let stream = UnixStream::connect(&spec.socket).map_err(|e| format!("connect {}: {e}", spec.socket.display()))?;
    Connection::from_socket(stream).map_err(|e| format!("wayland connect: {e}"))
}

pub fn discover(spec: &TargetSpec) -> Result<Discovery, String> {
    let conn = connect(spec)?;
    let (globals, mut queue) = registry_queue_init::<DiscState>(&conn).map_err(|e| format!("registry: {e}"))?;
    let qh = queue.handle();
    let mut state = DiscState::default();
    let global_list: Vec<(String, String, u32)> = globals.contents().clone_list().iter()
        .map(|g| (g.name.to_string(), g.interface.clone(), g.version)).collect();
    // Bind outputs (and xdg-output for logical geometry).
    let mut outputs_proxies = Vec::new();
    for g in globals.contents().clone_list().iter() {
        if g.interface == "wl_output" {
            let out: wl_output::WlOutput = globals.registry().bind(g.name, g.version.min(4), &qh, g.name);
            state.outputs.entry(g.name).or_default(); state.order.push(g.name);
            outputs_proxies.push((g.name, out));
        }
    }
    let xdg_mgr: Result<zxdg_output_manager_v1::ZxdgOutputManagerV1, _> = globals.bind(&qh, 1..=3, ());
    let mut _xdg_outs = Vec::new();
    if let Ok(mgr) = &xdg_mgr { for (name, out) in &outputs_proxies { _xdg_outs.push(mgr.get_xdg_output(out, &qh, *name)); } }
    queue.roundtrip(&mut state).map_err(|e| format!("roundtrip: {e}"))?;
    queue.roundtrip(&mut state).map_err(|e| format!("roundtrip: {e}"))?;
    let mut outputs = Vec::new();
    for name_id in &state.order {
        let raw = state.outputs.get(name_id).cloned().unwrap_or_default();
        let name = raw.xdg_name.clone().or(raw.name.clone()).unwrap_or_else(|| format!("output-{name_id}"));
        let scale = if raw.scale > 0 { raw.scale as f64 } else { 1.0 };
        // Prefer xdg logical geometry; fall back to mode/scale.
        let (lx, ly) = raw.logical_pos.unwrap_or((0, 0));
        // Without xdg logical size, derive it from mode/scale, swapping axes for
        // quarter-turn transforms so scale is not computed from rotated dimensions.
        let quarter = matches!(raw.transform_code, 1 | 3 | 5 | 7);
        let (mw, mh) = if quarter { (raw.mode_h, raw.mode_w) } else { (raw.mode_w, raw.mode_h) };
        let (lw, lh) = raw.logical_size.unwrap_or(((mw as f64 / scale) as i32, (mh as f64 / scale) as i32));
        // Effective scale from physical/logical when available (fractional scaling).
        let phys_for_lw = if quarter { raw.mode_h } else { raw.mode_w };
        let eff_scale = if lw > 0 && phys_for_lw > 0 { phys_for_lw as f64 / lw as f64 } else { scale };
        outputs.push(OutputInfo { name, logical: LogicalRect { x: lx, y: ly, width: lw, height: lh },
            physical_width: raw.mode_w.max(0) as u32, physical_height: raw.mode_h.max(0) as u32,
            scale: eff_scale, transform: raw.transform.clone(), make: raw.make.clone(), model: raw.model.clone(), transform_code: raw.transform_code });
    }
    Ok(Discovery { globals: global_list, outputs, output_names: state.order.clone() })
}

// ---------- screencopy ----------
#[derive(Default)]
struct CapState { buffer_info: Option<(u32, u32, u32, u32)>, ready: bool, failed: bool, y_invert: bool }
impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for CapState {
    fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1, ()> for CapState {
    fn event(state: &mut Self, _: &zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1, event: zwlr_screencopy_frame_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            zwlr_screencopy_frame_v1::Event::Buffer { format, width, height, stride } => {
                if let wayland_client::WEnum::Value(f) = format { state.buffer_info = Some((f as u32, width, height, stride)); }
            }
            zwlr_screencopy_frame_v1::Event::Flags { flags } => {
                if std::env::var("WAYLAND_MCP_DEBUG_CAPTURE").is_ok() { eprintln!("screencopy flags event: {flags:?}"); }
                if let wayland_client::WEnum::Value(f) = flags { state.y_invert = f.contains(zwlr_screencopy_frame_v1::Flags::YInvert); }
            }
            zwlr_screencopy_frame_v1::Event::Ready { .. } => state.ready = true,
            zwlr_screencopy_frame_v1::Event::Failed => state.failed = true,
            _ => {}
        }
    }
}
wayland_client::delegate_noop!(CapState: ignore wl_output::WlOutput);
wayland_client::delegate_noop!(CapState: ignore wl_shm::WlShm);
wayland_client::delegate_noop!(CapState: ignore wl_shm_pool::WlShmPool);
wayland_client::delegate_noop!(CapState: ignore wayland_client::protocol::wl_buffer::WlBuffer);
wayland_client::delegate_noop!(CapState: ignore zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1);

pub struct Captured { pub rgba: Vec<u8>, pub width: u32, pub height: u32, pub output: OutputInfo, pub cursor_composited: bool }

pub fn capture_output(spec: &TargetSpec, output_name: Option<&str>) -> Result<Captured, String> { capture_output_cursor(spec, output_name, false) }
pub fn capture_output_cursor(spec: &TargetSpec, output_name: Option<&str>, cursor: bool) -> Result<Captured, String> {
    let disc = discover(spec)?;
    let output = match output_name {
        Some(n) => disc.outputs.iter().find(|o| o.name == n).cloned().ok_or_else(|| format!("output {n} not found; available: {:?}", disc.outputs.iter().map(|o| &o.name).collect::<Vec<_>>()))?,
        None => disc.outputs.first().cloned().ok_or("no outputs reported by compositor")?,
    };
    let conn = connect(spec)?;
    let (globals, mut queue) = registry_queue_init::<CapState>(&conn).map_err(|e| format!("registry: {e}"))?;
    let qh = queue.handle();
    let mut state = CapState::default();
    // Bind the exact wl_output global carried from discovery (by registry name),
    // not by position across two connections (hotplug could reorder them).
    let idx = disc.outputs.iter().position(|o| o.name == output.name).ok_or("discovered output missing")?;
    let out_global = *disc.output_names.get(idx).ok_or("wl_output global name missing for output")?;
    let advertised = globals.contents().clone_list().iter().find(|g| g.name == out_global).map(|g| g.version).unwrap_or(1);
    let wl_out: wl_output::WlOutput = globals.registry().bind(out_global, advertised.min(4), &qh, ());
    let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).map_err(|_| "wl_shm not advertised; screencopy capture unsupported on this target".to_string())?;
    let mgr: zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1 = globals.bind(&qh, 1..=3, ())
        .map_err(|_| "zwlr_screencopy_manager_v1 not advertised; native capture unsupported on this target (portal capture is a separate, consented path)".to_string())?;
    let frame = mgr.capture_output(if cursor { 1 } else { 0 }, &wl_out, &qh, ());
    queue.roundtrip(&mut state).map_err(|e| format!("capture roundtrip: {e}"))?;
    let (format, width, height, stride) = state.buffer_info.ok_or("compositor sent no screencopy buffer info (failed or unsupported)")?;
    if state.failed { return Err("screencopy frame failed".into()); }
    let size = (stride as usize).checked_mul(height as usize).ok_or("capture size overflow")?;
    if size > 512 * 1024 * 1024 { return Err(format!("capture too large: {size} bytes")); }
    // memfd-backed shm
    let fd = unsafe { libc::memfd_create(b"wayland-mcp-capture\0".as_ptr() as *const libc::c_char, libc::MFD_CLOEXEC) };
    if fd < 0 { return Err(format!("memfd_create: {}", std::io::Error::last_os_error())); }
    use std::os::fd::FromRawFd;
    // Wrap immediately so every error path below closes the fd.
    let file = unsafe { std::fs::File::from_raw_fd(fd) };
    if unsafe { libc::ftruncate(fd, size as libc::off_t) } != 0 { return Err("ftruncate capture shm failed".into()); }
    use std::os::fd::AsFd;
    let pool = shm.create_pool(file.as_fd(), size as i32, &qh, ());
    let fmt = wl_shm::Format::try_from(format).unwrap_or(wl_shm::Format::Argb8888);
    let buffer = pool.create_buffer(0, width as i32, height as i32, stride as i32, fmt, &qh, ());
    frame.copy(&buffer);
    for _ in 0..100 {
        queue.roundtrip(&mut state).map_err(|e| format!("frame roundtrip: {e}"))?;
        if state.ready || state.failed { break; }
    }
    if state.failed { return Err("screencopy copy failed".into()); }
    if !state.ready { return Err("screencopy timed out waiting for ready".into()); }
    // mmap read-only
    let ptr = unsafe { libc::mmap(std::ptr::null_mut(), size, libc::PROT_READ, libc::MAP_SHARED, fd, 0) };
    if ptr == libc::MAP_FAILED { return Err("mmap capture failed".into()); }
    let raw = unsafe { std::slice::from_raw_parts(ptr as *const u8, size).to_vec() };
    unsafe { libc::munmap(ptr, size) };
    drop(file);
    let mut rgba = convert_to_rgba(&raw, width, height, stride, format)?;
    // A y_invert frame flag (not set by niri, verified flags=0x0 there) means the
    // buffer is bottom-to-top; flip it unless the output transform handling
    // below already accounts for the orientation (Flipped180 case).
    if state.y_invert && output.transform_code != 6 {
        let row = (width * 4) as usize;
        let mut flipped = vec![0u8; rgba.len()];
        for y in 0..height as usize { flipped[y * row..(y + 1) * row].copy_from_slice(&rgba[(height as usize - 1 - y) * row..(height as usize - y) * row]); }
        rgba = flipped;
    }
    let (rgba, width, height) = apply_output_transform(rgba, width, height, output.transform_code);
    Ok(Captured { rgba, width, height, output, cursor_composited: cursor })
}

/// Apply the wl_output transform so returned pixels are in upright logical
/// orientation. Verified against grim + fixture OCR: Normal outputs need no
/// correction; Flipped180 (nested-niri winit) buffers already carry the
/// horizontal flip from the compositor's rendering, so only a vertical flip
/// restores upright orientation. Quarter-turns and plain 180 follow wl_output
/// transform semantics. Live-verified on niri/virtio-gpu (real output): codes 0-4
/// and 6 read upright; flipped quarter-turns (5, 7) compose as flip∘rotate
/// (rotate first, then flip_h) — the reverse order comes out 180° off.
pub fn apply_output_transform(rgba: Vec<u8>, w: u32, h: u32, code: i32) -> (Vec<u8>, u32, u32) {
    fn flip_h(p: &[u8], w: u32, h: u32) -> Vec<u8> { let mut o = vec![0u8; p.len()]; for y in 0..h { for x in 0..w { let s = ((y * w + x) * 4) as usize; let d = ((y * w + (w - 1 - x)) * 4) as usize; o[d..d + 4].copy_from_slice(&p[s..s + 4]); } } o }
    fn flip_v(p: &[u8], w: u32, h: u32) -> Vec<u8> { let mut o = vec![0u8; p.len()]; let row = (w * 4) as usize; for y in 0..h as usize { o[y * row..(y + 1) * row].copy_from_slice(&p[(h as usize - 1 - y) * row..(h as usize - y) * row]); } o }
    fn rot90cw(p: &[u8], w: u32, h: u32) -> (Vec<u8>, u32, u32) { let mut o = vec![0u8; p.len()]; for y in 0..h { for x in 0..w { let s = ((y * w + x) * 4) as usize; let d = ((x * h + (h - 1 - y)) * 4) as usize; o[d..d + 4].copy_from_slice(&p[s..s + 4]); } } (o, h, w) }
    match code {
        0 => (rgba, w, h),
        6 => (flip_v(&rgba, w, h), w, h),
        2 => (flip_h(&flip_v(&rgba, w, h), w, h), w, h),
        4 => (flip_h(&rgba, w, h), w, h),
        1 => rot90cw(&rgba, w, h),
        3 => { let (p, nw, nh) = rot90cw(&rgba, w, h); let (p, nw, nh) = rot90cw(&p, nw, nh); rot90cw(&p, nw, nh) }
        5 => { let (p, nw, nh) = rot90cw(&rgba, w, h); (flip_h(&p, nw, nh), nw, nh) }
        7 => { let (p, nw, nh) = rot90cw(&rgba, w, h); let (p, nw, nh) = rot90cw(&p, nw, nh); let (p, nw, nh) = rot90cw(&p, nw, nh); (flip_h(&p, nw, nh), nw, nh) }
        _ => (rgba, w, h),
    }
}

fn convert_to_rgba(raw: &[u8], w: u32, h: u32, stride: u32, format: u32) -> Result<Vec<u8>, String> {
    // wl_shm format values: Argb8888=0, Xrgb8888=1, Abgr8888=..., Xbgr8888, Rgb888=..., Bgr888
    let mut out = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        let row = (y * stride) as usize;
        for x in 0..w {
            let i = row + (x * 4) as usize;
            match format {
                0 | 1 => { // ARGB/XRGB little-endian bytes: B,G,R,A
                    out.extend_from_slice(&[raw.get(i + 2).copied().unwrap_or(0), raw.get(i + 1).copied().unwrap_or(0), raw.get(i).copied().unwrap_or(0), if format == 0 { raw.get(i + 3).copied().unwrap_or(255) } else { 255 }]);
                }
                0x34324241 | 0x34324258 => { // wl_shm ABGR8888 ('AB24') / XBGR8888 ('XB24'): bytes R,G,B,A
                    out.extend_from_slice(&[raw.get(i).copied().unwrap_or(0), raw.get(i + 1).copied().unwrap_or(0), raw.get(i + 2).copied().unwrap_or(0), if format == 0x34324241 { raw.get(i + 3).copied().unwrap_or(255) } else { 255 }]);
                }
                _ => return Err(format!("unsupported shm format 0x{format:08x} from compositor")),
            }
        }
    }
    Ok(out)
}

pub type Shared<T> = Arc<Mutex<T>>;
