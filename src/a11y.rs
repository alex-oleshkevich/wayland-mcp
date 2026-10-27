//! AT-SPI2 accessibility over zbus. Refs are `bus_name|object_path` on the target's
//! accessibility bus; they are never tree offsets. Bounds may be client-relative or
//! missing on Wayland — uncertain bounds are reported, never converted into clicks.
use serde::Serialize;
use std::{future::Future, pin::Pin, time::{Duration, Instant}};
use zbus::{names::{BusName, InterfaceName}, zvariant::{ObjectPath, OwnedObjectPath, OwnedValue}, Connection, Proxy};

#[derive(Debug, Serialize, Clone)]
pub struct A11yNode {
    pub ref_id: String,
    pub name: Option<String>,
    pub role: Option<String>,
    pub role_id: Option<u32>,
    pub states: Vec<u32>,
    pub text: Option<String>,
    pub actions: Vec<String>,
    pub extents_screen: Option<[i32; 4]>,
    pub extents_note: Option<String>,
    pub children: Vec<A11yNode>,
    pub error: Option<String>,
}

pub async fn session_connection(session_bus: &Option<String>) -> Result<Connection, String> {
    match session_bus {
        Some(addr) => {
            let addr: zbus::address::Address = addr.parse().map_err(|e| format!("parse session bus address: {e}"))?;
            // zbus has no default method timeout (vendored zbus 5.19:
            // Builder.method_timeout is None unless set, and call_method only
            // applies a timeout when it is Some), so set one explicitly.
            zbus::connection::Builder::address(addr).map_err(|e| e.to_string())?.method_timeout(Duration::from_secs(10)).build().await.map_err(|e| format!("connect session bus: {e}"))
        }
        None => Connection::session().await.map_err(|e| format!("connect session bus (DBUS_SESSION_BUS_ADDRESS): {e}")),
    }
}

pub async fn a11y_connection(session_bus: &Option<String>) -> Result<Connection, String> {
    let session = session_connection(session_bus).await?;
    let proxy = Proxy::new(&session, "org.a11y.Bus", "/org/a11y/bus", "org.a11y.Bus").await.map_err(|e| format!("a11y bus proxy: {e}"))?;
    let address: String = tokio::time::timeout(Duration::from_secs(10), proxy.call("GetAddress", &())).await.map_err(|_| "AT-SPI GetAddress timed out after 10s".to_string())?.map_err(|e| format!("AT-SPI bus unavailable (GetAddress failed): {e}. Accessibility may not be enabled on this target; use a11y_enable."))?;
    let addr: zbus::address::Address = address.parse().map_err(|e| format!("parse a11y address: {e}"))?;
    zbus::connection::Builder::address(addr).map_err(|e| e.to_string())?.method_timeout(Duration::from_secs(10)).build().await.map_err(|e| format!("connect AT-SPI bus: {e}"))
}

pub async fn status(session_bus: &Option<String>) -> Result<serde_json::Value, String> {
    let session = session_connection(session_bus).await?;
    let proxy = Proxy::new(&session, "org.a11y.Bus", "/org/a11y/bus", "org.a11y.Status").await.map_err(|e| e.to_string())?;
    let enabled: Result<bool, _> = proxy.get_property("IsEnabled").await;
    let reader: Result<bool, _> = proxy.get_property("ScreenReaderEnabled").await;
    Ok(serde_json::json!({"is_enabled": enabled.ok(), "screen_reader_enabled": reader.ok()}))
}
pub async fn set_enabled(session_bus: &Option<String>, value: bool) -> Result<(), String> {
    let session = session_connection(session_bus).await?;
    let proxy = Proxy::new(&session, "org.a11y.Bus", "/org/a11y/bus", "org.a11y.Status").await.map_err(|e| e.to_string())?;
    proxy.set_property("IsEnabled", value).await.map_err(|e| format!("set IsEnabled: {e}"))
}

