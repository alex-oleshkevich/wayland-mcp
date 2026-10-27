//! XDG Desktop Portal capability probing (read-only). Consent sessions are only
//! started by an explicit input action on a portal-only target; doctor never prompts.
use zbus::Proxy;

pub async fn probe(session_bus: &Option<String>) -> serde_json::Value {
    let conn = match crate::a11y::session_connection(session_bus).await {
        Ok(c) => c,
        Err(e) => return serde_json::json!({"available": false, "error": e}),
    };
    let mut out = serde_json::json!({"available": true});
    if let Ok(proxy) = Proxy::new(&conn, "org.freedesktop.portal.Desktop", "/org/freedesktop/portal/desktop", "org.freedesktop.portal.RemoteDesktop").await {
        let devices: Result<u32, _> = proxy.get_property("AvailableDeviceTypes").await;
        out["remote_desktop_available_device_types"] = match devices { Ok(d) => serde_json::json!({"value": d, "keyboard": d & 1 != 0, "pointer": d & 2 != 0, "touchscreen": d & 4 != 0}), Err(e) => serde_json::json!({"error": e.to_string()}) };
    } else { out["remote_desktop"] = serde_json::json!("interface unavailable"); }
    if let Ok(proxy) = Proxy::new(&conn, "org.freedesktop.portal.Desktop", "/org/freedesktop/portal/desktop", "org.freedesktop.portal.ScreenCast").await {
        let sources: Result<u32, _> = proxy.get_property("AvailableSourceTypes").await;
        out["screencast_available_source_types"] = match sources { Ok(s) => serde_json::json!(s), Err(e) => serde_json::json!({"error": e.to_string()}) };
    } else { out["screencast"] = serde_json::json!("interface unavailable"); }
    // Proxy::new does not verify the interface exists (lazy proxy), so read a
    // real property: portal interfaces expose a `version` property. A failed
    // read means the interface is not actually implemented by the portal.
    match Proxy::new(&conn, "org.freedesktop.portal.Desktop", "/org/freedesktop/portal/desktop", "org.freedesktop.portal.Clipboard").await {
        Ok(proxy) => {
            let version: Result<u32, _> = proxy.get_property("version").await;
            out["clipboard_portal"] = match version {
                Ok(v) => serde_json::json!({"interface": "present", "version": v, "note": "requires a remote-desktop session to use"}),
                Err(e) => serde_json::json!({"interface": "unavailable", "error": e.to_string()}),
            };
        }
        Err(e) => { out["clipboard_portal"] = serde_json::json!({"interface": "unavailable", "error": e.to_string()}); }
    }
    out
}

/// Touch dispatch decision. Returns Ok(mapping description) only for a verified,
/// target-scoped touch backend. Host uinput is NEVER used for a private target.
pub fn touch_mapping(target_kind: &str) -> Result<String, String> {
    if target_kind == "sandbox" {
        return Err("touch on the private niri target is unsupported: no target-scoped touch backend is available, and host uinput events are never injected into an isolated target".into());
    }
    if let Ok(cfg) = std::env::var("WAYLAND_MCP_TOUCH_UINPUT") {
        // cfg format: /dev/input/eventN:WIDTHxHEIGHT@OUTPUT — mapping must be verified by the operator.
        if cfg.contains(':') { return Ok(format!("configured uinput touchscreen mapping: {cfg}")); }
        return Err(format!("WAYLAND_MCP_TOUCH_UINPUT='{cfg}' is not a verified mapping (expected /dev/input/eventN:WIDTHxHEIGHT@OUTPUT); refusing best-effort touch"));
    }
    Err("touch unsupported on this target: no granted portal touch capability and no explicitly configured, verified uinput touchscreen mapping (WAYLAND_MCP_TOUCH_UINPUT). Synthetic touch is not a touchpad gesture and is never faked via pointer events.".into())
}
