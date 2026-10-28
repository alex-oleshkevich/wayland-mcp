//! In-process layer-shell activity indicator: click-through accent border,
//! last-action ripple and a history of the last actions (newest on top, count
//! set by `--indicator-history`, each expiring after `--indicator-ttl-ms`) on an overlay surface. It never requests
//! keyboard focus. Labels carry coordinates, buttons and named keys; `type_text`
//! shows the typed text itself (`Type: hello`, supplied as an explicit label so
//! it never enters result JSON, events or traces) and a bare printable key shows
//! as `*` (see `action_label`, `typed_label`). Capture paths hide it and
//! wait for a compositor roundtrip acknowledgement before reading pixels — not
//! a fixed sleep. Without layer-shell it reports unavailable.
use crate::wayland::{connect, TargetSpec};
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::mpsc;
use std::time::{Duration, Instant};
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
    // Newest first, each with the instant it was shown; kept across hides so the
    // next Show redraws the whole list. Entries expire `ttl` after being shown.
    let cfg = crate::config::get();
    let history_len = cfg.indicator_history.max(1);
    let ttl = Duration::from_millis(cfg.indicator_ttl_ms);
    let mut history: VecDeque<(String, Instant)> = VecDeque::with_capacity(history_len);
    // Position of the latest action, reused for ripple redraws on expiry.
    let mut last_xy: (Option<f64>, Option<f64>) = (None, None);
    // One shm pool + two buffers per configured size, reused across Shows and
    // destroyed on resize/shutdown. (Previously every Show leaked a pool+buffer
    // in the compositor.)
    let mut ov = Overlay { state, queue, qh, layer, surface, shm, output_info, buffers: None, visible: false };
    loop {
        // Sleep until the oldest entry expires; block indefinitely when empty.
        let cmd = match history.back() {
            None => match rx.recv() { Ok(c) => Some(c), Err(_) => break },
            Some((_, shown)) => match rx.recv_timeout((*shown + ttl).saturating_duration_since(Instant::now())) {
                Ok(c) => Some(c),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            },
        };
        let Some(cmd) = cmd else {
            // Timeout: drop expired entries from the back.
            expire(&mut history, ttl);
            if !ov.visible { continue; } // hidden for a capture: nothing on screen to update
            if history.is_empty() {
                // Everything expired: commit a blank buffer (ripple and border
                // go with it). No reply is owed.
                ov.blank_commit()?;
            } else {
                ov.draw_commit(&history, last_xy.0, last_xy.1)?;
            }
            continue;
        };
        match cmd {
            Cmd::Shutdown => { if let Some(b) = ov.buffers.take() { b.destroy(); } ov.surface.destroy(); return Ok(()); }
            Cmd::Hide { reply } => {
                ov.blank_commit()?; // an Err drops `reply`, unblocking hide_ack at once
                let _ = reply.send(());
            }
            Cmd::Show { label, x, y } => {
                expire(&mut history, ttl);
                history.push_front((label, Instant::now()));
                history.truncate(history_len);
                last_xy = (x, y);
                ov.draw_commit(&history, x, y)?;
            }
        }
    }
    if let Some(b) = ov.buffers.take() { b.destroy(); }
    Ok(())
}

/// Drop entries shown at least `ttl` ago (they sit at the back).
fn expire(history: &mut VecDeque<(String, Instant)>, ttl: Duration) {
    let now = Instant::now();
    while history.back().is_some_and(|(_, shown)| now.duration_since(*shown) >= ttl) { history.pop_back(); }
}

/// Surface, connection and buffer state owned by the indicator thread.
struct Overlay {
    state: IState,
    queue: wayland_client::EventQueue<IState>,
    qh: QueueHandle<IState>,
    layer: zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
    surface: wl_surface::WlSurface,
    shm: wl_shm::WlShm,
    output_info: crate::coords::OutputInfo,
    buffers: Option<Buffers>,
    visible: bool,
}
impl Overlay {
    fn closed_err(&mut self) -> String {
        if let Some(b) = self.buffers.take() { b.destroy(); }
        "indicator unavailable: layer surface closed by the compositor".into()
    }

