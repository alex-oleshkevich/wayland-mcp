//! Consented portal session actor: one RemoteDesktop+ScreenCast session (created
//! only when the operator opted in with --portal), PipeWire frame
//! capture, and RemoteDesktop notify input. A consent refusal/cancel from the
//! portal is terminal for the triggering request — it is never routed around
//! via uinput or any other mechanism, and never retried automatically.
use crate::wayland::TargetSpec;
use ashpd::desktop::{
    clipboard::{Clipboard, RequestClipboardOptions, SetSelectionOptions},
    remote_desktop::{DeviceType, RemoteDesktop, SelectDevicesOptions, StartOptions},
    screencast::{CursorMode, OpenPipeWireRemoteOptions, Screencast, SelectSourcesOptions, SourceType},
    PersistMode, CreateSessionOptions, Session,
};
use std::{collections::HashMap, os::fd::IntoRawFd, sync::{atomic::{AtomicBool, Ordering}, mpsc, Arc, Mutex}, time::{Duration, Instant}};

#[derive(Clone, Debug)]
pub struct StreamInfo { pub node_id: u32, pub position: (i32, i32), pub size: (i32, i32) }
#[derive(Clone, Debug)]
pub struct PortalFrame { pub rgba: Vec<u8>, pub width: u32, pub height: u32, /// Age of the frame when served (0 at capture in the PipeWire thread; set by the actor on Capture replies).
    pub frame_age_ms: u128 }

pub enum Notify {
    MotionAbsolute { stream: u32, x: f64, y: f64 },
    Button { button: u32, pressed: bool },
    AxisDiscrete { vertical: bool, steps: i32 },
    AxisSmooth { vertical: bool, value: f64 },
    Keycode { code: u32, pressed: bool },
    Keysym { keysym: i32, pressed: bool },
    TouchDown { stream: u32, slot: u32, x: f64, y: f64 },
    TouchMotion { stream: u32, slot: u32, x: f64, y: f64 },
    TouchUp { slot: u32 },
}

pub enum Cmd {
    Capture { node_id: u32, reply: mpsc::Sender<Result<PortalFrame, String>> },
    Notify { n: Notify, reply: mpsc::Sender<Result<(), String>> },
    ClipboardRead { reply: mpsc::Sender<Result<String, String>> },
    ClipboardWrite { text: String, reply: mpsc::Sender<Result<(), String>> },
    Shutdown,
}

#[derive(Clone)]
pub struct PortalHandle {
    tx: mpsc::Sender<Cmd>,
    pub streams: Vec<StreamInfo>,
    pub devices: String,
    /// Why the portal clipboard is unusable on this session (None = enabled). Never fatal to capture/input.
    pub clipboard_unavailable: Option<String>,
    dead: Arc<Mutex<Option<String>>>,
}

/// node_id -> (frame sequence, when stored, frame or error)
type Frames = Arc<Mutex<HashMap<u32, (u64, Instant, Result<PortalFrame, String>)>>>;
type Dead = Arc<Mutex<Option<String>>>;

pub fn portal_enabled() -> bool { crate::config::get().portal }

