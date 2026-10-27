//! In-process layer-shell activity indicator: click-through accent border,
//! last-action ripple and label on an overlay surface. It never requests keyboard
//! focus and never shows typed text (labels are action names only). Capture paths
//! hide it and wait for a compositor roundtrip acknowledgement before reading
//! pixels — not a fixed sleep. Without layer-shell it reports unavailable.
use crate::wayland::{connect, TargetSpec};
use std::sync::mpsc;
use wayland_client::{globals::{registry_queue_init, GlobalListContents}, protocol::{wl_compositor, wl_output, wl_registry, wl_shm, wl_shm_pool, wl_surface}, Connection, Dispatch, QueueHandle};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

/// Layer-surface configure handling is deliberately deferred: a configure is
/// recorded as `pending_configure` and only acked (see `ack_pending`) on the
/// code path that is about to commit this surface, immediately before that
/// commit. Attaching a buffer sized for a configure that has not been acked
/// yet is a `zwlr_layer_surface_v1` protocol error (KDE: "a buffer has been
/// attached to a layer surface prior to the first layer_surface.configure
/// event", and the equivalent for a resize configure).
#[derive(Default)]
struct IState {
    /// Latest configure received but not yet acked, as (serial, width, height).
    /// A newer configure supersedes an unacked older one; only the size that
    /// is actually committed needs its serial acked.
    pending_configure: Option<(u32, u32, u32)>,
    /// Size from the most recently *acked* configure. A buffer may only be
    /// attached at exactly this size, and only after the ack was sent.
    configured_size: Option<(u32, u32)>,
    closed: bool,
}
impl IState {
    /// Ack the pending configure, if any, adopting its size. Must be called
    /// after the configure was received and before any commit that attaches a
    /// buffer (or detaches one) in response to it.
    fn ack_pending(&mut self, layer: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1) {
        if let Some((serial, width, height)) = self.pending_configure.take() {
            layer.ack_configure(serial);
            self.configured_size = Some((width, height));
        }
    }
}
impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for IState {
    fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, ()> for IState {
    fn event(state: &mut Self, _surface: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, event: zwlr_layer_surface_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            // Do NOT ack here: the ack is deferred to `ack_pending`, called
            // just before the commit that uses this size (see IState docs).
            zwlr_layer_surface_v1::Event::Configure { serial, width, height } => { state.pending_configure = Some((serial, width, height)); }
            zwlr_layer_surface_v1::Event::Closed => state.closed = true,
            _ => {}
        }
    }
}
wayland_client::delegate_noop!(IState: ignore wl_compositor::WlCompositor);
wayland_client::delegate_noop!(IState: ignore wl_output::WlOutput);
wayland_client::delegate_noop!(IState: ignore wl_shm::WlShm);
wayland_client::delegate_noop!(IState: ignore wl_shm_pool::WlShmPool);
wayland_client::delegate_noop!(IState: ignore wayland_client::protocol::wl_buffer::WlBuffer);
wayland_client::delegate_noop!(IState: ignore wl_surface::WlSurface);
wayland_client::delegate_noop!(IState: ignore wayland_client::protocol::wl_region::WlRegion);
wayland_client::delegate_noop!(IState: ignore zwlr_layer_shell_v1::ZwlrLayerShellV1);

pub enum Cmd {
    Show { label: String, x: Option<f64>, y: Option<f64> },
    Hide { reply: mpsc::Sender<()> },
    Shutdown,
}

