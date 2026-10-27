//! Clipboard/selection via data-control: zwlr_data_control_manager_v1 (wlroots family, tried
//! first) or ext_data_control_manager_v1 (KWin and others), which have identical requests and
//! events. The owner thread holds the data source for writes — ownership outlives the tool
//! call until replaced or the target shuts down — and serves Send events off-thread.
//! Preservation limit: a clipboard is only promised restorable if every advertised MIME
//! payload fits within MAX_PRESERVE_BYTES total; otherwise callers must refuse before
//! overwriting. Targets with neither protocol (e.g. GNOME) report clipboard unsupported here;
//! portal clipboard requires a consented RemoteDesktop session and is never used as a
//! silent bypass after a denial.
use crate::wayland::TargetSpec;
use std::{sync::{atomic::{AtomicU64, Ordering}, mpsc, Arc}, time::{Duration, Instant}};

pub const MAX_PRESERVE_BYTES: usize = 16 * 1024 * 1024;
pub const READ_TIMEOUT: Duration = Duration::from_secs(3);

/// Instantiate the data-control backend for one protocol family. wlr and ext data-control have
/// identical manager/device/offer/source requests and events and differ only in type names.
macro_rules! data_control_backend {
    ($modname:ident, $name:literal, $ver:expr, $path:path, $dev:ident :: $Dev:ident, $off:ident :: $Off:ident, $src:ident :: $Src:ident, $mgr:ident :: $Mgr:ident) => {
pub(crate) mod $modname {
    use crate::clipboard::{Cmd, Stats, MAX_PRESERVE_BYTES, READ_TIMEOUT};
    use crate::wayland::{connect, TargetSpec};
    use std::{io::{Read, Write}, os::fd::{AsFd, AsRawFd, FromRawFd, IntoRawFd, OwnedFd}, sync::{atomic::Ordering, mpsc, Arc, Mutex}, time::Instant};
    use wayland_client::{globals::{registry_queue_init, GlobalListContents}, protocol::{wl_registry, wl_seat}, Connection, Dispatch, Proxy, QueueHandle};
    use $path::{$dev, $mgr, $off, $src};

#[derive(Clone, Default)]
struct OfferState { mimes: Arc<Mutex<Vec<String>>> }
#[derive(Default)]
struct DeviceState {
    selection: Option<($off::$Off, Arc<Mutex<Vec<String>>>)>,
    primary: Option<($off::$Off, Arc<Mutex<Vec<String>>>)>,
}
#[derive(Clone)]
struct SourceData { payloads: Arc<Mutex<Vec<(String, Vec<u8>)>>>, stats: Arc<Stats> }

struct ClipState { device: Arc<Mutex<DeviceState>>, stats: Arc<Stats>, source: Option<$src::$Src> }

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for ClipState {
    fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<$dev::$Dev, ()> for ClipState {
    fn event(state: &mut Self, _: &$dev::$Dev, event: $dev::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            $dev::Event::DataOffer { .. } => {}
            $dev::Event::Selection { id } => {
                let mut dev = state.device.lock().unwrap();
                let old = dev.selection.take();
                dev.selection = id.map(|offer| { let mimes = offer.data::<OfferState>().map(|s| s.mimes.clone()).unwrap_or_default(); (offer, mimes) });
                if let Some((old, _)) = old { old.destroy(); }
                state.stats.selections.fetch_add(1, Ordering::SeqCst);
            }
            $dev::Event::PrimarySelection { id } => {
                let mut dev = state.device.lock().unwrap();
                let old = dev.primary.take();
                dev.primary = id.map(|offer| { let mimes = offer.data::<OfferState>().map(|s| s.mimes.clone()).unwrap_or_default(); (offer, mimes) });
                if let Some((old, _)) = old { old.destroy(); }
            }
            _ => {}
        }
    }
    wayland_client::event_created_child!(ClipState, $dev::$Dev, [
        $dev::EVT_DATA_OFFER_OPCODE => ($off::$Off, OfferState::default()),
    ]);
}
impl Dispatch<$off::$Off, OfferState> for ClipState {
    fn event(_: &mut Self, _: &$off::$Off, event: $off::Event, data: &OfferState, _: &Connection, _: &QueueHandle<Self>) {
        if let $off::Event::Offer { mime_type } = event { data.mimes.lock().unwrap().push(mime_type); }
    }
}
impl Dispatch<$src::$Src, SourceData> for ClipState {
    fn event(state: &mut Self, src: &$src::$Src, event: $src::Event, data: &SourceData, _: &Connection, _: &QueueHandle<Self>) {
        match event {
        $src::Event::Send { mime_type, fd } => {
            data.stats.sends.fetch_add(1, Ordering::SeqCst);
            let payload = data.payloads.lock().unwrap().iter().find(|(m, _)| *m == mime_type).map(|(_, b)| b.clone());
            if let Some(bytes) = payload {
                // Serve off-thread: this thread also drains our own offers (read_impl), so a blocking
                // write of a payload beyond the pipe buffer would deadlock it or truncate the read.
                let fd = fd.into_raw_fd();
                let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
                let stats = data.stats.clone();
                std::thread::spawn(move || { let _ = file.write_all(&bytes); drop(file); stats.done.fetch_add(1, Ordering::SeqCst); });
            } else {
                data.stats.done.fetch_add(1, Ordering::SeqCst); // nothing to serve for this MIME; request is handled
            }
        }
        $src::Event::Cancelled => {
            // Spec: a cancelled source is no longer valid; client should destroy it.
            // Only destroy when it is still our stored source — a source replaced
            // by our own Write was already destroyed there (double-destroy avoided).
            if state.source.as_ref().map(|s| s.id()) == Some(src.id()) { state.source = None; src.destroy(); }
        }
        _ => {}
        }
    }
}
wayland_client::delegate_noop!(ClipState: ignore wl_seat::WlSeat);
wayland_client::delegate_noop!(ClipState: ignore $mgr::$Mgr);

pub(crate) fn run_thread(spec: &TargetSpec, rx: mpsc::Receiver<Cmd>, ready: &mpsc::Sender<Result<(), String>>, stats: Arc<Stats>) -> Result<(), String> {
    let conn = connect(spec)?;
    let (globals, mut queue) = registry_queue_init::<ClipState>(&conn).map_err(|e| format!("clipboard registry: {e}"))?;
    let qh = queue.handle();
    let device_state = Arc::new(Mutex::new(DeviceState::default()));
    let mut state = ClipState { device: device_state.clone(), stats: stats.clone(), source: None };
    let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=1, ()).map_err(|_| "wl_seat unavailable for clipboard".to_string())?;
    let mgr: $mgr::$Mgr = globals.bind(&qh, $ver, ())
        .map_err(|_| format!("{} not advertised", $name))?;
    let device = mgr.get_data_device(&seat, &qh, ());
    queue.roundtrip(&mut state).map_err(|e| e.to_string())?;
    let _ = ready.send(Ok(()));
    loop {
        queue.flush().map_err(|e| e.to_string())?;
        let fd = conn.as_fd().as_raw_fd();
        let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        let ready = unsafe { libc::poll(&mut pfd, 1, 30) };
        if ready > 0 {
            if let Some(guard) = queue.prepare_read() { let _ = guard.read(); }
            queue.dispatch_pending(&mut state).map_err(|e| e.to_string())?;
        }
        loop {
            let cmd = match rx.try_recv() {
                Ok(c) => c,
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
            };
            match cmd {
                Cmd::Shutdown => return Ok(()),
                Cmd::Inventory { primary, reply } => {
                    queue.roundtrip(&mut state).map_err(|e| e.to_string())?;
                    let dev = device_state.lock().unwrap();
                    let slot = if primary { &dev.primary } else { &dev.selection };
                    let res = match slot { Some((_, mimes)) => Ok(mimes.lock().unwrap().clone()), None => Ok(Vec::new()) };
                    let _ = reply.send(res);
                }
                Cmd::Read { mime, primary, reply } => {
                    let res = read_impl(&mut queue, &mut state, &device_state, mime, primary);
                    let _ = reply.send(res);
                }
                Cmd::Write { payloads, primary, reply } => {
                    // Empty write = true empty clipboard: clear selection, destroy old source.
                    // Never install a zero-MIME source.
                    if payloads.is_empty() {
                        if primary { device.set_primary_selection(None::<&$src::$Src>); } else { device.set_selection(None::<&$src::$Src>); }
                        if let Some(old) = state.source.take() { old.destroy(); }
                        let res = queue.flush().map_err(|e| e.to_string());
                        let _ = reply.send(res);
                        continue;
                    }
                    let data = SourceData { payloads: Arc::new(Mutex::new(payloads.clone())), stats: stats.clone() };
                    let src = mgr.create_data_source(&qh, data);
                    for (mime, _) in &payloads { src.offer(mime.clone()); }
                    if primary { device.set_primary_selection(Some(&src)); } else { device.set_selection(Some(&src)); }
                    // Destroy the replaced source so its payloads are not leaked for the process lifetime.
                    if let Some(old) = state.source.replace(src) { old.destroy(); }
                    let res = queue.flush().map_err(|e| e.to_string());
                    let _ = reply.send(res);
                }
            }
            queue.flush().map_err(|e| e.to_string())?;
            queue.dispatch_pending(&mut state).map_err(|e| e.to_string())?;
        }
    }
}

fn read_impl(queue: &mut wayland_client::EventQueue<ClipState>, state: &mut ClipState, device_state: &Arc<Mutex<DeviceState>>, mime: Option<String>, primary: bool) -> Result<(String, Vec<u8>), String> {
    queue.roundtrip(state).map_err(|e| e.to_string())?;
    let (offer, mimes) = {
        let dev = device_state.lock().unwrap();
        let slot = if primary { &dev.primary } else { &dev.selection };
        match slot { Some((o, m)) => (o.clone(), m.lock().unwrap().clone()), None => return Ok((String::new(), Vec::new())) }
    };
    if mimes.is_empty() { return Ok((String::new(), Vec::new())); }
    let chosen = match &mime {
        Some(m) => { if !mimes.contains(m) { return Err(format!("clipboard does not offer '{m}'; available: {mimes:?}")); } m.clone() }
        None => mimes.iter().find(|m| m.starts_with("text/plain")).cloned().unwrap_or_else(|| mimes[0].clone()),
    };
    // O_CLOEXEC only: pipe2 would set O_NONBLOCK on BOTH ends, and the write end is
    // handed to the selection owner, which would then see EAGAIN/short writes and
    // silently truncate. Set O_NONBLOCK on our read end only via fcntl.
    let mut fds = [0i32; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 { return Err("pipe2 for clipboard read failed".into()); }
    let write_fd = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    let read_fd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    unsafe {
        let fl = libc::fcntl(read_fd.as_raw_fd(), libc::F_GETFL);
        if fl < 0 || libc::fcntl(read_fd.as_raw_fd(), libc::F_SETFL, fl | libc::O_NONBLOCK) != 0 {
            return Err("fcntl O_NONBLOCK on clipboard read end failed".into());
        }
    }
    offer.receive(chosen.clone(), write_fd.as_fd());
    drop(write_fd); // our copy of the write end must close or EOF never arrives
    queue.flush().map_err(|e| e.to_string())?;
    let mut file = std::fs::File::from(read_fd);
    // Inactivity deadline: reset on every chunk of progress, so large payloads that
    // keep flowing (up to the safety cap) complete; only a stalled source times out.
    let mut last_progress = Instant::now();
    let mut buf = Vec::new();
    let mut chunk = [0u8; 65536];
    loop {
        if let Some(guard) = queue.prepare_read() { let _ = guard.read(); }
        queue.dispatch_pending(state).map_err(|e| e.to_string())?;
        match file.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => { buf.extend_from_slice(&chunk[..n]); last_progress = Instant::now(); if buf.len() > MAX_PRESERVE_BYTES * 2 { return Err("clipboard payload exceeds safety limit".into()); } }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if last_progress.elapsed() > READ_TIMEOUT { return Err(format!("clipboard read timed out: no data for {:?} (source app unresponsive)", READ_TIMEOUT)); }
                // Wait efficiently for readability instead of a fixed sleep-poll.
                let remaining = READ_TIMEOUT.saturating_sub(last_progress.elapsed());
                let wait_ms = remaining.as_millis().min(50) as i32;
                let mut pfd = libc::pollfd { fd: file.as_raw_fd(), events: libc::POLLIN, revents: 0 };
                unsafe { libc::poll(&mut pfd, 1, wait_ms) };
            }
            Err(e) => return Err(format!("clipboard read: {e}")),
        }
    }
    Ok((chosen, buf))
}
}
    };
}