impl PortalHandle {
    /// Establish the consented session. Blocks until the user answers the portal
    /// dialog (this is the explicit-consent point) or it times out.
    pub fn start(target: &TargetSpec) -> Result<Self, String> {
        if !portal_enabled() {
            return Err("portal path not enabled: start the server with --portal (or WAYLAND_MCP_PORTAL=1) to opt into an explicitly consented RemoteDesktop/ScreenCast session. Refusing to bypass portal consent via any other mechanism.".into());
        }
        let (tx, rx) = mpsc::channel::<Cmd>();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(Vec<StreamInfo>, String, Option<String>), String>>();
        let session_bus = target.session_bus.clone();
        let cancel = Arc::new(tokio::sync::Notify::new());
        let cancel_actor = cancel.clone();
        let dead: Dead = Arc::new(Mutex::new(None));
        let dead_actor = dead.clone();
        std::thread::Builder::new().name(format!("portal-{}", target.id)).spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(e) => { let _ = ready_tx.send(Err(format!("portal runtime: {e}"))); return; }
            };
            // Stay inside the runtime context until the LocalSet is dropped: its
            // spawn_local tasks own zbus signal streams whose Drop needs a reactor,
            // and a panic in that destructor aborts the whole server process.
            let _enter = rt.enter();
            // LocalSet: the clipboard transfer/owner listeners are spawn_local tasks on this actor thread.
            let local = tokio::task::LocalSet::new();
            local.block_on(&rt, async move {
                match establish(session_bus, rx, ready_tx, cancel_actor, dead_actor).await {
                    Ok(()) => {}
                    Err(e) => eprintln!("portal actor exited: {e}"),
                }
            });
        }).map_err(|e| format!("spawn portal actor: {e}"))?;
        let timeout_ms = crate::config::get().portal_timeout_ms;
        let (streams, devices, clipboard_unavailable) = match ready_rx.recv_timeout(Duration::from_millis(timeout_ms)) {
            Ok(r) => r?,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                cancel.notify_one();
                return Err(format!("portal session establishment timed out after {timeout_ms} ms (consent dialog unanswered?); cancelling: the session is being closed, which dismisses the dialog — if it is still visible, it is stale and any answer is discarded"));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("portal actor exited during setup before signalling readiness (session-bus connect/spawn or actor failure); see server stderr for the underlying error".into());
            }
        };
        Ok(Self { tx, streams, devices, clipboard_unavailable, dead })
    }

    fn call<T>(&self, cmd: impl FnOnce(mpsc::Sender<Result<T, String>>) -> Cmd) -> Result<T, String> {
        if let Some(reason) = self.dead_reason() { return Err(format!("portal session ended: {reason}")); }
        let (rtx, rrx) = mpsc::channel();
        self.tx.send(cmd(rtx)).map_err(|_| "portal actor is gone (session closed?)".to_string())?;
        rrx.recv_timeout(Duration::from_secs(15)).map_err(|e| format!("portal operation timed out: {e}"))?
    }
    /// Some(reason) once the session/stream has ended; captures/input must fail explicitly.
    pub fn dead_reason(&self) -> Option<String> { self.dead.lock().unwrap().clone() }
    pub fn capture(&self, node_id: u32) -> Result<PortalFrame, String> { self.call(|reply| Cmd::Capture { node_id, reply }) }
    pub fn notify(&self, n: Notify) -> Result<(), String> { self.call(|reply| Cmd::Notify { n, reply }) }
    /// Read the portal clipboard text ("" = empty, a normal state).
    pub fn clipboard_read_text(&self) -> Result<String, String> {
        if let Some(why) = &self.clipboard_unavailable { return Err(format!("portal clipboard unavailable: {why}")); }
        self.call(|reply| Cmd::ClipboardRead { reply })
    }
    /// Own the portal clipboard with text; ownership is the persistent session's.
    pub fn clipboard_write_text(&self, text: String) -> Result<(), String> {
        if let Some(why) = &self.clipboard_unavailable { return Err(format!("portal clipboard unavailable: {why}")); }
        self.call(|reply| Cmd::ClipboardWrite { text, reply })
    }
    pub fn shutdown(&self) { let _ = self.tx.send(Cmd::Shutdown); }
    /// Stream whose logical position/size exactly matches an output, if any.
    /// No silent fallback to another stream: a mismatch is an explicit error
    /// at the caller, which names the requested output and available streams.
    pub fn stream_for_output(&self, x: i32, y: i32, w: i32, h: i32) -> Option<&StreamInfo> {
        self.streams.iter().find(|s| s.position == (x, y) && s.size == (w, h))
    }
}

