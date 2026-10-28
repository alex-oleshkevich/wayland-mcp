//! Private nested-niri targets and server-owned application processes.
//! A sandbox isolates desktop interaction (own Wayland socket, D-Bus session,
//! accessibility bus and clipboard) — it is NOT a filesystem/network sandbox.
use crate::{server::data_dir, wayland::TargetSpec};
use std::{collections::{HashMap, VecDeque}, io::{BufRead, BufReader}, os::unix::process::CommandExt, path::{Path, PathBuf}, process::{Child, Command, Stdio}, sync::{atomic::{AtomicBool, Ordering}, Arc, Mutex}};

/// Kills started child pids on drop unless disarmed (successful start).
/// Prevents leaking niri/dbus-daemon/registryd, and anything the private bus
/// (second field: its socket path) activated, on any error path.
struct PidGuard(Vec<u32>, PathBuf);
impl PidGuard {
    fn disarm(&mut self) { self.0.clear(); }
}
impl Drop for PidGuard {
    fn drop(&mut self) {
        if self.0.is_empty() { return; }
        // Bus clients first, while the bus still runs (same order as stop_sandbox).
        stop_bus_clients(&self.1);
        for pid in &self.0 { unsafe { libc::kill(*pid as i32, libc::SIGTERM); } }
        // Pids and the bus stay in LIVE_SANDBOX_PIDS/LIVE_SANDBOX_BUSES
        // (registered at spawn time): if a daemon ignores SIGTERM, a later
        // shutdown sweep can still SIGKILL it, exactly as for a successfully
        // started sandbox, whose daemons are deregistered only by stop_sandbox.
    }
}

/// Pids of every live sandbox daemon (niri, bus, a11y) this process started,
/// so a forced exit (second termination signal) can kill them without
/// walking the server's maps or sleeping.
static LIVE_SANDBOX_PIDS: Mutex<Vec<u32>> = Mutex::new(Vec::new());

/// Socket paths of every live private sandbox bus. Processes the bus
/// D-Bus-activated are not our children; they are found by the bus address
/// in their environment (see `pids_on_buses`).
static LIVE_SANDBOX_BUSES: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

fn register_sandbox_pid(pid: u32) {
    let mut live = LIVE_SANDBOX_PIDS.lock().unwrap_or_else(|e| e.into_inner());
    if !live.contains(&pid) { live.push(pid); }
}

fn register_sandbox_bus(bus: &Path) {
    let mut live = LIVE_SANDBOX_BUSES.lock().unwrap_or_else(|e| e.into_inner());
    if !live.iter().any(|b| b == bus) { live.push(bus.to_path_buf()); }
}

/// Set once shutdown begins: in-flight sandbox starts abort at their next
/// checkpoint (their PidGuard kills what they already spawned) and no new
/// start begins. Set by begin_sandbox_shutdown from the server's cleanup.
static SANDBOX_START_CLOSED: AtomicBool = AtomicBool::new(false);

fn sandbox_start_closed() -> bool { SANDBOX_START_CLOSED.load(Ordering::SeqCst) }

/// Begin sandbox shutdown: refuse/abort in-flight starts and SIGKILL every
/// registered sandbox daemon, including daemons of a start that has not
/// returned yet (they register at spawn time, not at start success).
/// Called from the server's shutdown cleanup (server.rs/main.rs).
pub fn begin_sandbox_shutdown() {
    SANDBOX_START_CLOSED.store(true, Ordering::SeqCst);
    kill_live_sandboxes_now();
}

/// SIGKILL every registered sandbox daemon and every process on a registered
/// sandbox bus immediately (no waiting, no cleanup).
pub fn kill_live_sandboxes_now() {
    // Find bus clients before the buses die, while they are still connected.
    let buses: Vec<PathBuf> = LIVE_SANDBOX_BUSES.lock().unwrap_or_else(|e| e.into_inner()).drain(..).collect();
    let clients = pids_on_buses(&buses);
    for pid in LIVE_SANDBOX_PIDS.lock().unwrap_or_else(|e| e.into_inner()).drain(..) { unsafe { libc::kill(pid as i32, libc::SIGKILL); } }
    for pid in clients { unsafe { libc::kill(pid as i32, libc::SIGKILL); } }
}

/// Decode D-Bus address value escaping (`%xx`; other bytes are literal).
fn unescape_bus_value(value: &str) -> Vec<u8> {
    let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) { out.push(hi << 4 | lo); i += 3; continue; }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

/// Whether a D-Bus address (`;`-separated list of `transport:k=v,…`) names
/// the unix socket `bus` exactly: path equality, never a prefix match.
fn bus_address_names_socket(address: &str, bus: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    address.split(';').filter_map(|a| a.strip_prefix("unix:"))
        .flat_map(|params| params.split(','))
        .filter_map(|kv| kv.strip_prefix("path="))
        .any(|p| unescape_bus_value(p) == bus.as_os_str().as_bytes())
}