    /// Render `history` (+ ripple at x/y) and commit it, then roundtrip.
    fn draw_commit(&mut self, history: &VecDeque<(String, Instant)>, x: Option<f64>, y: Option<f64>) -> Result<(), String> {
        if self.state.closed { return Err(self.closed_err()); }
        // First Show ever: if no configure has been received yet, pump
        // the queue (bounded) until one arrives. NEVER fall back to a
        // guessed size: attaching any buffer before the first
        // configure has been received and acked is the exact protocol
        // error KDE aborts on. If no usable configure arrives, skip
        // this Show (buffers stay untouched) and try again next Show.
        for _ in 0..3 {
            if self.state.pending_configure.is_some() || self.state.configured_size.is_some() || self.state.closed { break; }
            if let Err(e) = self.queue.roundtrip(&mut self.state) {
                if let Some(b) = self.buffers.take() { b.destroy(); }
                return Err(format!("indicator unavailable: configure roundtrip failed: {e}"));
            }
        }
        if self.state.closed { return Err(self.closed_err()); }
        // Ack BEFORE creating/attaching any buffer for this size and
        // before the commit below. On a resize this adopts the new
        // size, so the differently-sized buffer attached below is
        // always preceded by the ack of its configure. With no pending
        // configure this is a no-op and the already-configured size is
        // reused unchanged (buffers below are then reused as-is).
        self.state.ack_pending(&self.layer);
        let Some((w, h)) = self.state.configured_size.filter(|&(w, h)| w > 0 && h > 0) else {
            eprintln!("indicator show skipped: no usable layer-surface configure received yet");
            return Ok(());
        };
        // On resize, keep the old buffers alive until the new one is
        // attached (the surface may still reference the old buffer).
        let mut replaced: Option<Buffers> = None;
        if self.buffers.as_ref().map(|b| (b.w, b.h)) != Some((w, h)) {
            replaced = self.buffers.take();
            match Buffers::new(&self.shm, &self.qh, w, h) {
                Ok(b) => self.buffers = Some(b),
                Err(e) => { eprintln!("indicator draw: {e}"); self.buffers = replaced.take(); return Ok(()); }
            }
        }
        let b = self.buffers.as_mut().unwrap();
        let buffer = b.draw_and_pick(history, x, y, &self.output_info).clone();
        self.surface.attach(Some(&buffer), 0, 0);
        self.surface.damage_buffer(0, 0, w as i32, h as i32);
        self.surface.commit();
        match self.queue.roundtrip(&mut self.state) {
            Ok(_) if !self.state.closed => {
                if let Some(b) = replaced.take() { b.destroy(); }
                self.visible = true;
                // Note: a configure received during that roundtrip is left
                // pending (unacked) and no buffer is attached until the next
                // Show/Hide acks it via ack_pending above.
                Ok(())
            }
            Ok(_) => { if let Some(b) = replaced.take() { b.destroy(); } Err(self.closed_err()) }
            Err(e) => {
                if let Some(b) = self.buffers.take() { b.destroy(); }
                if let Some(b) = replaced.take() { b.destroy(); }
                Err(format!("indicator unavailable: show roundtrip failed: {e}"))
            }
        }
    }