async fn establish(session_bus: Option<String>, rx: mpsc::Receiver<Cmd>, ready: mpsc::Sender<Result<(Vec<StreamInfo>, String, Option<String>), String>>, cancel: Arc<tokio::sync::Notify>, dead: Dead) -> Result<(), String> {
    let conn_res: Result<zbus::Connection, String> = match &session_bus {
        Some(addr) => match addr.parse::<zbus::address::Address>() {
            Ok(address) => match zbus::connection::Builder::address(address) {
                Ok(b) => b.build().await.map_err(|e| format!("portal bus connect: {e}")),
                Err(e) => Err(e.to_string()),
            },
            Err(e) => Err(format!("portal bus address: {e}")),
        },
        None => zbus::Connection::session().await.map_err(|e| format!("portal session bus: {e}")),
    };
    let conn = match conn_res {
        Ok(c) => c,
        Err(e) => { let _ = ready.send(Err(e.clone())); return Err(e); }
    };
    // Cancellation: PortalHandle::start fires `cancel` on ready-timeout. ashpd's
    // `RemoteDesktop::start(..).await` awaits the Request's Response signal
    // internally and never exposes the Request, so the only portal-side handle
    // we own is the Session: `Session::close()` ("Close" on
    // org.freedesktop.portal.Session) ends the session, which makes the portal
    // tear down the in-flight Start request and its consent dialog. Dropping
    // the pending future itself (select! branch loser) only abandons our wait;
    // it does not close anything — hence the explicit close below.
    let cancelled = cancel.notified();
    tokio::pin!(cancelled);
    let remote = RemoteDesktop::with_connection(conn.clone()).await.map_err(|e| { let e = format!("RemoteDesktop portal: {e}"); let _ = ready.send(Err(e.clone())); e })?;
    let cast = Screencast::with_connection(conn.clone()).await.map_err(|e| { let e = format!("ScreenCast portal: {e}"); let _ = ready.send(Err(e.clone())); e })?;
    let session: Session<RemoteDesktop> = tokio::select! {
        r = remote.create_session(CreateSessionOptions::default()) => match r {
            Ok(s) => s,
            Err(e) => { let e = format!("portal CreateSession: {e}"); let _ = ready.send(Err(e.clone())); return Err(e); }
        },
        _ = &mut cancelled => {
            eprintln!("portal establishment cancelled before a session existed; nothing to close");
            return Err("portal establishment cancelled".into());
        }
    };
    let clipboard_proxy = Clipboard::with_connection(conn.clone()).await;
    let rest = async {
        // RequestClipboard must precede Start. Failure only disables the clipboard (capture/input unaffected).
        let mut clip_err: Option<String> = match &clipboard_proxy {
            Ok(c) => c.request(&session, RequestClipboardOptions::default()).await.err().map(|e| format!("RequestClipboard: {e}")),
            Err(e) => Some(format!("Clipboard portal: {e}")),
        };
        remote.select_devices(&session, SelectDevicesOptions::default()
            .set_devices(DeviceType::Keyboard | DeviceType::Pointer | DeviceType::Touchscreen))
            .await.map_err(|e| format!("portal SelectDevices: {e}"))?
            .response().map_err(|e| format!("portal SelectDevices response: {e}"))?;
        cast.select_sources(&session, SelectSourcesOptions::default()
            .set_cursor_mode(CursorMode::Embedded)
            .set_sources(Some(SourceType::Monitor.into()))
            .set_multiple(true)
            .set_persist_mode(PersistMode::DoNot))
            .await.map_err(|e| format!("portal SelectSources: {e}"))?
            .response().map_err(|e| format!("portal SelectSources response: {e}"))?;
        // This presents the consent dialog; cancellation/refusal is terminal.
        let started = remote.start(&session, None, StartOptions::default()).await
            .map_err(|e| format!("portal Start: {e}"))?
            .response()
            .map_err(|e| format!("portal session refused or cancelled by the user ({e}); this is terminal — no fallback will be attempted"))?;
        let streams: Vec<StreamInfo> = started.streams().iter().map(|s| StreamInfo {
            node_id: s.pipe_wire_node_id(),
            position: s.position().unwrap_or((0, 0)),
            size: s.size().unwrap_or((0, 0)),
        }).collect();
        if streams.is_empty() { return Err("portal session started but returned no screen streams".to_string()); }
        if clip_err.is_none() && !started.is_clipboard_enabled() {
            clip_err = Some("the portal did not enable clipboard access for this session (Start response clipboard_enabled=false)".into());
        }
        let devices = format!("{:?}", started.devices());
        let fd = cast.open_pipe_wire_remote(&session, OpenPipeWireRemoteOptions::default()).await
            .map_err(|e| format!("portal OpenPipeWireRemote: {e}"))?;
        Ok::<(Vec<StreamInfo>, String, std::os::fd::OwnedFd, Option<String>), String>((streams, devices, fd, clip_err))
    };
    let outcome = tokio::select! {
        r = rest => Some(r),
        _ = &mut cancelled => None,
    };
    let (streams, devices, pw_fd, clip_err) = match outcome {
        Some(Ok(v)) => v,
        Some(Err(e)) => {
            let _ = session.close().await;
            let _ = ready.send(Err(e.clone()));
            return Err(e);
        }
        None => {
            match session.close().await {
                Ok(()) => eprintln!("portal establishment cancelled (timeout/shutdown): session closed, consent dialog should be dismissed"),
                Err(e) => eprintln!("portal establishment cancelled: session close failed ({e}); consent dialog may remain until answered"),
            }
            return Err("portal establishment cancelled".into());
        }
    };
    let _ = cancelled; // establishment finished; no further cancellation.
    // PipeWire capture thread.
    let frames: Frames = Arc::new(Mutex::new(HashMap::new()));
    let frame_seq = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let pw_handle = {
        let frames = frames.clone(); let stop = stop.clone(); let dead = dead.clone(); let frame_seq = frame_seq.clone();
        let nodes: Vec<u32> = streams.iter().map(|s| s.node_id).collect();
        match std::thread::Builder::new().name("portal-pipewire".into()).spawn(move || {
            if let Err(e) = run_pipewire(pw_fd.into_raw_fd(), nodes, frames, stop, dead, frame_seq) { eprintln!("pipewire capture exited: {e}"); }
        }) {
            Ok(h) => h,
            Err(e) => {
                let e = format!("spawn pipewire thread: {e}");
                let _ = session.close().await;
                let _ = ready.send(Err(e.clone()));
                return Err(e);
            }
        }
    };
    // Watch for the portal ending the session (sharing revoked from the desktop
    // indicator). The signal stream borrows the session, so it cannot be moved
    // into a 'static task; poll it (non-blocking) from the command loop below.
    let mut closed_stream = match session.receive_closed().await {
        Ok(s) => Some(Box::pin(s)),
        Err(e) => { eprintln!("portal Session Closed subscribe failed: {e}"); None }
    };
    // Clipboard listeners (only when the session has clipboard access).
    let served: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let owner_mimes: OwnerState = Arc::new(Mutex::new(None));
    if clip_err.is_none() {
        spawn_clipboard_listeners(conn.clone(), served.clone(), owner_mimes.clone());
    }
    let clip = if clip_err.is_none() { clipboard_proxy.ok() } else { None };
    let _ = ready.send(Ok((streams.clone(), devices, clip_err)));
    // Command loop (async notify calls; capture served from the frame map).
    loop {
        if let Some(cs) = closed_stream.as_mut() {
            use futures_util::Stream as _;
            let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
            if matches!(cs.as_mut().poll_next(&mut cx), std::task::Poll::Ready(Some(_))) {
                *dead.lock().unwrap() = Some("portal session closed by the portal (screen sharing revoked or ended)".to_string());
            }
        }
        // Poll instead of blocking recv so spawn_local clipboard listeners keep running between commands.
        let cmd = match rx.try_recv() {
            Ok(c) => c,
            Err(mpsc::TryRecvError::Empty) => { tokio::time::sleep(Duration::from_millis(10)).await; continue; }
            Err(mpsc::TryRecvError::Disconnected) => break,
        };
        match cmd {
            Cmd::ClipboardRead { reply } => {
                let r = match &clip { Some(c) => clipboard_read(c, &session, &served, &owner_mimes).await, None => Err("portal clipboard unavailable".to_string()) };
                let r = r.map_err(|e| if session_gone(&e) { *dead.lock().unwrap() = Some(e.clone()); format!("portal session ended: {e}") } else { e });
                let _ = reply.send(r);
            }
            Cmd::ClipboardWrite { text, reply } => {
                let r = match &clip { Some(c) => clipboard_write(c, &session, &served, &owner_mimes, text).await, None => Err("portal clipboard unavailable".to_string()) };
                let r = r.map_err(|e| if session_gone(&e) { *dead.lock().unwrap() = Some(e.clone()); format!("portal session ended: {e}") } else { e });
                let _ = reply.send(r);
            }
            Cmd::Shutdown => break,
            Cmd::Capture { node_id, reply } => {
                // Portal streams are damage-driven: on a LIVE stream, no new frame
                // means the screen is unchanged, so the latest frame IS current.
                // Wait briefly (<=300 ms) for a fresher frame (just-rendered damage
                // after an action), then serve the latest frame. Refuse only when
                // the session/stream is dead, or no frame was ever received.
                let started_at = Instant::now();
                let snapshot = frames.lock().unwrap().get(&node_id).map(|(seq, _, _)| *seq).unwrap_or(0);
                let ever_received = snapshot > 0;
                let fresh_wait = Duration::from_millis(300);
                let first_frame_wait = Duration::from_secs(3);
                let result = loop {
                    if let Some(reason) = dead.lock().unwrap().clone() { break Err(format!("portal session ended: {reason}")); }
                    let got = frames.lock().unwrap().get(&node_id).cloned();
                    match got {
                        Some((seq, _, Err(e))) if seq > snapshot => break Err(e),
                        Some((_, _, Err(e))) => break Err(e),
                        Some((seq, at, Ok(f))) if seq > snapshot => {
                            let mut f = f;
                            f.frame_age_ms = at.elapsed().as_millis();
                            break Ok(f);
                        }
                        Some((_, at, Ok(f))) => {
                            // Latest frame on a live stream; serve it once the
                            // short fresh-wait has elapsed (or immediately if it
                            // only just arrived within this request).
                            if started_at.elapsed() >= fresh_wait {
                                let mut f = f;
                                f.frame_age_ms = at.elapsed().as_millis();
                                break Ok(f);
                            }
                            tokio::time::sleep(Duration::from_millis(30)).await;
                        }
                        _ => {
                            let limit = if ever_received { fresh_wait } else { first_frame_wait };
                            if started_at.elapsed() > limit {
                                break Err(format!("no portal frame ever received for stream {node_id} (session alive but stream produced no frames)"));
                            }
                            tokio::time::sleep(Duration::from_millis(30)).await;
                        }
                    }
                };
                let _ = reply.send(result);
            }
            Cmd::Notify { n, reply } => {
                // Copy the flag out first: a guard temporary in the match scrutinee
                // would be held across the await and self-deadlock the write below.
                let already_dead = dead.lock().unwrap().clone();
                let r = match already_dead {
                    Some(reason) => Err(format!("portal session ended: {reason}")),
                    None => match tokio::time::timeout(Duration::from_secs(8), notify(&remote, &session, n)).await.unwrap_or_else(|_| Err("portal notify: no reply from the portal within 8 s (portal frontend gone or hung)".to_string())) {
                        // The portal frontend can vanish (crash/restart) without a
                        // Closed signal while the stream keeps flowing; a notify
                        // rejected as an invalid/unknown session is authoritative.
                        Err(e) if session_gone(&e) => {
                            *dead.lock().unwrap() = Some(e.clone());
                            Err(format!("portal session ended: {e}"))
                        }
                        other => other,
                    },
                };
                let _ = reply.send(r);
            }
        }
    }
    stop.store(true, Ordering::Relaxed);
    let _ = pw_handle.join();
    Ok(())
}