/// Live pids (never this process) whose initial environment has a
/// `DBUS_SESSION_BUS_ADDRESS` naming one of `buses`.
fn pids_on_buses(buses: &[PathBuf]) -> Vec<u32> {
    if buses.is_empty() { return Vec::new(); }
    let me = std::process::id();
    let Ok(dirs) = std::fs::read_dir("/proc") else { return Vec::new() };
    dirs.flatten().filter_map(|d| {
        let pid: u32 = d.file_name().to_str()?.parse().ok()?;
        if pid == me { return None; }
        let environ = std::fs::read(d.path().join("environ")).ok()?;
        let addr = environ.split(|b| *b == 0).find_map(|e| e.strip_prefix(b"DBUS_SESSION_BUS_ADDRESS="))?;
        let addr = std::str::from_utf8(addr).ok()?;
        buses.iter().any(|b| bus_address_names_socket(addr, b)).then_some(pid)
    }).collect()
}

/// SIGTERM every process on the private bus `bus`, wait briefly, SIGKILL
/// survivors. Survivors are re-scanned rather than reused, so a pid recycled
/// in between is signalled only if it is itself on this bus.
fn stop_bus_clients(bus: &Path) {
    let buses = [bus.to_path_buf()];
    let clients = pids_on_buses(&buses);
    if clients.is_empty() { return; }
    for pid in &clients { unsafe { libc::kill(*pid as i32, libc::SIGTERM); } }
    for _ in 0..20 {
        std::thread::sleep(std::time::Duration::from_millis(50));
        if pids_on_buses(&buses).is_empty() { return; }
    }
    for pid in pids_on_buses(&buses) { unsafe { libc::kill(pid as i32, libc::SIGKILL); } }
    for _ in 0..20 { if pids_on_buses(&buses).is_empty() { break; } std::thread::sleep(std::time::Duration::from_millis(50)); }
}

pub struct Sandbox {
    pub target_id: String,
    pub dir: std::path::PathBuf,
    pub niri_pid: u32,
    pub bus_pid: Option<u32>,
    pub socket_name: String,
    pub a11y_pid: Option<u32>,
}

pub struct LogStore { pub lines: VecDeque<(u64, String, String)>, pub next_seq: u64 }
impl LogStore { fn new() -> Self { Self { lines: VecDeque::new(), next_seq: 1 } } }

pub struct AppEntry {
    pub id: String,
    pub target_id: String,
    pub argv: Vec<String>,
    pub cwd: Option<String>,
    pub env: HashMap<String, String>,
    pub wayland_debug: bool,
    pub child: Mutex<Child>,
    pub logs: Arc<Mutex<LogStore>>,
    pub started_ms: u128,
}

fn spawn_log_reader<R: std::io::Read + Send + 'static>(reader: R, stream: &str, logs: Arc<Mutex<LogStore>>) {
    let stream = stream.to_string();
    std::thread::spawn(move || {
        let mut br = BufReader::new(reader);
        let mut buf: Vec<u8> = Vec::new();
        loop {
            buf.clear();
            // Lossy decode: one non-UTF-8 byte must neither stop capture nor
            // close the pipe (which would SIGPIPE the app).
            match br.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let line = String::from_utf8_lossy(&buf);
                    let mut store = logs.lock().unwrap();
                    let seq = store.next_seq; store.next_seq += 1;
                    store.lines.push_back((seq, stream.clone(), line.trim_end().to_string()));
                    while store.lines.len() > 10_000 { store.lines.pop_front(); }
                }
            }
        }
    });
}

/// Candidate locations of the distribution's session bus config.
const SESSION_BUS_CONFIGS: &[&str] = &["/usr/share/dbus-1/session.conf", "/etc/dbus-1/session.conf"];