/// Read-only accessibility check for `doctor`. Returns `(status, detail)`:
/// - `unavailable`: no session bus, or nothing owns `org.a11y.Bus`. The bus is
///   not activated here: activation would start a daemon, and doctor must not
///   change anything.
/// - `error`: the bus is owned but its address or the AT-SPI bus can't be reached.
/// - `disabled`: the AT-SPI bus works but `IsEnabled` is false, so toolkits
///   may not expose their apps (a11y_enable turns it on).
/// - `available`: reachable and enabled; `applications` counts registered apps.
pub async fn probe(session_bus: &Option<String>) -> (&'static str, serde_json::Value) {
    let session = match session_connection(session_bus).await {
        Ok(c) => c,
        Err(e) => return ("unavailable", serde_json::json!({"error": e})),
    };
    let dbus = match zbus::fdo::DBusProxy::new(&session).await {
        Ok(p) => p,
        Err(e) => return ("error", serde_json::json!({"error": format!("D-Bus proxy: {e}")})),
    };
    let owned = match BusName::try_from("org.a11y.Bus") {
        Ok(name) => tokio::time::timeout(Duration::from_secs(3), dbus.name_has_owner(name)).await,
        Err(e) => return ("error", serde_json::json!({"error": e.to_string()})),
    };
    match owned {
        Ok(Ok(true)) => {}
        Ok(Ok(false)) => return ("unavailable", serde_json::json!({"bus": "not running", "note": "org.a11y.Bus has no owner; a11y_enable or a11y_tree start it on demand"})),
        Ok(Err(e)) => return ("error", serde_json::json!({"error": format!("NameHasOwner: {e}")})),
        Err(_) => return ("error", serde_json::json!({"error": "NameHasOwner timed out after 3s"})),
    }
    let st = match tokio::time::timeout(Duration::from_secs(3), status(session_bus)).await {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return ("error", serde_json::json!({"error": e})),
        Err(_) => return ("error", serde_json::json!({"error": "reading org.a11y.Status timed out after 3s"})),
    };
    let conn = match tokio::time::timeout(Duration::from_secs(5), a11y_connection(session_bus)).await {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => return ("error", serde_json::json!({"is_enabled": st["is_enabled"], "error": e})),
        Err(_) => return ("error", serde_json::json!({"is_enabled": st["is_enabled"], "error": "connecting to the AT-SPI bus timed out after 5s"})),
    };
    let apps: Option<usize> = async {
        let proxy = Proxy::new(&conn, "org.a11y.atspi.Registry", "/org/a11y/atspi/accessible/root", "org.a11y.atspi.Accessible").await.ok()?;
        let children: Vec<(String, OwnedObjectPath)> = tokio::time::timeout(Duration::from_secs(3), proxy.call("GetChildren", &())).await.ok()?.ok()?;
        Some(children.len())
    }.await;
    let enabled = st["is_enabled"].as_bool();
    let status = if enabled == Some(false) { "disabled" } else { "available" };
    (status, serde_json::json!({
        "is_enabled": enabled,
        "screen_reader_enabled": st["screen_reader_enabled"],
        "applications": apps,
        "note": if apps.is_none() { "AT-SPI registry did not answer; the tree may be empty until an app registers" } else { "a11y_tree lists these applications" },
    }))
}

fn split_ref(ref_id: &str) -> Result<(String, String), String> {
    ref_id.split_once('|').map(|(a, b)| (a.to_string(), b.to_string())).ok_or_else(|| format!("invalid accessibility ref '{ref_id}' (expected 'bus|/path' from a11y_tree/a11y_find)"))
}

async fn get_name(conn: &Connection, bus: &str, path: &str) -> Option<String> {
    let proxy = Proxy::new(conn, BusName::try_from(bus).ok()?, ObjectPath::try_from(path).ok()?, InterfaceName::try_from("org.freedesktop.DBus.Properties").ok()?).await.ok()?;
    let v: OwnedValue = proxy.call("Get", &("org.a11y.atspi.Accessible", "Name")).await.ok()?;
    String::try_from(v).ok()
}
async fn call_u32(conn: &Connection, bus: &str, path: &str, method: &str) -> Option<u32> {
    let proxy = Proxy::new(conn, BusName::try_from(bus).ok()?, ObjectPath::try_from(path).ok()?, InterfaceName::try_from("org.a11y.atspi.Accessible").ok()?).await.ok()?;
    proxy.call(method, &()).await.ok()
}

pub struct TreeOpts { pub max_nodes: usize, pub max_depth: usize, pub timeout: Duration, pub include_text: bool }