/// Latest clipboard owner info from SelectionOwnerChanged: (MIME list, this session is the owner).
type OwnerState = Arc<Mutex<Option<(Vec<String>, bool)>>>;
/// A portal error that authoritatively says the session no longer exists (frontend crash/restart, name vanished).
fn session_gone(e: &str) -> bool {
    let l = e.to_lowercase();
    l.contains("invalid session") || l.contains("unknownobject") || l.contains("unknown object") || l.contains("serviceunknown") || l.contains("name has no owner") || l.contains("no reply from the portal")
}

const PORTAL_TEXT_MIMES: [&str; 2] = ["text/plain;charset=utf-8", "text/plain"];

/// Serve SelectionTransfer requests with the text we own, and track the current owner's MIME list.
fn spawn_clipboard_listeners(conn: zbus::Connection, served: Arc<Mutex<Option<String>>>, owner_mimes: OwnerState) {
    use futures_util::StreamExt;
    let conn2 = conn.clone();
    tokio::task::spawn_local(async move {
        let clip = match Clipboard::with_connection(conn).await { Ok(c) => c, Err(e) => { eprintln!("portal clipboard transfer listener: {e}"); return; } };
        let mut stream = match clip.receive_selection_transfer::<RemoteDesktop>().await { Ok(s) => Box::pin(s), Err(e) => { eprintln!("portal SelectionTransfer subscribe: {e}"); return; } };
        while let Some((sess, mime, serial)) = stream.next().await {
            let text = served.lock().unwrap().clone();
            let Some(text) = text.filter(|_| PORTAL_TEXT_MIMES.contains(&mime.as_str())) else {
                let _ = clip.selection_write_done(&sess, serial, false).await; continue;
            };
            let ok = async {
                let fd = clip.selection_write(&sess, serial).await.map_err(|e| e.to_string())?;
                let mut tx = tokio::net::unix::pipe::Sender::from_owned_fd(std::os::fd::OwnedFd::from(fd)).map_err(|e| e.to_string())?;
                use tokio::io::AsyncWriteExt;
                tokio::time::timeout(Duration::from_secs(5), tx.write_all(text.as_bytes())).await.map_err(|_| "write timed out".to_string())?.map_err(|e| e.to_string())?;
                Ok::<(), String>(())
            }.await;
            if let Err(e) = &ok { eprintln!("portal clipboard serve failed: {e}"); }
            let _ = clip.selection_write_done(&sess, serial, ok.is_ok()).await;
        }
    });
    tokio::task::spawn_local(async move {
        let clip = match Clipboard::with_connection(conn2).await { Ok(c) => c, Err(_) => return };
        let mut stream = match clip.receive_selection_owner_changed::<RemoteDesktop>().await { Ok(s) => Box::pin(s), Err(_) => return };
        while let Some((_, change)) = stream.next().await { *owner_mimes.lock().unwrap() = Some((change.mime_types().to_vec(), change.session_is_owner() == Some(true))); }
    });
}