fn xml_escape(s: &str) -> String { s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;") }

/// Write `<dir>/bus.conf`: the standard session config (service dirs,
/// policy, limits) plus `<dir>/services` as an extra service dir, so a
/// sandbox-local `.service` file can be activated on the private bus.
fn write_bus_config(dir: &std::path::Path) -> Result<std::path::PathBuf, String> {
    let session = SESSION_BUS_CONFIGS.iter().find(|p| std::path::Path::new(p).exists())
        .ok_or_else(|| format!("no D-Bus session config found (looked in {SESSION_BUS_CONFIGS:?})"))?;
    let services = dir.join("services");
    std::fs::create_dir_all(&services).map_err(|e| format!("create sandbox service dir: {e}"))?;
    let path = dir.join("bus.conf");
    let body = format!(
        "<!DOCTYPE busconfig PUBLIC \"-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN\"\n \"http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd\">\n<busconfig>\n  <include>{}</include>\n  <servicedir>{}</servicedir>\n</busconfig>\n",
        xml_escape(session), xml_escape(&services.to_string_lossy()));
    std::fs::write(&path, body).map_err(|e| format!("write sandbox bus config: {e}"))?;
    Ok(path)
}

pub fn start_sandbox(id: &str, width: u32, height: u32) -> Result<(Sandbox, TargetSpec, String), String> {
    if sandbox_start_closed() { return Err("server is shutting down; refusing to start a sandbox that would not be registered or cleaned up".into()); }
    let niri = std::process::Command::new("which").arg("niri").output().map_err(|e| e.to_string())?;
    if !niri.status.success() { return Err("nested target requires the `niri` executable, which was not found in PATH. Install niri; no different compositor will be substituted and the host desktop will not be taken over.".into()); }
    let dir = data_dir().join("sandboxes").join(id);
    let runtime = dir.join("runtime");
    // Never reuse a previous run's directory: stale bus/wayland sockets would
    // satisfy the waits below before the new daemons are listening.
    if dir.exists() { let _ = std::fs::remove_dir_all(&dir); }
    std::fs::create_dir_all(&runtime).map_err(|e| format!("create sandbox dir: {e}"))?;
    #[cfg(unix)]
    { use std::os::unix::fs::PermissionsExt; let _ = std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)); }
    // Private D-Bus session: the standard session config plus a sandbox-owned
    // service dir (dbus-daemon --session is exactly --config-file=session.conf).
    // A session bus hands its own environment to every service it activates,
    // so the host routing variables are removed here; the sandbox display is
    // added via UpdateActivationEnvironment once niri is up. Until then an
    // activation finds no display instead of escaping to the host.
    let bus_path = dir.join("bus");
    let bus_addr = format!("unix:path={}", bus_path.display());
    let bus_config = write_bus_config(&dir)?;
    let mut bus_cmd = Command::new("dbus-daemon");
    bus_cmd.args(["--nofork", &format!("--config-file={}", bus_config.display()), &format!("--address={bus_addr}")])
        .stdout(Stdio::null()).stderr(Stdio::null());
    strip_sandbox_host_env(&mut bus_cmd);
    for var in HOST_SESSION_ROUTING_ENV { bus_cmd.env_remove(var); }
    // DBUS_SESSION_BUS_ADDRESS is removed, not set: dbus-daemon gives every
    // service it activates on a session-type bus its own address, and the
    // daemon must not match its own client sweep (stop_bus_clients), which
    // has to finish before the bus is signalled.
    bus_cmd.env("XDG_RUNTIME_DIR", &runtime).env_remove("DBUS_SESSION_BUS_ADDRESS");
    let bus_child = bus_cmd.spawn().map_err(|e| format!("start private dbus-daemon: {e}"))?;
    let bus_pid = bus_child.id();
    // Register before anything else can fail or the process can exit: a
    // shutdown sweep (begin_sandbox_shutdown / kill_live_sandboxes_now)
    // must find this daemon (and what it activates) even though
    // start_sandbox has not returned.
    register_sandbox_pid(bus_pid);
    register_sandbox_bus(&bus_path);
    // Leak the handle intentionally: lifecycle is managed by pid in stop_sandbox.
    std::mem::forget(bus_child);
    // Every early return from here stops bus clients and kills bus/niri/a11y
    // pids started so far.
    let mut guard = PidGuard(vec![bus_pid], bus_path.clone());
    for _ in 0..50 {
        if sandbox_start_closed() { return Err("sandbox start aborted by server shutdown".into()); }
        if dir.join("bus").exists() { break; } std::thread::sleep(std::time::Duration::from_millis(100));
    }
    if !dir.join("bus").exists() { return Err("private D-Bus daemon did not create its socket".into()); }
    // Minimal niri config: fixed-size nested output, no host config.
    let config_path = dir.join("niri-config.kdl");
    std::fs::write(&config_path, format!("output \"winit\" {{\n    mode \"{width}x{height}@60\"\n    scale 1\n    position x=0 y=0\n}}\n")).map_err(|e| e.to_string())?;
    let log_path = dir.join("niri.log");
    let log_file = std::fs::File::create(&log_path).map_err(|e| e.to_string())?;
    let log_file2 = log_file.try_clone().map_err(|e| e.to_string())?;
    let host_display = std::env::var("WAYLAND_DISPLAY").map_err(|_| "WAYLAND_DISPLAY is not set; cannot locate the host Wayland socket for the nested compositor (no guessed socket name will be used)".to_string())?;
    let host_socket = if host_display.starts_with('/') { host_display.clone() } else {
        let host_runtime = std::env::var("XDG_RUNTIME_DIR").map_err(|_| "XDG_RUNTIME_DIR is not set and WAYLAND_DISPLAY is not an absolute path; refusing to guess the host runtime directory".to_string())?;
        format!("{host_runtime}/{host_display}")
    };
    let mut cmd = Command::new("niri");
    cmd.env("XDG_RUNTIME_DIR", &runtime)
        .env("WAYLAND_DISPLAY", &host_socket)
        .env("DBUS_SESSION_BUS_ADDRESS", &bus_addr)
        .env("NIRI_CONFIG", &config_path)
        .env_remove("NIRI_SOCKET")
        .stdout(Stdio::from(log_file)).stderr(Stdio::from(log_file2));
    if sandbox_start_closed() { return Err("sandbox start aborted by server shutdown".into()); }
    let child = cmd.spawn().map_err(|e| format!("start nested niri: {e}"))?;
    let niri_pid = child.id();
    register_sandbox_pid(niri_pid);
    guard.0.push(niri_pid);
    std::mem::forget(child);
    // Wait for the child's own socket (never guess the name).
    let mut socket_name: Option<String> = None;
    for _ in 0..100 {
        if sandbox_start_closed() { return Err("sandbox start aborted by server shutdown".into()); }
        if let Ok(entries) = std::fs::read_dir(&runtime) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if name.starts_with("wayland-") && !name.ends_with(".lock") { socket_name = Some(name); break; }
            }
        }
        if socket_name.is_some() { break; }
        if unsafe { libc::kill(niri_pid as i32, 0) } != 0 {
            let log = std::fs::read_to_string(&log_path).unwrap_or_default();
            return Err(format!("nested niri exited during startup. Log:\n{}", log.chars().take(3000).collect::<String>()));
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let socket_name = socket_name.ok_or_else(|| {
        let log = std::fs::read_to_string(&log_path).unwrap_or_default();
        format!("nested niri did not create a Wayland socket in {:?}. Log:\n{}", runtime, log.chars().take(3000).collect::<String>())
    })?;
    // The nested compositor's IPC socket. niri binds it before it listens on
    // the Wayland socket, so it exists by now unless binding failed (a
    // runtime path longer than SUN_LEN, logged in niri.log); then the sandbox
    // simply has no IPC, as before: NIRI_SOCKET stays unset, never the host's.
    let niri_socket = std::fs::read_dir(&runtime).ok().and_then(|entries| entries.flatten().map(|e| e.path())
        .find(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("niri.") && n.ends_with(".sock"))));
    let wayland_socket = runtime.join(&socket_name);
    // From now on every service the private bus activates runs inside the sandbox.
    let mut activation_env: Vec<(&str, &Path)> = vec![("WAYLAND_DISPLAY", &wayland_socket), ("XDG_RUNTIME_DIR", &runtime)];
    if let Some(sock) = &niri_socket { activation_env.push(("NIRI_SOCKET", sock)); }
    update_activation_environment(&bus_addr, &activation_env)?;
    // Ask the nested compositor for the requested mode via its IPC socket; the
    // effective geometry is always re-read and reported by the caller afterwards.
    if let Some(sock) = &niri_socket {
        let _ = Command::new("niri").args(["msg", "output", "winit", "custom-mode", &width.to_string(), &height.to_string(), "60000"])
            .env("NIRI_SOCKET", sock).output();
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
    let spec = TargetSpec { id: id.to_string(), socket: wayland_socket.clone(), session_bus: Some(bus_addr.clone()), kind: "sandbox".into() };
    // Accessibility for the private bus: activating org.a11y.Bus only creates the
    // a11y bus; the registry (org.a11y.atspi root) is a separate daemon that a
    // private session does not autostart, so the sandbox owns one explicitly.
    let mut a11y_pid = None;
    let _ = Command::new("gdbus").args(["call", "--address", &bus_addr, "--dest", "org.a11y.Bus", "--object-path", "/org/a11y/bus", "--method", "org.a11y.Bus.GetAddress"]).output();
    for candidate in ["/usr/lib/at-spi2-registryd", "/usr/libexec/at-spi2-registryd", "/usr/lib/at-spi2/at-spi2-registryd"] {
        if std::path::Path::new(candidate).exists() {
            let mut registryd = Command::new(candidate);
            strip_sandbox_host_env(&mut registryd);
            for var in HOST_SESSION_ROUTING_ENV { registryd.env_remove(var); }
            registryd.env("DBUS_SESSION_BUS_ADDRESS", &bus_addr).env("WAYLAND_DISPLAY", &wayland_socket).env("XDG_RUNTIME_DIR", &runtime);
            if let Some(sock) = &niri_socket { registryd.env("NIRI_SOCKET", sock); }
            registryd
                .stdout(Stdio::null()).stderr(Stdio::null());
            if let Ok(child) = registryd.spawn() {
                a11y_pid = Some(child.id());
                register_sandbox_pid(child.id());
                guard.0.push(child.id());
                std::mem::forget(child);
            }
            break;
        }
    }
    if a11y_pid.is_some() { std::thread::sleep(std::time::Duration::from_millis(400)); }
    if sandbox_start_closed() { return Err("sandbox start aborted by server shutdown".into()); }
    guard.disarm();
    // Pids were already registered at spawn time; nothing to extend here.
    Ok((Sandbox { target_id: id.to_string(), dir, niri_pid, bus_pid: Some(bus_pid), socket_name, a11y_pid }, spec, bus_addr))
}