#[derive(Clone)]
pub struct IndicatorHandle { tx: mpsc::Sender<Cmd>, pub target_id: String }
impl IndicatorHandle {
    pub fn start(spec: &TargetSpec) -> Result<Self, String> {
        // Fail fast if layer-shell is not advertised (e.g. GNOME without an extension).
        let disc = crate::wayland::discover(spec)?;
        if !disc.globals.iter().any(|(_, i, _)| i == "zwlr_layer_shell_v1") {
            return Err("activity indicator unavailable: zwlr_layer_shell_v1 not advertised on this target (e.g. GNOME without a shell extension, which this server will not install)".into());
        }
        let (tx, rx) = mpsc::channel();
        let spec2 = spec.clone();
        std::thread::Builder::new().name(format!("indicator-{}", spec.id)).spawn(move || {
            if let Err(e) = run(spec2, rx) { eprintln!("indicator thread exited: {e}"); }
        }).map_err(|e| e.to_string())?;
        Ok(Self { tx, target_id: spec.id.clone() })
    }
    pub fn show(&self, label: &str, x: Option<f64>, y: Option<f64>) { let _ = self.tx.send(Cmd::Show { label: label.to_string(), x, y }); }
    pub fn hide_ack(&self) {
        let (tx, rx) = mpsc::channel();
        if self.tx.send(Cmd::Hide { reply: tx }).is_ok() { let _ = rx.recv_timeout(std::time::Duration::from_secs(2)); }
    }
    pub fn shutdown(&self) { let _ = self.tx.send(Cmd::Shutdown); }
}