async fn clipboard_read(clip: &Clipboard, session: &Session<RemoteDesktop>, served: &Arc<Mutex<Option<String>>>, owner_mimes: &OwnerState) -> Result<String, String> {
    use tokio::io::AsyncReadExt;
    let (known_empty, we_own) = owner_mimes.lock().unwrap().as_ref().map(|(m, own)| (m.is_empty(), *own)).unwrap_or((false, false));
    if known_empty { return Ok(String::new()); }
    // mutter refuses SelectionRead of a selection owned by the same session ("Tried to read own selection"):
    // when this session owns the clipboard the text we set is the clipboard content, so answer from it.
    if we_own {
        if let Some(t) = served.lock().unwrap().clone() {
            // Probe the session: a vanished portal session (frontend restarted) must surface as an error, not a stale cached answer.
            if let Err(e) = clip.selection_read(session, PORTAL_TEXT_MIMES[0]).await { let e = format!("portal clipboard read: {e}"); if session_gone(&e) { return Err(e); } }
            return Ok(t);
        }
    }
    let mut last_err = String::new();
    for mime in PORTAL_TEXT_MIMES {
        let fd = match clip.selection_read(session, mime).await { Ok(fd) => fd, Err(e) => { last_err = format!("SelectionRead {mime}: {e}"); if last_err.contains("own selection") { if let Some(t) = served.lock().unwrap().clone() { return Ok(t); } } continue; } };
        let mut rx = tokio::net::unix::pipe::Receiver::from_owned_fd(std::os::fd::OwnedFd::from(fd)).map_err(|e| format!("portal clipboard pipe: {e}"))?;
        let mut buf = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), rx.read_to_end(&mut buf)).await
            .map_err(|_| "portal clipboard read timed out (source app unresponsive)".to_string())?
            .map_err(|e| format!("portal clipboard read: {e}"))?;
        if buf.len() > crate::clipboard::MAX_PRESERVE_BYTES * 2 { return Err("clipboard payload exceeds safety limit".into()); }
        return String::from_utf8(buf).map_err(|_| "portal clipboard text is not valid UTF-8".to_string());
    }
    // No text flavour readable: with no known owner MIME list this is indistinguishable from an empty clipboard.
    if owner_mimes.lock().unwrap().is_none() { Ok(String::new()) } else { Err(format!("portal clipboard has no readable text flavour ({last_err})")) }
}