pub fn stop_sandbox(sb: &Sandbox) {
    unsafe {
        libc::kill(sb.niri_pid as i32, libc::SIGTERM);
    }
    std::thread::sleep(std::time::Duration::from_millis(300));
    unsafe {
        if libc::kill(sb.niri_pid as i32, 0) == 0 { libc::kill(sb.niri_pid as i32, libc::SIGKILL); }
    }
    // Everything on this sandbox's bus (activated services, the a11y bus and
    // registry, leftover clients) before the bus itself goes away.
    let bus_path = sb.dir.join("bus");
    stop_bus_clients(&bus_path);
    unsafe {
        if let Some(pid) = sb.bus_pid { libc::kill(pid as i32, libc::SIGTERM); }
        if let Some(pid) = sb.a11y_pid { libc::kill(pid as i32, libc::SIGTERM); }
    }
    // Deregister only after every daemon has been signalled, so a forced exit
    // arriving mid-stop still finds them.
    LIVE_SANDBOX_PIDS.lock().unwrap_or_else(|e| e.into_inner()).retain(|p| *p != sb.niri_pid && Some(*p) != sb.bus_pid && Some(*p) != sb.a11y_pid);
    LIVE_SANDBOX_BUSES.lock().unwrap_or_else(|e| e.into_inner()).retain(|b| *b != bus_path);
    unmount_fuse_under(&sb.dir);
    let _ = std::fs::remove_dir_all(&sb.dir);
}