    /// Make the overlay invisible (if shown) and roundtrip as the acknowledgement.
    fn blank_commit(&mut self) -> Result<(), String> {
        if self.state.closed { return Err(self.closed_err()); }
        // A pending (e.g. resize) configure must be acked before this
        // commit too: any commit following a configure whose ack has
        // not been sent risks a protocol error.
        self.state.ack_pending(&self.layer);
        // Hide = commit a fully transparent buffer, NEVER unmap. Attaching
        // a null buffer unmaps the layer surface, returning it to its
        // pre-configure state: the next buffer attach is then a protocol
        // error on KDE ("a buffer has been attached to a layer surface
        // prior to the first layer_surface.configure event") because no
        // new configure is sent after a remap. A mapped transparent
        // surface with an empty input region is invisible to capture
        // and to input.
        let mut replaced: Option<Buffers> = None;
        if self.visible {
            if let Some((w, h)) = self.state.configured_size.filter(|&(w, h)| w > 0 && h > 0) {
                if self.buffers.as_ref().map(|b| (b.w, b.h)) != Some((w, h)) {
                    replaced = self.buffers.take();
                    match Buffers::new(&self.shm, &self.qh, w, h) {
                        Ok(b) => self.buffers = Some(b),
                        Err(e) => { eprintln!("indicator hide: {e}"); self.buffers = replaced.take(); }
                    }
                }
            }
            if let Some(b) = self.buffers.as_mut() {
                let (bw, bh) = (b.w as i32, b.h as i32);
                let blank = b.blank_and_pick().clone();
                self.surface.attach(Some(&blank), 0, 0);
                self.surface.damage_buffer(0, 0, bw, bh);
                self.surface.commit();
            }
            self.visible = false;
        }
        // Roundtrip is the acknowledgement that the hide was processed.
        // A failed roundtrip (protocol error / dead surface) is fatal
        // for the indicator, not a successful hide: exit the thread so
        // the failure is logged instead of capturing stale pixels.
        match self.queue.roundtrip(&mut self.state) {
            Ok(_) if !self.state.closed => { if let Some(b) = replaced.take() { b.destroy(); } Ok(()) }
            Ok(_) => { if let Some(b) = replaced.take() { b.destroy(); } Err(self.closed_err()) }
            Err(e) => {
                if let Some(b) = self.buffers.take() { b.destroy(); }
                if let Some(b) = replaced.take() { b.destroy(); }
                Err(format!("indicator unavailable: hide roundtrip failed: {e}"))
            }
        }
    }
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
    fn draw_and_pick(&mut self, history: &VecDeque<(String, Instant)>, x: Option<f64>, y: Option<f64>, output: &crate::coords::OutputInfo) -> &wayland_client::protocol::wl_buffer::WlBuffer {
        let idx = self.next;
        self.next = 1 - self.next;
        let pixels = unsafe { std::slice::from_raw_parts_mut(self.ptr.add(idx * self.per), self.per) };
        pixels.fill(0);
        draw_pixels(pixels, self.w, self.h, history, x, y, output);
        &self.buffers[idx]
    }
    fn destroy(self) {
        for b in &self.buffers { b.destroy(); }
        self.pool.destroy();
        unsafe { libc::munmap(self.ptr as *mut _, self.total) };
    }
}

fn draw_pixels(pixels: &mut [u8], w: u32, h: u32, history: &VecDeque<(String, Instant)>, x: Option<f64>, y: Option<f64>, output: &crate::coords::OutputInfo) {
    let stride = w * 4;
    // ARGB8888 little-endian bytes: B,G,R,A. Accent: orange-red.
    let set = |pixels: &mut [u8], px: u32, py: u32, r: u8, g: u8, b: u8, a: u8| {
        if px < w && py < h { let i = (py * stride + px * 4) as usize; pixels[i] = b; pixels[i + 1] = g; pixels[i + 2] = r; pixels[i + 3] = a; }
    };
    let border = 8u32;
    for px in 0..w { for t in 0..border { set(pixels, px, t, 255, 90, 0, 200); set(pixels, px, h - 1 - t, 255, 90, 0, 200); } }
    for py in 0..h { for t in 0..border { set(pixels, t, py, 255, 90, 0, 200); set(pixels, w - 1 - t, py, 255, 90, 0, 200); } }
    draw_history_panel(pixels, stride, w, h, history);
    if let (Some(lx), Some(ly)) = (x, y) {
        let cx = (lx - output.logical.x as f64) as i32; let cy = (ly - output.logical.y as f64) as i32;
        for dy in -16i32..=16 { for dx in -16i32..=16 {
            if dx * dx + dy * dy <= 256 && dx * dx + dy * dy >= 100 {
                set(pixels, (cx + dx).max(0) as u32, (cy + dy).max(0) as u32, 255, 90, 0, 230);
            }
        } }
    }
    draw_history_text(pixels, stride, w, h, history);
}