fn run(spec: TargetSpec, rx: mpsc::Receiver<Cmd>) -> Result<(), String> {
    let conn = connect(&spec)?;
    let (globals, mut queue) = registry_queue_init::<IState>(&conn).map_err(|e| e.to_string())?;
    let qh = queue.handle();
    let mut state = IState::default();
    let disc = crate::wayland::discover(&spec)?;
    let output_info = disc.outputs.first().cloned().ok_or("no output for indicator")?;
    let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 4..=6, ()).map_err(|_| "wl_compositor missing".to_string())?;
    let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).map_err(|_| "wl_shm missing".to_string())?;
    let shell: zwlr_layer_shell_v1::ZwlrLayerShellV1 = globals.bind(&qh, 1..=4, ()).map_err(|_| "layer shell missing".to_string())?;
    let out_global = globals.contents().clone_list().iter().find(|g| g.interface == "wl_output").map(|g| g.name).ok_or("wl_output missing")?;
    let output: wl_output::WlOutput = globals.registry().bind(out_global, 1, &qh, ());
    let surface = compositor.create_surface(&qh, ());
    let layer = shell.get_layer_surface(&surface, Some(&output), zwlr_layer_shell_v1::Layer::Overlay, "wayland-mcp-indicator".into(), &qh, ());
    layer.set_anchor(zwlr_layer_surface_v1::Anchor::Top | zwlr_layer_surface_v1::Anchor::Bottom | zwlr_layer_surface_v1::Anchor::Left | zwlr_layer_surface_v1::Anchor::Right);
    layer.set_exclusive_zone(-1);
    layer.set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::None);
    // Click/touch-through: an explicitly EMPTY input region. A NULL region would
    // reset to the default (infinite) region and swallow pointer/touch input.
    let empty_region = compositor.create_region(&qh, ());
    surface.set_input_region(Some(&empty_region));
    empty_region.destroy();
    layer.set_size(0, 0);
    // Initial commit with NO buffer attached: this is the one commit the
    // protocol requires before the first configure, and it must not carry a
    // buffer. The compositor replies with a configure (recorded as pending,
    // not yet acked); no buffer is attached until a Show acks it.
    surface.commit();
    queue.roundtrip(&mut state).map_err(|e| format!("indicator initial roundtrip failed: {e}"))?;
    if state.closed { return Err("indicator unavailable: layer surface closed by the compositor before its first configure".into()); }
    let mut visible = false;
    // One shm pool + two buffers per configured size, reused across Shows and
    // destroyed on resize/shutdown. (Previously every Show leaked a pool+buffer
    // in the compositor.)
    let mut buffers: Option<Buffers> = None;
    while let Ok(cmd) = rx.recv() {
        match cmd {
            Cmd::Shutdown => { if let Some(b) = buffers.take() { b.destroy(); } surface.destroy(); return Ok(()); }
            Cmd::Hide { reply } => {
                if state.closed {
                    if let Some(b) = buffers.take() { b.destroy(); }
                    // Dropping `reply` unblocks hide_ack's caller at once.
                    return Err("indicator unavailable: layer surface closed by the compositor".into());
                }
                // A pending (e.g. resize) configure must be acked before this
                // commit too: any commit following a configure whose ack has
                // not been sent risks a protocol error.
                state.ack_pending(&layer);
                // Hide = commit a fully transparent buffer, NEVER unmap. Attaching
                // a null buffer unmaps the layer surface, returning it to its
                // pre-configure state: the next buffer attach is then a protocol
                // error on KDE ("a buffer has been attached to a layer surface
                // prior to the first layer_surface.configure event") because no
                // new configure is sent after a remap. A mapped transparent
                // surface with an empty input region is invisible to capture
                // and to input.
                let mut replaced: Option<Buffers> = None;
                if visible {
                    if let Some((w, h)) = state.configured_size.filter(|&(w, h)| w > 0 && h > 0) {
                        if buffers.as_ref().map(|b| (b.w, b.h)) != Some((w, h)) {
                            replaced = buffers.take();
                            match Buffers::new(&shm, &qh, w, h) {
                                Ok(b) => buffers = Some(b),
                                Err(e) => { eprintln!("indicator hide: {e}"); buffers = replaced.take(); }
                            }
                        }
                    }
                    if let Some(b) = buffers.as_mut() {
                        let (bw, bh) = (b.w as i32, b.h as i32);
                        let blank = b.blank_and_pick().clone();
                        surface.attach(Some(&blank), 0, 0);
                        surface.damage_buffer(0, 0, bw, bh);
                        surface.commit();
                    }
                    visible = false;
                }
                // Roundtrip is the acknowledgement that the hide was processed.
                // A failed roundtrip (protocol error / dead surface) is fatal
                // for the indicator, not a successful hide: exit the thread so
                // the failure is logged instead of capturing stale pixels, and
                // drop `reply` so hide_ack does not wait for its timeout.
                match queue.roundtrip(&mut state) {
                    Ok(_) if !state.closed => { if let Some(b) = replaced.take() { b.destroy(); } let _ = reply.send(()); }
                    Ok(_) => { if let Some(b) = buffers.take() { b.destroy(); } if let Some(b) = replaced.take() { b.destroy(); } return Err("indicator unavailable: layer surface closed by the compositor".into()); }
                    Err(e) => { if let Some(b) = buffers.take() { b.destroy(); } if let Some(b) = replaced.take() { b.destroy(); } return Err(format!("indicator unavailable: hide roundtrip failed: {e}")); }
                }
            }
            Cmd::Show { label, x, y } => {
                if state.closed {
                    if let Some(b) = buffers.take() { b.destroy(); }
                    return Err("indicator unavailable: layer surface closed by the compositor".into());
                }
                // First Show ever: if no configure has been received yet, pump
                // the queue (bounded) until one arrives. NEVER fall back to a
                // guessed size: attaching any buffer before the first
                // configure has been received and acked is the exact protocol
                // error KDE aborts on. If no usable configure arrives, skip
                // this Show (buffers stay untouched) and try again next Show.
                for _ in 0..3 {
                    if state.pending_configure.is_some() || state.configured_size.is_some() || state.closed { break; }
                    if let Err(e) = queue.roundtrip(&mut state) {
                        if let Some(b) = buffers.take() { b.destroy(); }
                        return Err(format!("indicator unavailable: configure roundtrip failed: {e}"));
                    }
                }
                if state.closed {
                    if let Some(b) = buffers.take() { b.destroy(); }
                    return Err("indicator unavailable: layer surface closed by the compositor".into());
                }
                // Ack BEFORE creating/attaching any buffer for this size and
                // before the commit below. On a resize this adopts the new
                // size, so the differently-sized buffer attached below is
                // always preceded by the ack of its configure. With no pending
                // configure this is a no-op and the already-configured size is
                // reused unchanged (buffers below are then reused as-is).
                state.ack_pending(&layer);
                let Some((w, h)) = state.configured_size.filter(|&(w, h)| w > 0 && h > 0) else {
                    eprintln!("indicator show skipped: no usable layer-surface configure received yet");
                    continue;
                };
                // On resize, keep the old buffers alive until the new one is
                // attached (the surface may still reference the old buffer).
                let mut replaced: Option<Buffers> = None;
                if buffers.as_ref().map(|b| (b.w, b.h)) != Some((w, h)) {
                    replaced = buffers.take();
                    match Buffers::new(&shm, &qh, w, h) {
                        Ok(b) => buffers = Some(b),
                        Err(e) => { eprintln!("indicator draw: {e}"); buffers = replaced.take(); continue; }
                    }
                }
                let b = buffers.as_mut().unwrap();
                let buffer = b.draw_and_pick(&label, x, y, &output_info).clone();
                surface.attach(Some(&buffer), 0, 0);
                surface.damage_buffer(0, 0, w as i32, h as i32);
                surface.commit();
                match queue.roundtrip(&mut state) {
                    Ok(_) if !state.closed => {
                        if let Some(b) = replaced.take() { b.destroy(); }
                        visible = true;
                    }
                    Ok(_) => { if let Some(b) = buffers.take() { b.destroy(); } if let Some(b) = replaced.take() { b.destroy(); } return Err("indicator unavailable: layer surface closed by the compositor".into()); }
                    Err(e) => { if let Some(b) = buffers.take() { b.destroy(); } if let Some(b) = replaced.take() { b.destroy(); } return Err(format!("indicator unavailable: show roundtrip failed: {e}")); }
                }
                // Note: a configure received during that roundtrip is left
                // pending (unacked) and no buffer is attached until the next
                // Show/Hide acks it via ack_pending above.
            }
        }
    }
    if let Some(b) = buffers.take() { b.destroy(); }
    Ok(())
}