data_control_backend!(wlr, "zwlr_data_control_manager_v1", 1..=2, wayland_protocols_wlr::data_control::v1::client,
    zwlr_data_control_device_v1::ZwlrDataControlDeviceV1, zwlr_data_control_offer_v1::ZwlrDataControlOfferV1,
    zwlr_data_control_source_v1::ZwlrDataControlSourceV1, zwlr_data_control_manager_v1::ZwlrDataControlManagerV1);
data_control_backend!(ext, "ext_data_control_manager_v1", 1..=1, wayland_protocols::ext::data_control::v1::client,
    ext_data_control_device_v1::ExtDataControlDeviceV1, ext_data_control_offer_v1::ExtDataControlOfferV1,
    ext_data_control_source_v1::ExtDataControlSourceV1, ext_data_control_manager_v1::ExtDataControlManagerV1);

pub enum Cmd {
    Inventory { primary: bool, reply: mpsc::Sender<Result<Vec<String>, String>> },
    Read { mime: Option<String>, primary: bool, reply: mpsc::Sender<Result<(String, Vec<u8>), String>> },
    Write { payloads: Vec<(String, Vec<u8>)>, primary: bool, reply: mpsc::Sender<Result<(), String>> },
    Shutdown,
}

/// Counters shared between the clipboard thread (and its transfer writers) and handle callers.
/// `sends`: transfer requests received for any source we own; `done`: requests fully handled
/// (payload written and write end closed); `selections`: selection-owner changes observed.
#[derive(Default)]
pub(crate) struct Stats { pub sends: AtomicU64, pub done: AtomicU64, pub selections: AtomicU64 }