/// Longest label drawn; longer ones are cut.
const MAX_LABEL_CHARS: usize = 40;
const LABEL_SCALE: u32 = 3;
const LINE_HEIGHT: u32 = 10 * LABEL_SCALE;
const PANEL_X: u32 = 16;
const PANEL_Y: u32 = 16;
const PANEL_PAD: u32 = 12;

/// Translucent dark panel behind the history so it reads on any background.
fn draw_history_panel(pixels: &mut [u8], stride: u32, w: u32, h: u32, history: &VecDeque<(String, Instant)>) {
    let Some(chars) = history.iter().map(|(l, _)| l.chars().count().min(MAX_LABEL_CHARS)).max() else { return };
    let pw = chars as u32 * 6 * LABEL_SCALE + 2 * PANEL_PAD;
    let ph = history.len() as u32 * LINE_HEIGHT + 2 * PANEL_PAD - 3 * LABEL_SCALE;
    for py in PANEL_Y..(PANEL_Y + ph).min(h) {
        for px in PANEL_X..(PANEL_X + pw).min(w) {
            let i = (py * stride + px * 4) as usize;
            // Premultiplied black.
            pixels[i..i + 4].copy_from_slice(&[0, 0, 0, 170]);
        }
    }
}

/// History lines, newest on top and brightest; older lines fade.
fn draw_history_text(pixels: &mut [u8], stride: u32, w: u32, h: u32, history: &VecDeque<(String, Instant)>) {
    for (i, (label, _)) in history.iter().enumerate() {
        let a = 255u32.saturating_sub(i as u32 * 40).max(90) as u8;
        let y = PANEL_Y + PANEL_PAD + i as u32 * LINE_HEIGHT;
        // Premultiplied white at alpha `a`.
        draw_text_at(pixels, stride, w, h, PANEL_X + PANEL_PAD, y, LABEL_SCALE, label, [a, a, a, a]);
    }
}