/// Reusable double-buffered shm pool for one output size.
struct Buffers {
    pool: wl_shm_pool::WlShmPool,
    buffers: Vec<wayland_client::protocol::wl_buffer::WlBuffer>,
    ptr: *mut u8,
    total: usize,
    per: usize,
    w: u32,
    h: u32,
    next: usize,
    _file: std::fs::File,
}
impl Buffers {
    fn new(shm: &wl_shm::WlShm, qh: &QueueHandle<IState>, w: u32, h: u32) -> Result<Self, String> {
        let stride = w * 4;
        let per = (stride * h) as usize;
        let total = per * 2;
        let fd = unsafe { libc::memfd_create(b"wayland-mcp-indicator\0".as_ptr() as *const libc::c_char, libc::MFD_CLOEXEC) };
        if fd < 0 { return Err("memfd indicator failed".into()); }
        use std::os::fd::FromRawFd;
        // Wrap immediately so error paths close the fd.
        let file = unsafe { std::fs::File::from_raw_fd(fd) };
        if unsafe { libc::ftruncate(fd, total as libc::off_t) } != 0 { return Err("ftruncate indicator failed".into()); }
        let ptr = unsafe { libc::mmap(std::ptr::null_mut(), total, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0) };
        if ptr == libc::MAP_FAILED { return Err("mmap indicator failed".into()); }
        use std::os::fd::AsFd;
        let pool = shm.create_pool(file.as_fd(), total as i32, qh, ());
        let buffers = (0..2).map(|i| pool.create_buffer((i * per) as i32, w as i32, h as i32, stride as i32, wl_shm::Format::Argb8888, qh, ())).collect();
        Ok(Self { pool, buffers, ptr: ptr as *mut u8, total, per, w, h, next: 0, _file: file })
    }
    /// Clear the next buffer to fully transparent and return it (the hidden state; the surface stays mapped).
    fn blank_and_pick(&mut self) -> &wayland_client::protocol::wl_buffer::WlBuffer {
        let idx = self.next;
        self.next = 1 - self.next;
        unsafe { std::slice::from_raw_parts_mut(self.ptr.add(idx * self.per), self.per) }.fill(0);
        &self.buffers[idx]
    }
    fn draw_and_pick(&mut self, label: &str, x: Option<f64>, y: Option<f64>, output: &crate::coords::OutputInfo) -> &wayland_client::protocol::wl_buffer::WlBuffer {
        let idx = self.next;
        self.next = 1 - self.next;
        let pixels = unsafe { std::slice::from_raw_parts_mut(self.ptr.add(idx * self.per), self.per) };
        pixels.fill(0);
        draw_pixels(pixels, self.w, self.h, label, x, y, output);
        &self.buffers[idx]
    }
    fn destroy(self) {
        for b in &self.buffers { b.destroy(); }
        self.pool.destroy();
        unsafe { libc::munmap(self.ptr as *mut _, self.total) };
    }
}