pub async fn fetch_tree(conn: &Connection, opts: &TreeOpts) -> Result<A11yNode, String> {
    let started = Instant::now();
    let mut count = 0usize;
    // Root children are applications.
    let root_bus = "org.a11y.atspi.Registry".to_string();
    let root_path = "/org/a11y/atspi/accessible/root".to_string();
    // Hard outer bound: per-node checks between calls cannot interrupt a
    // single hung D-Bus call, so wrap the whole walk in a timeout.
    let budget = opts.timeout + Duration::from_secs(2);
    tokio::time::timeout(budget, build_node(conn, &root_bus, &root_path, 0, opts, &mut count, started)).await
        .map_err(|_| format!("accessibility tree timed out after {budget:?} (an application on the a11y bus is likely hung)"))?
}

fn build_node<'a>(conn: &'a Connection, bus: &'a str, path: &'a str, depth: usize, opts: &'a TreeOpts, count: &'a mut usize, started: Instant) -> Pin<Box<dyn Future<Output = Result<A11yNode, String>> + Send + 'a>> {
    Box::pin(async move {
        *count += 1;
        let mut node = A11yNode { ref_id: format!("{bus}|{path}"), name: get_name(conn, bus, path).await,
            role: None, role_id: None, states: vec![], text: None, actions: vec![], extents_screen: None, extents_note: None, children: vec![], error: None };
        node.role_id = call_u32(conn, bus, path, "GetRole").await;
        if let Ok(proxy) = Proxy::new(conn, BusName::try_from(bus).map_err(|e| e.to_string())?, ObjectPath::try_from(path).map_err(|e| e.to_string())?, InterfaceName::try_from("org.a11y.atspi.Accessible").map_err(|e| e.to_string())?).await {
            node.role = proxy.call::<_, _, String>("GetRoleName", &()).await.ok();
            node.states = proxy.call::<_, _, Vec<u32>>("GetStates", &()).await.unwrap_or_default();
        }
        // Actions
        if let Ok(proxy) = Proxy::new(conn, bus.to_string(), path.to_string(), "org.a11y.atspi.Action").await {
            if let Ok(actions) = proxy.call::<_, _, Vec<(String, String, String)>>("GetActions", &()).await {
                node.actions = actions.into_iter().map(|a| a.0).collect();
            }
        }
        // Text (bounded; protected values are whatever the app exposes — callers must treat as sensitive)
        if opts.include_text {
            if let Ok(proxy) = Proxy::new(conn, bus.to_string(), path.to_string(), "org.a11y.atspi.Text").await {
                let count_chars: Result<i32, _> = proxy.get_property("CharacterCount").await;
                if let Ok(n) = count_chars { if n > 0 && n <= 4096 { node.text = proxy.call::<_, _, String>("GetText", &(0i32, n)).await.ok(); } }
            }
        }
        // Extents: coordinate type 1 = screen. Failure is reported, not guessed.
        // Note: Proxy::new does not verify the interface exists, so a proxy
        // error here is a construction failure, not proof of absence.
        match Proxy::new(conn, bus.to_string(), path.to_string(), "org.a11y.atspi.Component").await {
            Ok(proxy) => match proxy.call::<_, _, (i32, i32, i32, i32)>("GetExtents", &(1u32)).await {
                Ok((x, y, w, h)) if w > 0 && h > 0 => node.extents_screen = Some([x, y, w, h]),
                Ok(_) => node.extents_note = Some("component reported empty extents; not usable as pointer geometry".into()),
                Err(e) => node.extents_note = Some(format!("extents unavailable: {e}")),
            },
            Err(e) => node.extents_note = Some(format!("extents unavailable (Component proxy could not be created): {e}")),
        }
        if depth < opts.max_depth && *count < opts.max_nodes && started.elapsed() < opts.timeout {
            if let Ok(proxy) = Proxy::new(conn, bus.to_string(), path.to_string(), "org.a11y.atspi.Accessible").await {
                if let Ok(children) = proxy.call::<_, _, Vec<(String, OwnedObjectPath)>>("GetChildren", &()).await {
                    for (cbus, cpath) in children {
                        if *count >= opts.max_nodes || started.elapsed() >= opts.timeout { node.error = Some(format!("truncated at {} nodes / {:?} timeout", opts.max_nodes, opts.timeout)); break; }
                        match build_node(conn, &cbus, cpath.as_str(), depth + 1, opts, count, started).await {
                            Ok(child) => node.children.push(child),
                            Err(e) => node.children.push(A11yNode { ref_id: format!("{cbus}|{cpath}"), name: None, role: None, role_id: None, states: vec![], text: None, actions: vec![], extents_screen: None, extents_note: None, children: vec![], error: Some(e) }),
                        }
                    }
                }
            }
        }
        Ok(node)
    })
}