async fn clipboard_write(clip: &Clipboard, session: &Session<RemoteDesktop>, served: &Arc<Mutex<Option<String>>>, owner_mimes: &Arc<Mutex<Option<(Vec<String>, bool)>>>, text: String) -> Result<(), String> {
    *served.lock().unwrap() = Some(text);
    *owner_mimes.lock().unwrap() = Some((PORTAL_TEXT_MIMES.iter().map(|m| m.to_string()).collect(), true));
    clip.set_selection(session, SetSelectionOptions::default().set_mime_types(&PORTAL_TEXT_MIMES)).await.map_err(|e| { *served.lock().unwrap() = None; format!("portal SetSelection: {e}") })
}

async fn notify(remote: &RemoteDesktop, session: &Session<RemoteDesktop>, n: Notify) -> Result<(), String> {
    use ashpd::desktop::remote_desktop::*;
    let e = |e: ashpd::Error| format!("portal notify: {e}");
    match n {
        Notify::MotionAbsolute { stream, x, y } => remote.notify_pointer_motion_absolute(session, stream, x, y, NotifyPointerMotionAbsoluteOptions::default()).await.map_err(e),
        Notify::Button { button, pressed } => remote.notify_pointer_button(session, button as i32, if pressed { KeyState::Pressed } else { KeyState::Released }, NotifyPointerButtonOptions::default()).await.map_err(e),
        Notify::AxisDiscrete { vertical, steps } => remote.notify_pointer_axis_discrete(session, if vertical { Axis::Vertical } else { Axis::Horizontal }, steps, NotifyPointerAxisDiscreteOptions::default()).await.map_err(e),
        Notify::AxisSmooth { vertical, value } => { let (dx, dy) = if vertical { (0.0, value) } else { (value, 0.0) }; remote.notify_pointer_axis(session, dx, dy, NotifyPointerAxisOptions::default()).await.map_err(e) }
        Notify::Keycode { code, pressed } => remote.notify_keyboard_keycode(session, code as i32, if pressed { KeyState::Pressed } else { KeyState::Released }, NotifyKeyboardKeycodeOptions::default()).await.map_err(e),
        Notify::Keysym { keysym, pressed } => remote.notify_keyboard_keysym(session, keysym, if pressed { KeyState::Pressed } else { KeyState::Released }, NotifyKeyboardKeysymOptions::default()).await.map_err(e),
        Notify::TouchDown { stream, slot, x, y } => remote.notify_touch_down(session, stream, slot, x, y, NotifyTouchDownOptions::default()).await.map_err(e),
        Notify::TouchMotion { stream, slot, x, y } => remote.notify_touch_motion(session, stream, slot, x, y, NotifyTouchMotionOptions::default()).await.map_err(e),
        Notify::TouchUp { slot } => remote.notify_touch_up(session, slot, NotifyTouchUpOptions::default()).await.map_err(e),
    }
}