fn draw_pixels(pixels: &mut [u8], w: u32, h: u32, label: &str, x: Option<f64>, y: Option<f64>, output: &crate::coords::OutputInfo) {
    let stride = w * 4;
    // ARGB8888 little-endian bytes: B,G,R,A. Accent: orange-red.
    let set = |pixels: &mut [u8], px: u32, py: u32, r: u8, g: u8, b: u8, a: u8| {
        if px < w && py < h { let i = (py * stride + px * 4) as usize; pixels[i] = b; pixels[i + 1] = g; pixels[i + 2] = r; pixels[i + 3] = a; }
    };
    let border = 8u32;
    for px in 0..w { for t in 0..border { set(pixels, px, t, 255, 90, 0, 200); set(pixels, px, h - 1 - t, 255, 90, 0, 200); } }
    for py in 0..h { for t in 0..border { set(pixels, t, py, 255, 90, 0, 200); set(pixels, w - 1 - t, py, 255, 90, 0, 200); } }
    if let (Some(lx), Some(ly)) = (x, y) {
        let cx = (lx - output.logical.x as f64) as i32; let cy = (ly - output.logical.y as f64) as i32;
        for dy in -16i32..=16 { for dx in -16i32..=16 {
            if dx * dx + dy * dy <= 256 && dx * dx + dy * dy >= 100 {
                set(pixels, (cx + dx).max(0) as u32, (cy + dy).max(0) as u32, 255, 90, 0, 230);
            }
        } }
    }
    draw_label(pixels, stride, w, h, label);
}

/// Tiny 5x7 uppercase label (action names only — never typed secrets).
/// Draw tiny 5x7 uppercase text (BGRA byte order) at a pixel position.
pub fn draw_text_at(pixels: &mut [u8], stride: u32, w: u32, h: u32, x0: u32, y0: u32, scale: u32, text: &str, rgba: [u8; 4]) {
    let mut cx = x0;
    for ch in text.to_uppercase().chars().take(24) {
        let glyph = glyph(ch);
        for (row, line) in glyph.iter().enumerate() {
            for (col, c) in line.chars().enumerate() {
                if c == '#' {
                    for dy in 0..scale { for dx in 0..scale {
                        let px = cx + col as u32 * scale + dx; let py = y0 + row as u32 * scale + dy;
                        if px < w && py < h { let i = (py * stride + px * 4) as usize; pixels[i] = rgba[2]; pixels[i + 1] = rgba[1]; pixels[i + 2] = rgba[0]; pixels[i + 3] = rgba[3]; }
                    } }
                }
            }
        }
        cx += 6 * scale;
    }
}