pub fn flatten(node: &A11yNode, out: &mut Vec<A11yNode>) {
    let mut n = node.clone(); let children = std::mem::take(&mut n.children);
    out.push(n);
    for c in children { flatten(&c, out); }
}

pub async fn do_action(conn: &Connection, ref_id: &str, action: &str) -> Result<serde_json::Value, String> {
    let (bus, path) = split_ref(ref_id)?;
    let proxy = Proxy::new(conn, bus.clone(), path.clone(), "org.a11y.atspi.Action").await.map_err(|e| e.to_string())?;
    let actions: Vec<(String, String, String)> = proxy.call("GetActions", &()).await.map_err(|e| format!("element no longer exposes actions (stale ref?): {e}"))?;
    let idx = actions.iter().position(|a| a.0 == action).ok_or_else(|| format!("action '{action}' not exposed by this element; available: {:?}", actions.iter().map(|a| &a.0).collect::<Vec<_>>()))?;
    let ok: bool = proxy.call("DoAction", &(idx as i32)).await.map_err(|e| format!("DoAction: {e}"))?;
    Ok(serde_json::json!({"invoked": action, "index": idx, "result": ok, "mode": "semantic (not a physical pointer/keyboard event)"}))
}

pub async fn set_value(conn: &Connection, ref_id: &str, value: &serde_json::Value) -> Result<serde_json::Value, String> {
    let (bus, path) = split_ref(ref_id)?;
    // EditableText first for strings.
    if let Some(text) = value.as_str() {
        match Proxy::new(conn, bus.clone(), path.clone(), "org.a11y.atspi.EditableText").await {
            Ok(proxy) => {
                let ok: bool = proxy.call("SetTextContents", &(text)).await.map_err(|e| format!("SetTextContents failed (element may expose no EditableText interface, or it rejected the value): {e}"))?;
                // Read back through Text.
                let readback = read_text_of(conn, &bus, &path).await;
                let verified = readback.as_deref() == Some(text);
                return Ok(serde_json::json!({"set_via": "EditableText.SetTextContents", "call_ok": ok, "readback": readback, "verification": if verified { "verified" } else if readback.is_some() { "mismatch" } else { "unavailable" }}));
            }
            Err(e) => return Err(format!("could not create an EditableText proxy for this element: {e}")),
        }
    }
    if let Some(num) = value.as_f64() {
        let proxy = Proxy::new(conn, bus.clone(), path.clone(), "org.a11y.atspi.Value").await.map_err(|e| e.to_string())?;
        proxy.call::<_, _, ()>("SetCurrentValue", &(num)).await.map_err(|e| format!("SetCurrentValue: {e}"))?;
        let readback: Result<f64, _> = proxy.get_property("CurrentValue").await;
        let verification = match &readback { Ok(v) if (*v - num).abs() < 1e-9 => "verified", Ok(_) => "mismatch", Err(_) => "unavailable" };
        return Ok(serde_json::json!({"set_via": "Value.SetCurrentValue", "readback": readback.ok(), "verification": verification }));
    }
    Err("value must be a string (editable text) or number (Value interface)".into())
}
pub async fn read_text_of(conn: &Connection, bus: &str, path: &str) -> Option<String> {
    let proxy = Proxy::new(conn, bus.to_string(), path.to_string(), "org.a11y.atspi.Text").await.ok()?;
    let n: i32 = proxy.get_property("CharacterCount").await.ok()?;
    if n <= 0 { return Some(String::new()); }
    proxy.call::<_, _, String>("GetText", &(0i32, n.min(8192))).await.ok()
}