// ---------- PipeWire capture ----------
fn run_pipewire(fd: i32, nodes: Vec<u32>, frames: Frames, stop: Arc<AtomicBool>, dead: Dead, frame_seq: Arc<std::sync::atomic::AtomicU64>) -> Result<(), String> {
    use pipewire as pw;
    use pw::{properties::properties, spa};
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(|e| format!("pw mainloop: {e}"))?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(|e| format!("pw context: {e}"))?;
    let core = context.connect_fd_rc(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) }, None).map_err(|e| format!("pw connect_fd: {e}"))?;
    use std::os::fd::FromRawFd;
    let mut streams = Vec::new();
    for node in nodes {
        let frames_for_stream = frames.clone();
        let seq_for_stream = frame_seq.clone();
        let stream = pw::stream::StreamBox::new(&core, "wayland-mcp-capture", properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        }).map_err(|e| format!("pw stream: {e}"))?;
        let listener = stream.add_local_listener_with_user_data(spa::param::video::VideoInfoRaw::default())
            .state_changed({
                let dead = dead.clone(); let frames = frames.clone(); let frame_seq = frame_seq.clone();
                move |_, _, _, new| {
                    let reason = match &new {
                        pw::stream::StreamState::Error(e) => Some(format!("portal PipeWire stream {node} error: {e}")),
                        pw::stream::StreamState::Unconnected => Some(format!("portal PipeWire stream {node} ended (unconnected)")),
                        _ => None,
                    };
                    if let Some(reason) = reason {
                        *dead.lock().unwrap() = Some(reason.clone());
                        let seq = frame_seq.fetch_add(1, Ordering::SeqCst) + 1;
                        frames.lock().unwrap().insert(node, (seq, Instant::now(), Err(reason)));
                    }
                }
            })
            .param_changed(move |_, format, id, param| {
                let Some(param) = param else { return };
                if id != spa::param::ParamType::Format.as_raw() { return; }
                let Ok((mt, mst)) = spa::param::format_utils::parse_format(param) else { return };
                if mt != spa::param::format::MediaType::Video || mst != spa::param::format::MediaSubtype::Raw { return; }
                let _ = format.parse(param);
            })
            .process({
                let frames = frames_for_stream.clone();
                let frame_seq = seq_for_stream.clone();
                move |stream, format| {
                    // Inside pipewire's extern "C" trampoline: a panic aborts the
                    // server. All indexing is bounds-checked; short chunks skipped.
                    let Some(mut buffer) = stream.dequeue_buffer() else { return };
                    let datas = buffer.datas_mut();
                    if datas.is_empty() { return; }
                    let data = &mut datas[0];
                    let chunk_size = data.chunk().size() as usize;
                    let stride = data.chunk().stride().max(0) as usize;
                    let Some(raw) = data.data() else { return };
                    let raw = &raw[..chunk_size.min(raw.len())];
                    let (w, h) = (format.size().width, format.size().height);
                    if w == 0 || h == 0 { return; }
                    if frame_needed_len(stride, w, h, format.format()).map_or(false, |need| raw.len() < need) { return; }
                    let converted = convert_frame(raw, stride, w, h, format.format());
                    let seq = frame_seq.fetch_add(1, Ordering::SeqCst) + 1;
                    frames.lock().unwrap().insert(node, (seq, Instant::now(), converted.map(|rgba| PortalFrame { rgba, width: w, height: h, frame_age_ms: 0 })));
                }
            })
            .register().map_err(|e| format!("pw listener: {e}"))?;
        let obj = spa::pod::object!(
            spa::utils::SpaTypes::ObjectParamFormat,
            spa::param::ParamType::EnumFormat,
            spa::pod::property!(spa::param::format::FormatProperties::MediaType, Id, spa::param::format::MediaType::Video),
            spa::pod::property!(spa::param::format::FormatProperties::MediaSubtype, Id, spa::param::format::MediaSubtype::Raw),
            spa::pod::property!(spa::param::format::FormatProperties::VideoFormat, Choice, Enum, Id,
                spa::param::video::VideoFormat::BGRx,
                spa::param::video::VideoFormat::BGRx,
                spa::param::video::VideoFormat::BGRA,
                spa::param::video::VideoFormat::RGBx,
                spa::param::video::VideoFormat::RGBA,
                spa::param::video::VideoFormat::RGB),
            spa::pod::property!(spa::param::format::FormatProperties::VideoSize, Choice, Range, Rectangle,
                spa::utils::Rectangle { width: 1920, height: 1080 },
                spa::utils::Rectangle { width: 1, height: 1 },
                spa::utils::Rectangle { width: 8192, height: 8192 }),
            spa::pod::property!(spa::param::format::FormatProperties::VideoFramerate, Choice, Range, Fraction,
                spa::utils::Fraction { num: 30, denom: 1 },
                spa::utils::Fraction { num: 0, denom: 1 },
                spa::utils::Fraction { num: 1000, denom: 1 }),
        );
        let values: Vec<u8> = spa::pod::serialize::PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &spa::pod::Value::Object(obj))
            .map_err(|e| format!("pw pod serialize: {e:?}"))?.0.into_inner();
        let mut params = [spa::pod::Pod::from_bytes(&values).ok_or("pw pod parse")?];
        stream.connect(spa::utils::Direction::Input, Some(node), pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS, &mut params)
            .map_err(|e| format!("pw stream connect node {node}: {e}"))?;
        streams.push((stream, listener));
    }
    while !stop.load(Ordering::Relaxed) {
        mainloop.loop_().iterate(Duration::from_millis(60));
    }
    drop(streams);
    Ok(())
}