enum Backend { Wlr, Ext }
/// Both backends report a missing manager global as "<interface> not advertised".
fn not_advertised(e: &str) -> bool { e.ends_with(" not advertised") }
#[derive(Clone)]
pub struct ClipboardHandle { inner: Inner, pub target_id: String }
#[derive(Clone)]
enum Inner { DataControl(mpsc::Sender<Cmd>, Arc<Stats>), Portal(crate::portal_session::PortalHandle) }
const PORTAL_MIME: &str = "text/plain;charset=utf-8";
/// Heuristic, not an event: clipboard managers (e.g. Plasma's Klipper) fetch every new selection eagerly, and
/// data-control cannot identify the requester. Reads within this window are excluded from paste_consumed; a manager
/// that reads lazily after ctrl+v can still produce a false paste_consumed:true.
const MANAGER_SETTLE: Duration = Duration::from_millis(250);
impl ClipboardHandle {
    /// Prefer wlr data-control; fall back to ext only when wlr is not advertised.
    pub fn start(spec: &TargetSpec) -> Result<Self, String> {
        match Self::start_with(spec, Backend::Wlr) {
            Err(e) if not_advertised(&e) => Self::start_with(spec, Backend::Ext).map_err(|e2| if not_advertised(&e2) {
                "neither zwlr_data_control_manager_v1 nor ext_data_control_manager_v1 is advertised: clipboard unsupported on this target via data-control (portal clipboard would require a consented RemoteDesktop session)".to_string()
            } else { e2 }),
            other => other,
        }
    }
    fn start_with(spec: &TargetSpec, backend: Backend) -> Result<Self, String> {
        let (tx, rx) = mpsc::channel::<Cmd>();
        let spec2 = spec.clone();
        let stats = Arc::new(Stats::default());
        let handle = Self { inner: Inner::DataControl(tx, stats.clone()), target_id: spec.id.clone() };
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
        std::thread::Builder::new().name(format!("clipboard-{}", spec.id)).spawn(move || {
            let r = match backend { Backend::Wlr => wlr::run_thread(&spec2, rx, &ready_tx, stats), Backend::Ext => ext::run_thread(&spec2, rx, &ready_tx, stats) };
            if let Err(e) = r {
                // Before startup completes this is the handshake failure; afterwards nobody listens.
                let _ = ready_tx.send(Err(e.clone()));
                eprintln!("clipboard thread for {} exited: {e}", spec2.id);
            }
        }).map_err(|e| format!("spawn clipboard thread: {e}"))?;
        ready_rx.recv_timeout(Duration::from_secs(5)).map_err(|e| format!("clipboard startup timed out: {e}"))??;
        Ok(handle)
    }
    /// Portal-backed (text-only) clipboard riding an existing consented RemoteDesktop session.
    pub fn portal(h: crate::portal_session::PortalHandle, target_id: String) -> Self { Self { inner: Inner::Portal(h), target_id } }
    /// Some(reason) when this handle is backed by a portal session that has ended.
    pub fn portal_dead_reason(&self) -> Option<String> {
        match &self.inner { Inner::Portal(h) => h.dead_reason(), Inner::DataControl(_, _) => None }
    }
    /// True when `start` failed because no data-control protocol exists (the only case portal is considered).
    pub fn is_unsupported_error(e: &str) -> bool { e.starts_with("neither zwlr_data_control_manager_v1 nor ext_data_control_manager_v1 is advertised") }
    fn call<T>(&self, cmd: impl FnOnce(mpsc::Sender<Result<T, String>>) -> Cmd) -> Result<T, String> {
        let Inner::DataControl(tx, _) = &self.inner else { return Err("internal: data-control command on portal clipboard".into()) };
        let (rtx, rrx) = mpsc::channel();
        tx.send(cmd(rtx)).map_err(|_| "clipboard thread is gone (target clipboard unavailable)".to_string())?;
        rrx.recv_timeout(Duration::from_secs(10)).map_err(|e| format!("clipboard operation timed out: {e}"))?
    }
    pub fn inventory(&self, primary: bool) -> Result<Vec<String>, String> {
        if let Inner::Portal(h) = &self.inner {
            if primary { return Err("portal clipboard has no primary selection".into()); }
            return Ok(if h.clipboard_read_text()?.is_empty() { vec![] } else { vec![PORTAL_MIME.to_string()] });
        }
        self.call(|reply| Cmd::Inventory { primary, reply })
    }
    pub fn read(&self, mime: Option<String>, primary: bool) -> Result<(String, Vec<u8>), String> {
        if let Inner::Portal(h) = &self.inner {
            if primary { return Err("portal clipboard has no primary selection".into()); }
            if let Some(m) = &mime { if !m.starts_with("text/plain") { return Err("portal clipboard is text-only".into()); } }
            let text = h.clipboard_read_text()?;
            return Ok(if text.is_empty() { (String::new(), Vec::new()) } else { (PORTAL_MIME.to_string(), text.into_bytes()) });
        }
        self.call(|reply| Cmd::Read { mime, primary, reply })
    }
    pub fn write(&self, payloads: Vec<(String, Vec<u8>)>) -> Result<(), String> {
        if let Inner::Portal(h) = &self.inner {
            return match payloads.as_slice() {
                [(m, b)] if m.starts_with("text/plain") => h.clipboard_write_text(String::from_utf8(b.clone()).map_err(|_| "portal clipboard text must be UTF-8".to_string())?),
                _ => Err("portal clipboard is text-only: cannot write non-text or multi-MIME payloads".into()),
            };
        }
        self.call(|reply| Cmd::Write { payloads, primary: false, reply })
    }
    pub fn shutdown(&self) {
        // Portal clipboard shares the PortalHandle's session; the portal registry owns its lifetime.
        if let Inner::DataControl(tx, _) = &self.inner { let _ = tx.send(Cmd::Shutdown); }
    }
    /// Number of selection-owner changes this clipboard thread has observed (0 for portal). Take it
    /// before an action, then `wait_selection_change` to learn whether an app took ownership.
    pub fn selection_generation(&self) -> u64 {
        match &self.inner { Inner::DataControl(_, s) => s.selections.load(Ordering::SeqCst), Inner::Portal(_) => 0 }
    }
    /// Block until the selection owner changes after `since`, or `timeout` elapses. True if it changed.
    pub fn wait_selection_change(&self, since: u64, timeout: Duration) -> bool {
        let Inner::DataControl(_, s) = &self.inner else { return false };
        let start = Instant::now();
        loop {
            if s.selections.load(Ordering::SeqCst) > since { return true; }
            if start.elapsed() >= timeout { return false; }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    /// Write `payloads` and return a marker for `wait_consumed`. Wayland never says who requests a
    /// transfer, and clipboard managers (Klipper, cliphist, ...) read every new selection immediately.
    /// The marker is therefore taken only after a short settle window, so those eager reads are not
    /// mistaken for the target app pasting; the caller sends ctrl+v after this returns.
    pub fn write_tracked(&self, payloads: Vec<(String, Vec<u8>)>) -> Result<u64, String> {
        let Inner::DataControl(_, s) = &self.inner else { return Err("internal: consumption tracking needs a data-control clipboard".into()) };
        self.write(payloads)?;
        std::thread::sleep(MANAGER_SETTLE);
        Ok(s.sends.load(Ordering::SeqCst))
    }
    /// Wait up to `timeout` for at least one transfer request after `marker` and for every received
    /// transfer to finish writing. False means no app requested the data in time (callers must report it).
    pub fn wait_consumed(&self, marker: u64, timeout: Duration) -> bool {
        let Inner::DataControl(_, s) = &self.inner else { return false };
        let start = Instant::now();
        loop {
            let sends = s.sends.load(Ordering::SeqCst);
            if sends > marker && s.done.load(Ordering::SeqCst) >= sends { return true; }
            if start.elapsed() >= timeout { return false; }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    /// Read every advertised MIME type if the total fits the preservation limit.
    pub fn read_all_preservable(&self) -> Result<Vec<(String, Vec<u8>)>, String> {
        if matches!(self.inner, Inner::Portal(_)) {
            return Err("portal clipboard cannot enumerate MIME formats, so preservation of the existing clipboard cannot be verified; refusing clipboard-assisted paste/copy rather than risk destroying non-text clipboard data (use type_text mode='keys', or an explicit clipboard_write)".into());
        }
        let mimes = self.inventory(false)?;
        let mut out = Vec::new(); let mut total = 0usize;
        for mime in mimes {
            let (_, bytes) = self.read(Some(mime.clone()), false)?;
            total += bytes.len();
            if total > MAX_PRESERVE_BYTES { return Err(format!("clipboard advertises more than the {MAX_PRESERVE_BYTES}-byte preservation limit; refusing to overwrite what cannot be restored")); }
            out.push((mime, bytes));
        }
        Ok(out)
    }
}
