//! XDG Desktop Portal capability probing (read-only). Consent sessions are only
//! started by an explicit input action on a portal-only target; doctor never prompts.
use zbus::Proxy;

/// Read-only portal check for `doctor`. Returns `(status, detail)`:
/// - `available`: ScreenCast and RemoteDesktop work, with keyboard or pointer.
/// - `capture-only`: ScreenCast works but RemoteDesktop offers no input devices.
/// - `unavailable`: no portal service, or no usable ScreenCast.
/// The portal service is not D-Bus-activated here: if nothing owns
/// `org.freedesktop.portal.Desktop`, that is reported instead of starting it.
pub async fn probe(session_bus: &Option<String>) -> (&'static str, serde_json::Value) {
    let conn = match crate::a11y::session_connection(session_bus).await {
        Ok(c) => c,
        Err(e) => return ("unavailable", serde_json::json!({"error": e})),
    };
    let owned = match (zbus::fdo::DBusProxy::new(&conn).await, zbus::names::BusName::try_from("org.freedesktop.portal.Desktop")) {
        (Ok(dbus), Ok(name)) => tokio::time::timeout(std::time::Duration::from_secs(3), dbus.name_has_owner(name)).await,
        (Err(e), _) => return ("error", serde_json::json!({"error": format!("D-Bus proxy: {e}")})),
        (_, Err(e)) => return ("error", serde_json::json!({"error": e.to_string()})),
    };
    match owned {
        Ok(Ok(true)) => {}
        Ok(Ok(false)) => return ("unavailable", serde_json::json!({"service": "not running"})),
        Ok(Err(e)) => return ("error", serde_json::json!({"error": format!("NameHasOwner: {e}")})),
        Err(_) => return ("error", serde_json::json!({"error": "NameHasOwner timed out after 3s"})),
    }
    let mut out = serde_json::json!({});
    // Proxy::new is lazy and does not prove an interface exists; reading a
    // property does. A failed read means the interface is not implemented.
    let mut devices = 0u32;
    match Proxy::new(&conn, "org.freedesktop.portal.Desktop", "/org/freedesktop/portal/desktop", "org.freedesktop.portal.RemoteDesktop").await {
        Ok(proxy) => match proxy.get_property::<u32>("AvailableDeviceTypes").await {
            Ok(d) => { devices = d; out["input_devices"] = serde_json::json!([(d & 1 != 0).then_some("keyboard"), (d & 2 != 0).then_some("pointer"), (d & 4 != 0).then_some("touchscreen")].into_iter().flatten().collect::<Vec<_>>()); }
            Err(e) => out["remote_desktop"] = serde_json::json!(format!("unavailable: {e}")),
        },
        Err(e) => out["remote_desktop"] = serde_json::json!(format!("unavailable: {e}")),
    }
    let mut sources = 0u32;
    match Proxy::new(&conn, "org.freedesktop.portal.Desktop", "/org/freedesktop/portal/desktop", "org.freedesktop.portal.ScreenCast").await {
        Ok(proxy) => match proxy.get_property::<u32>("AvailableSourceTypes").await {
            Ok(s) => { sources = s; out["capture_sources"] = serde_json::json!([(s & 1 != 0).then_some("monitor"), (s & 2 != 0).then_some("window"), (s & 4 != 0).then_some("virtual")].into_iter().flatten().collect::<Vec<_>>()); }
            Err(e) => out["screencast"] = serde_json::json!(format!("unavailable: {e}")),
        },
        Err(e) => out["screencast"] = serde_json::json!(format!("unavailable: {e}")),
    }
    out["clipboard"] = match Proxy::new(&conn, "org.freedesktop.portal.Desktop", "/org/freedesktop/portal/desktop", "org.freedesktop.portal.Clipboard").await {
        Ok(proxy) => match proxy.get_property::<u32>("version").await {
            Ok(_) => serde_json::json!("available"),
            Err(_) => serde_json::json!("unavailable"),
        },
        Err(_) => serde_json::json!("unavailable"),
    };
    let status = if sources == 0 { "unavailable" } else if devices & 3 == 0 { "capture-only" } else { "available" };
    (status, out)
}

/// Touch dispatch decision. Returns Ok(mapping description) only for a verified,
/// target-scoped touch backend. Host uinput is NEVER used for a private target.
pub fn touch_mapping(target_kind: &str) -> Result<String, String> {
    if target_kind == "sandbox" {
        return Err("touch on the private niri target is unsupported: no target-scoped touch backend is available, and host uinput events are never injected into an isolated target".into());
    }
    if let Some(cfg) = &crate::config::get().touch_uinput {
        // Format already validated at startup: /dev/input/eventN:WIDTHxHEIGHT@OUTPUT.
        return Ok(format!("configured uinput touchscreen mapping: {cfg}"));
    }
    Err("touch unsupported on this target: no granted portal touch capability and no explicitly configured, verified uinput touchscreen mapping (--touch-uinput). Synthetic touch is not a touchpad gesture and is never faked via pointer events.".into())
}