fn frame_bpp(format: pipewire::spa::param::video::VideoFormat) -> Option<usize> {
    use pipewire::spa::param::video::VideoFormat as F;
    match format { F::BGRx | F::BGRA | F::RGBx | F::RGBA => Some(4), F::RGB => Some(3), _ => None }
}
fn frame_needed_len(stride: usize, w: u32, h: u32, format: pipewire::spa::param::video::VideoFormat) -> Option<usize> {
    let bpp = frame_bpp(format)?;
    if h == 0 { return Some(0); }
    stride.checked_mul((h - 1) as usize)?.checked_add((w as usize).checked_mul(bpp)?)
}

fn convert_frame(raw: &[u8], stride: usize, w: u32, h: u32, format: pipewire::spa::param::video::VideoFormat) -> Result<Vec<u8>, String> {
    use pipewire::spa::param::video::VideoFormat as F;
    let need = frame_needed_len(stride, w, h, format).ok_or_else(|| format!("unsupported PipeWire frame format {format:?}"))?;
    if raw.len() < need { return Err(format!("short PipeWire frame: {} bytes, need {need} (stride {stride}, {w}x{h})", raw.len())); }
    let bpp = frame_bpp(format).unwrap();
    if stride < w as usize * bpp { return Err(format!("PipeWire stride {stride} smaller than row width for {w}")); }
    let mut out = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h as usize {
        let row = raw.get(y * stride..y * stride + w as usize * bpp).ok_or("PipeWire row out of bounds")?;
        match format {
            F::BGRx | F::BGRA => {
                for x in 0..w as usize {
                    let i = x * 4;
                    out.extend_from_slice(&[row[i + 2], row[i + 1], row[i], if format == F::BGRA { row[i + 3] } else { 255 }]);
                }
            }
            F::RGBx | F::RGBA => {
                for x in 0..w as usize {
                    let i = x * 4;
                    out.extend_from_slice(&[row[i], row[i + 1], row[i + 2], if format == F::RGBA { row[i + 3] } else { 255 }]);
                }
            }
            F::RGB => {
                for x in 0..w as usize {
                    let i = x * 3;
                    out.extend_from_slice(&[row[i], row[i + 1], row[i + 2], 255]);
                }
            }
            other => return Err(format!("unsupported PipeWire frame format {other:?}")),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pipewire::spa::param::video::VideoFormat as F;
    #[test] fn short_frame_rejected_not_panicking() {
        assert!(convert_frame(&[0u8; 8], 8, 2, 2, F::BGRA).is_err());
        assert!(convert_frame(&[0u8; 0], 0, 2, 2, F::BGRA).is_err());
    }
    #[test] fn full_frame_converts() {
        let raw = [10, 20, 30, 40, 50, 60, 70, 80, 90, 100, 110, 120, 130, 140, 150, 160];
        let out = convert_frame(&raw, 8, 2, 2, F::BGRA).unwrap();
        assert_eq!(out.len(), 16);
        assert_eq!(&out[0..4], &[30, 20, 10, 40]);
    }
}