/// Tiny 5x7 uppercase label (action names only — never typed secrets).
fn draw_label(pixels: &mut [u8], stride: u32, w: u32, h: u32, label: &str) {
    let scale = 3u32;
    let mut cx = 20u32;
    let cy = 20u32;
    for ch in label.to_uppercase().chars().take(24) {
        let glyph = glyph(ch);
        for (row, line) in glyph.iter().enumerate() {
            for (col, c) in line.chars().enumerate() {
                if c == '#' {
                    for dy in 0..scale { for dx in 0..scale {
                        let px = cx + col as u32 * scale + dx; let py = cy + row as u32 * scale + dy;
                        if px < w && py < h { let i = (py * stride + px * 4) as usize; pixels[i] = 255; pixels[i + 1] = 255; pixels[i + 2] = 255; pixels[i + 3] = 255; }
                    } }
                }
            }
        }
        cx += 6 * scale;
    }
}
fn glyph(c: char) -> [&'static str; 7] {
    match c {
        'A' => [".###.", "#...#", "#...#", "#####", "#...#", "#...#", "#...#"],
        'B' => ["####.", "#...#", "#...#", "####.", "#...#", "#...#", "####."],
        'C' => [".####", "#....", "#....", "#....", "#....", "#....", ".####"],
        'D' => ["####.", "#...#", "#...#", "#...#", "#...#", "#...#", "####."],
        'E' => ["#####", "#....", "#....", "####.", "#....", "#....", "#####"],
        'F' => ["#####", "#....", "#....", "####.", "#....", "#....", "#...."],
        'G' => [".####", "#....", "#....", "#.###", "#...#", "#...#", ".###."],
        'H' => ["#...#", "#...#", "#...#", "#####", "#...#", "#...#", "#...#"],
        'I' => ["#####", "..#..", "..#..", "..#..", "..#..", "..#..", "#####"],
        'K' => ["#...#", "#..#.", "#.#..", "##...", "#.#..", "#..#.", "#...#"],
        'L' => ["#....", "#....", "#....", "#....", "#....", "#....", "#####"],
        'M' => ["#...#", "##.##", "#.#.#", "#.#.#", "#...#", "#...#", "#...#"],
        'N' => ["#...#", "##..#", "##..#", "#.#.#", "#..##", "#..##", "#...#"],
        'O' => [".###.", "#...#", "#...#", "#...#", "#...#", "#...#", ".###."],
        'P' => ["####.", "#...#", "#...#", "####.", "#....", "#....", "#...."],
        'R' => ["####.", "#...#", "#...#", "####.", "#.#..", "#..#.", "#...#"],
        'S' => [".####", "#....", "#....", ".###.", "....#", "....#", "####."],
        'T' => ["#####", "..#..", "..#..", "..#..", "..#..", "..#..", "..#.."],
        'U' => ["#...#", "#...#", "#...#", "#...#", "#...#", "#...#", ".###."],
        'V' => ["#...#", "#...#", "#...#", "#...#", "#...#", ".#.#.", "..#.."],
        'W' => ["#...#", "#...#", "#...#", "#.#.#", "#.#.#", "##.##", "#...#"],
        'Y' => ["#...#", "#...#", ".#.#.", "..#..", "..#..", "..#..", "..#.."],
        '0' => [".###.", "#..##", "#.#.#", "##..#", "#...#", "#...#", ".###."],
        '1' => ["..#..", ".##..", "..#..", "..#..", "..#..", "..#..", ".###."],
        '2' => [".###.", "#...#", "....#", "...#.", "..#..", ".#...", "#####"],
        '3' => ["####.", "....#", "....#", ".###.", "....#", "....#", "####."],
        '4' => ["...#.", "..##.", ".#.#.", "#####", "...#.", "...#.", "...#."],
        '5' => ["#####", "#....", "####.", "....#", "....#", "#...#", ".###."],
        '6' => [".###.", "#....", "#....", "####.", "#...#", "#...#", ".###."],
        '7' => ["#####", "....#", "...#.", "..#..", "..#..", "..#..", "..#.."],
        '8' => [".###.", "#...#", "#...#", ".###.", "#...#", "#...#", ".###."],
        '9' => [".###.", "#...#", "#...#", ".####", "....#", "....#", ".###."],
        '-' => [".....", ".....", ".....", "#####", ".....", ".....", "....."],
        _ => [".....", ".....", ".....", ".....", ".....", ".....", "....."],
    }
}