/// Decode a /proc/self/mountinfo path field (`\ooo` octal escapes).
fn unescape_mountinfo(field: &str) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    let oct = |c: u8| (b'0'..=b'7').contains(&c).then(|| c - b'0');
    let b = field.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() {
            if let (Some(x), Some(y), Some(z)) = (oct(b[i + 1]), oct(b[i + 2]), oct(b[i + 3])) {
                out.push(x.wrapping_shl(6) | y << 3 | z);
                i += 4;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    PathBuf::from(std::ffi::OsString::from_vec(out))
}

/// Lazily unmount FUSE mounts under `dir`, deepest first. Services the
/// sandbox bus activated mount into the sandbox runtime dir
/// (xdg-document-portal: runtime/doc, gvfsd-fuse: runtime/gvfs); when they
/// are SIGKILLed (shutdown sweep, unresponsive service) the mount stays
/// behind as a dead endpoint and would block removing the sandbox dir.
fn unmount_fuse_under(dir: &Path) {
    let Ok(info) = std::fs::read_to_string("/proc/self/mountinfo") else { return };
    let mut mounts: Vec<PathBuf> = info.lines().filter_map(|line| {
        let (pre, post) = line.split_once(" - ")?;
        if !post.split(' ').next()?.starts_with("fuse") { return None; }
        let target = unescape_mountinfo(pre.split(' ').nth(4)?);
        target.starts_with(dir).then_some(target)
    }).collect();
    mounts.sort_by_key(|m| std::cmp::Reverse(m.components().count()));
    for target in mounts {
        for tool in ["fusermount3", "fusermount"] {
            if Command::new(tool).arg("-u").arg("-z").arg(&target).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok() { break; }
        }
    }
}

/// Host session-routing variables (beyond SANDBOX_STRIPPED_ENV) that must not
/// reach anything the private bus activates: they point at the host
/// compositor and its IPC.
const HOST_SESSION_ROUTING_ENV: &[&str] = &["WAYLAND_DISPLAY", "NIRI_SOCKET", "HYPRLAND_INSTANCE_SIGNATURE", "SWAYSOCK"];

/// GVariant text-format string literal.
fn gvariant_string(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// Add `vars` to the environment the bus at `bus_addr` gives activated services.
fn update_activation_environment(bus_addr: &str, vars: &[(&str, &Path)]) -> Result<(), String> {
    let mut entries = Vec::with_capacity(vars.len());
    for (k, v) in vars {
        let v = v.to_str().ok_or_else(|| format!("sandbox path for {k} is not UTF-8: {v:?}"))?;
        entries.push(format!("{}: {}", gvariant_string(k), gvariant_string(v)));
    }
    let out = Command::new("gdbus").args(["call", "--address", bus_addr, "--dest", "org.freedesktop.DBus", "--object-path", "/org/freedesktop/DBus", "--method", "org.freedesktop.DBus.UpdateActivationEnvironment", &format!("{{{}}}", entries.join(", "))])
        .output().map_err(|e| format!("run gdbus for UpdateActivationEnvironment: {e}"))?;
    if !out.status.success() { return Err(format!("set the sandbox bus activation environment (UpdateActivationEnvironment) failed: {}", String::from_utf8_lossy(&out.stderr).trim())); }
    Ok(())
}

/// Host display-routing variables stripped for sandbox launches. The nested
/// compositor provides no X server, so an inherited DISPLAY (host Xwayland)
/// or a toolkit backend override would open the app on the HOST desktop,
/// outside sandbox screenshot/input/clipboard. Choice: DISPLAY is *removed*
/// (not set empty — an empty DISPLAY is still an X11 display name some
/// toolkits parse), WAYLAND_DISPLAY is set to the sandbox socket below, and
/// backend overrides are removed so GTK/Qt/SDL fall back to Wayland. An
/// X11-only app then fails explicitly ("cannot open display") instead of
/// escaping. Caller-supplied extra_env is applied after this, so an explicit
/// caller value for any of these variables still wins.
const SANDBOX_STRIPPED_ENV: &[&str] = &["DISPLAY", "GDK_BACKEND", "QT_QPA_PLATFORM", "SDL_VIDEODRIVER", "SDL_VIDEO_DRIVER", "CLUTTER_BACKEND"];

fn strip_sandbox_host_env(cmd: &mut Command) {
    for var in SANDBOX_STRIPPED_ENV { cmd.env_remove(var); }
}

pub fn launch_app(id: &str, target: &TargetSpec, argv: &[String], cwd: &Option<String>, extra_env: &HashMap<String, String>, wayland_debug: bool, started_ms: u128) -> Result<AppEntry, String> {
    if argv.is_empty() { return Err("argv must not be empty (no shell is used)".into()); }
    let logs = Arc::new(Mutex::new(LogStore::new()));
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).process_group(0);
    if let Some(cwd) = cwd { cmd.current_dir(cwd); }
    if target.kind == "sandbox" { strip_sandbox_host_env(&mut cmd); }
    cmd.env("WAYLAND_DISPLAY", target.socket.to_string_lossy().to_string());
    if let Some(parent) = target.socket.parent() { cmd.env("XDG_RUNTIME_DIR", parent.to_string_lossy().to_string()); }
    if let Some(bus) = &target.session_bus { cmd.env("DBUS_SESSION_BUS_ADDRESS", bus); }
    if wayland_debug { cmd.env("WAYLAND_DEBUG", "client"); }
    for (k, v) in extra_env { cmd.env(k, v); }
    let mut child = cmd.spawn().map_err(|e| format!("launch {:?}: {e}", argv[0]))?;
    if let Some(out) = child.stdout.take() { spawn_log_reader(out, "stdout", logs.clone()); }
    if let Some(errp) = child.stderr.take() { spawn_log_reader(errp, "stderr", logs.clone()); }
    Ok(AppEntry { id: id.to_string(), target_id: target.id.clone(), argv: argv.to_vec(), cwd: cwd.clone(), env: extra_env.clone(), wayland_debug, child: Mutex::new(child), logs, started_ms })
}

/// Parse (state, pgrp) from a /proc/<pid>/stat body. comm may contain spaces
/// and parentheses, so fields are taken after the last ')'.
fn parse_proc_stat(content: &str) -> Option<(char, u32)> {
    let after = &content[content.rfind(')')? + 1..];
    let mut fields = after.split_whitespace();
    let state = fields.next()?.chars().next()?;
    let _ppid = fields.next()?;
    let pgrp = fields.next()?.parse().ok()?;
    Some((state, pgrp))
}

/// Live (non-zombie) members of `entries` belonging to process group `pgid`.
fn select_live_group_members(entries: &[(u32, char, u32)], pgid: u32) -> Vec<u32> {
    entries.iter().filter(|(_, state, grp)| *grp == pgid && !matches!(state, 'Z' | 'X' | 'x')).map(|(pid, _, _)| *pid).collect()
}

fn live_group_members(pgid: u32) -> Vec<u32> {
    let mut entries = Vec::new();
    if let Ok(dirs) = std::fs::read_dir("/proc") {
        for dir in dirs.flatten() {
            let Ok(pid) = dir.file_name().to_string_lossy().parse::<u32>() else { continue };
            if let Ok(stat) = std::fs::read_to_string(dir.path().join("stat")) {
                if let Some((state, grp)) = parse_proc_stat(&stat) { entries.push((pid, state, grp)); }
            }
        }
    }
    select_live_group_members(&entries, pgid)
}

pub fn app_status(entry: &AppEntry) -> (String, Option<i32>) {
    let mut child = entry.child.lock().unwrap();
    match child.try_wait() {
        Ok(Some(status)) => ("exited".into(), status.code()),
        Ok(None) => ("running".into(), None),
        Err(_) => ("unknown".into(), None),
    }
}
pub fn stop_app(entry: &AppEntry, timeout_ms: u64) -> Result<serde_json::Value, String> {
    let pid = entry.child.lock().unwrap().id();
    // Probe the process group BEFORE reaping the leader in this call.
    // Safety against pgid reuse: while the leader is unreaped (even as a
    // zombie) or any member remains, the group is non-empty and its pgid
    // cannot be recycled, so kill(-pgid) reaches exactly the processes this
    // server launched (apps are spawned with process_group(0)). We only
    // ever signal when live members are observed; if the leader was reaped
    // by an earlier app_status and the group has since emptied, we signal
    // nothing. The leader is reaped (via app_status) only once we know
    // whether group members survive it.
    let others_survive_leader = live_group_members(pid).iter().any(|m| *m != pid);
    let (status, code) = app_status(entry);
    if status == "exited" && !others_survive_leader { return Ok(serde_json::json!({"stopped": false, "already_exited": true, "exit_code": code, "escalated": false})); }
    let leader_already_exited = status == "exited";
    unsafe { libc::kill(-(pid as i32), libc::SIGTERM); }
    let started = std::time::Instant::now();
    loop {
        let (status, code) = app_status(entry);
        if status == "exited" && live_group_members(pid).is_empty() { return Ok(serde_json::json!({"stopped": true, "exit_code": code, "escalated": false, "leader_already_exited": leader_already_exited})); }
        if started.elapsed().as_millis() as u64 >= timeout_ms {
            unsafe { libc::kill(-(pid as i32), libc::SIGKILL); }
            // Give the group a bounded chance to actually die before reporting.
            for _ in 0..20 { if live_group_members(pid).is_empty() { break; } std::thread::sleep(std::time::Duration::from_millis(50)); }
            let (_, code) = app_status(entry);
            return Ok(serde_json::json!({"stopped": true, "exit_code": code, "escalated": true, "leader_already_exited": leader_already_exited}));
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_env_strip_removes_host_display_routing() {
        let mut cmd = Command::new("true");
        cmd.env("DISPLAY", ":0").env("GDK_BACKEND", "x11").env("QT_QPA_PLATFORM", "xcb").env("SDL_VIDEODRIVER", "x11").env("KEEP_ME", "1");
        strip_sandbox_host_env(&mut cmd);
        let envs: Vec<(String, Option<String>)> = cmd.get_envs().map(|(k, v)| (k.to_string_lossy().into_owned(), v.map(|v| v.to_string_lossy().into_owned()))).collect();
        for var in SANDBOX_STRIPPED_ENV { assert!(envs.contains(&(var.to_string(), None)), "{var} must be removed, got {envs:?}"); }
        assert!(envs.contains(&("KEEP_ME".to_string(), Some("1".to_string()))));
    }

    #[test]
    fn proc_stat_parses_pgrp_after_parenthesised_comm() {
        assert_eq!(parse_proc_stat("4321 (weird ) name) S 100 4321 4321 0 -1"), Some(('S', 4321)));
        assert_eq!(parse_proc_stat("4321 (sleep) Z 100 4321 4321 0 -1"), Some(('Z', 4321)));
        assert_eq!(parse_proc_stat("garbage"), None);
    }

    #[test]
    fn bus_address_matches_socket_path_exactly() {
        let bus = Path::new("/d/sandboxes/s 1/bus");
        assert!(bus_address_names_socket("unix:path=/d/sandboxes/s%201/bus,guid=abc", bus));
        assert!(bus_address_names_socket("tcp:host=x;unix:path=/d/sandboxes/s 1/bus", bus));
        assert!(!bus_address_names_socket("unix:path=/d/sandboxes/s 1/bus2", bus), "prefix must not match");
        assert!(!bus_address_names_socket("unix:path=/d/sandboxes/s 1", bus));
        assert!(!bus_address_names_socket("unix:abstract=/d/sandboxes/s 1/bus", bus));
        assert!(!bus_address_names_socket("unix:path=/d/sandboxes/s%2", bus), "truncated escape stays literal");
    }

    #[test]
    fn live_group_members_exclude_zombies_and_other_groups() {
        let entries = [(10, 'S', 10), (11, 'R', 10), (12, 'Z', 10), (13, 'S', 99)];
        assert_eq!(select_live_group_members(&entries, 10), vec![10, 11]);
        assert!(select_live_group_members(&entries, 42).is_empty());
    }

    /// Install `org.example.EnvProbe` on the sandbox's own service dir and
    /// activate it through the sandbox bus. The probe records its pid and
    /// environment, claims the name, and (like a real service that does not
    /// exit on bus disconnect) keeps running until signalled.
    fn activate_env_probe(sb: &Sandbox, bus_addr: &str) -> (u32, HashMap<String, String>) {
        let out = sb.dir.join("envprobe.env");
        let script = sb.dir.join("envprobe.sh");
        std::fs::write(&script, format!(
            "#!/bin/sh\n{{ echo \"PROBE_PID=$$\"; env; }} > '{out}.tmp' && mv '{out}.tmp' '{out}'\ndbus-test-tool black-hole --name=org.example.EnvProbe &\nexec sleep infinity\n",
            out = out.display())).unwrap();
        { use std::os::unix::fs::PermissionsExt; std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap(); }
        std::fs::write(sb.dir.join("services/org.example.EnvProbe.service"), format!("[D-BUS Service]\nName=org.example.EnvProbe\nExec={}\n", script.display())).unwrap();
        let bus_call = |method: &str, args: &[&str]| Command::new("gdbus").args(["call", "--address", bus_addr, "--dest", "org.freedesktop.DBus", "--object-path", "/org/freedesktop/DBus", "--method", method]).args(args).output().unwrap();
        let reload = bus_call("org.freedesktop.DBus.ReloadConfig", &[]);
        assert!(reload.status.success(), "ReloadConfig: {}", String::from_utf8_lossy(&reload.stderr));
        let started = bus_call("org.freedesktop.DBus.StartServiceByName", &["org.example.EnvProbe", "0"]);
        assert!(started.status.success(), "StartServiceByName: {}", String::from_utf8_lossy(&started.stderr));
        for _ in 0..50 { if out.exists() { break; } std::thread::sleep(std::time::Duration::from_millis(100)); }
        let env: HashMap<String, String> = std::fs::read_to_string(&out).expect("probe wrote its environment")
            .lines().filter_map(|l| l.split_once('=')).map(|(k, v)| (k.to_string(), v.to_string())).collect();
        (env["PROBE_PID"].parse().unwrap(), env)
    }

    /// Pids whose environment names exactly this bus socket (independent of
    /// the production matcher: plain `unix:path=<bus>` with an optional
    /// `,key=value` suffix).
    fn host_pids_on_bus(bus_addr: &str) -> Vec<u32> {
        let want = format!("DBUS_SESSION_BUS_ADDRESS={bus_addr}");
        std::fs::read_dir("/proc").unwrap().flatten().filter_map(|d| {
            let pid: u32 = d.file_name().to_str()?.parse().ok()?;
            let environ = std::fs::read(d.path().join("environ")).ok()?;
            environ.split(|b| *b == 0).map(String::from_utf8_lossy)
                .any(|e| e == want || e.starts_with(&format!("{want},"))).then_some(pid)
        }).collect()
    }

    #[test]
    #[ignore = "needs a host niri session (TESTING.md)"]
    fn sandbox_bus_activates_services_inside_the_sandbox() {
        // Short ids: niri's IPC socket path under runtime/ must fit SUN_LEN.
        let (sb, spec, bus_addr) = start_sandbox(&format!("envp-a-{}", std::process::id()), 640, 480).unwrap();
        let (_pid, env) = activate_env_probe(&sb, &bus_addr);
        stop_sandbox(&sb);
        // Compare only the routing variables: the full environment holds secrets.
        let runtime = sb.dir.join("runtime");
        let routing = ["WAYLAND_DISPLAY", "XDG_RUNTIME_DIR", "DISPLAY"].map(|k| env.get(k).cloned());
        assert_eq!(routing, [Some(spec.socket.to_string_lossy().into_owned()), Some(runtime.to_string_lossy().into_owned()), None]);
        let niri_socket = env.get("NIRI_SOCKET").map(std::path::PathBuf::from);
        assert_eq!(niri_socket.as_deref().and_then(Path::parent), Some(runtime.as_path()), "NIRI_SOCKET must be the nested compositor's");
    }

    #[test]
    #[ignore = "needs a host niri session (TESTING.md)"]
    fn sandbox_stop_stops_bus_activated_services() {
        let (sb, _spec, bus_addr) = start_sandbox(&format!("envp-b-{}", std::process::id()), 640, 480).unwrap();
        let (pid, _env) = activate_env_probe(&sb, &bus_addr);
        assert!(host_pids_on_bus(&bus_addr).contains(&pid), "probe must be on the sandbox bus before stop");
        stop_sandbox(&sb);
        let survivors = host_pids_on_bus(&bus_addr);
        for p in &survivors { unsafe { libc::kill(*p as i32, libc::SIGKILL); } }
        assert!(survivors.is_empty(), "processes still on the stopped sandbox bus: {survivors:?}");
    }
}