/// Draw tiny 5x7 uppercase text (BGRA byte order) at a pixel position.
pub fn draw_text_at(pixels: &mut [u8], stride: u32, w: u32, h: u32, x0: u32, y0: u32, scale: u32, text: &str, rgba: [u8; 4]) {
    let mut cx = x0;
    for ch in text.to_uppercase().chars().take(MAX_LABEL_CHARS) {
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

fn is_command_modifier(key: &str) -> bool {
    matches!(key.to_ascii_lowercase().as_str(), "ctrl" | "control" | "leftctrl" | "rightctrl" | "alt" | "leftalt" | "rightalt" | "super" | "meta" | "win" | "leftmeta" | "rightmeta")
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

/// OSD line for `type_text`, e.g. `Type: hello` / `Paste: hello`. The text is
/// passed straight to the overlay (never through the action JSON, so it stays
/// out of results, events and traces); control characters become spaces.
pub fn typed_label(paste: bool, text: &str) -> String {
    let text: String = text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    format!("{}: {text}", if paste { "Paste" } else { "Type" })
}

/// `Ctrl+S`, `Enter`, `Shift+*`. A single-character key is typed text unless a
/// ctrl/alt/super modifier makes the chord a shortcut, so it shows as `*`.
fn chord(keys: &Value) -> String {
    let keys: Vec<&str> = keys.as_array().map(|k| k.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    let shortcut = keys.iter().any(|k| is_command_modifier(k));
    keys.iter().map(|k| if k.chars().count() == 1 && !shortcut { "*".to_owned() } else { capitalize(k) }).collect::<Vec<_>>().join("+")
}

fn num(v: &Value) -> String {
    let n = v.as_f64().unwrap_or(0.0);
    if n.fract() == 0.0 { format!("{}", n as i64) } else { format!("{n:.1}") }
}

/// ` x:y` of a `[x, y]` pair, rounded to whole logical pixels; empty when absent.
fn at(v: &Value) -> String {
    match (v[0].as_f64(), v[1].as_f64()) {
        (Some(x), Some(y)) => format!(" {}:{}", x.round() as i64, y.round() as i64),
        _ => String::new(),
    }
}

/// One history line for a dispatched action result, e.g. `Move to 120:340`,
/// `Key: Enter`. `type_text` is labelled by `typed_label` instead; a bare printable key shows as `*`.
pub fn action_label(a: &Value) -> String {
    let name = a["action"].as_str().unwrap_or("action");
    match name {
        "move" => format!("Move to{}", at(&a["logical"])),
        "click" => {
            let mut s: String = a["modifiers"].as_array().into_iter().flatten().filter_map(Value::as_str).map(|m| format!("{}+", capitalize(m))).collect();
            let button = a["button"].as_str().unwrap_or("left");
            if button != "left" { s.push_str(&capitalize(button)); s.push(' '); }
            let kind = if a["long_press_ms"].as_u64().is_some() { "Long press" } else {
                match a["count"].as_u64().unwrap_or(1) { 2 => "Double click", 3 => "Triple click", _ => "Click" }
            };
            let kind = if s.is_empty() { kind.to_owned() } else { kind.to_lowercase() };
            s.push_str(&kind);
            format!("{s}{}", at(&a["logical"]))
        }
        "drag" => format!("Drag{} to{}", at(&a["from"]), at(&a["to"])),
        "scroll" => format!("Scroll {}:{}{}", num(&a["dx"]), num(&a["dy"]), at(&a["logical"])),
        "mouse_button" => format!("{} button {}", capitalize(a["button"].as_str().unwrap_or("mouse")), if a["pressed"].as_bool() == Some(true) { "down" } else { "up" }),
        "key" => {
            let repeat = a["repeat"].as_u64().unwrap_or(1);
            format!("Key: {}{}", chord(&a["keys"]), if repeat > 1 { format!(" x{repeat}") } else { String::new() })
        }
        "key_hold" => format!("{}: {}", if a["pressed"].as_bool() == Some(true) { "Hold" } else { "Release" }, chord(&a["keys"])),
        "touch_tap" => format!("Tap{}", at(&a["logical"])),
        "touch_swipe" => format!("Swipe {} points", a["points"].as_u64().unwrap_or(0)),
        "touch_pinch" => format!("Pinch{}", at(&a["center"])),
        other => capitalize(&other.replace('_', " ")),
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
        'J' => ["..###", "...#.", "...#.", "...#.", "...#.", "#..#.", ".##.."],
        'K' => ["#...#", "#..#.", "#.#..", "##...", "#.#..", "#..#.", "#...#"],
        'L' => ["#....", "#....", "#....", "#....", "#....", "#....", "#####"],
        'M' => ["#...#", "##.##", "#.#.#", "#.#.#", "#...#", "#...#", "#...#"],
        'N' => ["#...#", "##..#", "##..#", "#.#.#", "#..##", "#..##", "#...#"],
        'O' => [".###.", "#...#", "#...#", "#...#", "#...#", "#...#", ".###."],
        'P' => ["####.", "#...#", "#...#", "####.", "#....", "#....", "#...."],
        'Q' => [".###.", "#...#", "#...#", "#...#", "#.#.#", "#..#.", ".##.#"],
        'R' => ["####.", "#...#", "#...#", "####.", "#.#..", "#..#.", "#...#"],
        'S' => [".####", "#....", "#....", ".###.", "....#", "....#", "####."],
        'T' => ["#####", "..#..", "..#..", "..#..", "..#..", "..#..", "..#.."],
        'U' => ["#...#", "#...#", "#...#", "#...#", "#...#", "#...#", ".###."],
        'V' => ["#...#", "#...#", "#...#", "#...#", "#...#", ".#.#.", "..#.."],
        'W' => ["#...#", "#...#", "#...#", "#.#.#", "#.#.#", "##.##", "#...#"],
        'X' => ["#...#", "#...#", ".#.#.", "..#..", ".#.#.", "#...#", "#...#"],
        'Y' => ["#...#", "#...#", ".#.#.", "..#..", "..#..", "..#..", "..#.."],
        'Z' => ["#####", "....#", "...#.", "..#..", ".#...", "#....", "#####"],
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
        ':' => [".....", "..#..", "..#..", ".....", "..#..", "..#..", "....."],
        '+' => [".....", "..#..", "..#..", "#####", "..#..", "..#..", "....."],
        '*' => [".....", "#.#.#", ".###.", "#####", ".###.", "#.#.#", "....."],
        '.' => [".....", ".....", ".....", ".....", ".....", ".##..", ".##.."],
        ',' => [".....", ".....", ".....", ".....", ".##..", "..#..", ".#..."],
        '/' => [".....", "....#", "...#.", "..#..", ".#...", "#....", "....."],
        '(' => ["...#.", "..#..", ".#...", ".#...", ".#...", "..#..", "...#."],
        ')' => [".#...", "..#..", "...#.", "...#.", "...#.", "..#..", ".#..."],
        '\'' => ["..#..", "..#..", ".#...", ".....", ".....", ".....", "....."],
        '"' => [".#.#.", ".#.#.", ".#.#.", ".....", ".....", ".....", "....."],
        '!' => ["..#..", "..#..", "..#..", "..#..", "..#..", ".....", "..#.."],
        '?' => [".###.", "#...#", "....#", "...#.", "..#..", ".....", "..#.."],
        '_' => [".....", ".....", ".....", ".....", ".....", ".....", "#####"],
        '=' => [".....", ".....", "#####", ".....", "#####", ".....", "....."],
        '@' => [".###.", "#...#", "#.###", "#.#.#", "#.###", "#....", ".###."],
        '#' => [".#.#.", ".#.#.", "#####", ".#.#.", "#####", ".#.#.", ".#.#."],
        ';' => [".....", "..#..", "..#..", ".....", ".##..", "..#..", ".#..."],
        '<' => ["...#.", "..#..", ".#...", "#....", ".#...", "..#..", "...#."],
        '>' => [".#...", "..#..", "...#.", "....#", "...#.", "..#..", ".#..."],
        '[' => [".###.", ".#...", ".#...", ".#...", ".#...", ".#...", ".###."],
        ']' => [".###.", "...#.", "...#.", "...#.", "...#.", "...#.", ".###."],
        _ => [".....", ".....", ".....", ".....", ".....", ".....", "....."],
    }
}

#[cfg(test)]
mod tests {
    use super::{action_label, typed_label};
    use serde_json::json;

    #[test]
    fn labels_carry_the_action_values() {
        assert_eq!(action_label(&json!({"action": "move", "logical": [120.4, 339.6]})), "Move to 120:340");
        assert_eq!(action_label(&json!({"action": "key", "keys": ["Return"]})), "Key: Return");
        assert_eq!(action_label(&json!({"action": "key", "keys": ["ctrl", "s"], "repeat": 3})), "Key: Ctrl+S x3");
        assert_eq!(action_label(&json!({"action": "click", "logical": [10, 20], "button": "right", "count": 2, "modifiers": ["shift"]})), "Shift+Right double click 10:20");
        assert_eq!(action_label(&json!({"action": "drag", "from": [1, 2], "to": [3, 4]})), "Drag 1:2 to 3:4");
        assert_eq!(action_label(&json!({"action": "scroll", "dx": 0.0, "dy": 2.5})), "Scroll 0:2.5");
    }

    /// Typed text reaches the overlay only through `typed_label`; bare keys stay masked.
    #[test]
    fn typed_text_label_and_bare_key_masking() {
        assert_eq!(typed_label(false, "hello"), "Type: hello");
        assert_eq!(typed_label(true, "a\tb\nc"), "Paste: a b c");
        assert_eq!(action_label(&json!({"action": "key", "keys": ["p"]})), "Key: *");
        assert_eq!(action_label(&json!({"action": "key", "keys": ["shift", "p"]})), "Key: Shift+*");
        assert_eq!(action_label(&json!({"action": "key_hold", "keys": ["x"], "pressed": true})), "Hold: *");
    }
}
