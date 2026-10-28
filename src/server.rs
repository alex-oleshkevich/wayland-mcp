use crate::{coords::LogicalRect, input::InputDevice, state::Session, wayland};
use base64::{engine::general_purpose::STANDARD, Engine};
use image::{ImageFormat, RgbaImage};
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router, ErrorData, ServerHandler,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, io::Cursor, sync::{Arc, Mutex}};

pub fn now_ms() -> u128 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0) }
pub fn err(msg: impl Into<String>) -> ErrorData { ErrorData::invalid_params(msg.into(), None) }
pub fn json_result(v: &serde_json::Value) -> CallToolResult { CallToolResult::success(vec![ContentBlock::text(serde_json::to_string_pretty(v).unwrap_or_default())]) }
pub fn encode_image(img: &RgbaImage, format: &str, max_dimension: Option<u32>) -> Result<(Vec<u8>, String, RgbaImage), String> {
    let mut img = img.clone();
    if let Some(maxd) = max_dimension {
        if maxd > 0 && (img.width() > maxd || img.height() > maxd) {
            // Aspect-preserving: scale so the larger dimension becomes maxd.
            let scale = maxd as f64 / img.width().max(img.height()) as f64;
            let nw = ((img.width() as f64 * scale).round() as u32).max(1);
            let nh = ((img.height() as f64 * scale).round() as u32).max(1);
            img = image::imageops::resize(&img, nw, nh, image::imageops::FilterType::Triangle);
        }
    }
    let (fmt, mime) = if format.eq_ignore_ascii_case("jpeg") || format.eq_ignore_ascii_case("jpg") { (ImageFormat::Jpeg, "image/jpeg") } else { (ImageFormat::Png, "image/png") };
    let mut buf = Vec::new();
    if fmt == ImageFormat::Jpeg {
        // JPEG has no alpha channel; encode from an RGB conversion (stored shot stays RGBA).
        image::DynamicImage::ImageRgba8(img.clone()).to_rgb8().write_to(&mut Cursor::new(&mut buf), fmt).map_err(|e| format!("encode image: {e}"))?;
    } else {
        img.write_to(&mut Cursor::new(&mut buf), fmt).map_err(|e| format!("encode image: {e}"))?;
    }
    Ok((buf, mime.to_string(), img))
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ScreenshotParams {
    /// Output name (e.g. eDP-1); omit for the first output.
    pub output: Option<String>,
    /// png (default) or jpeg
    pub format: Option<String>,
    /// Downscale so neither dimension exceeds this; transform stays exact for the returned image.
    pub max_dimension: Option<u32>,
    /// Logical-rect crop {x,y,width,height} within the captured output.
    pub crop: Option<LogicalRectParam>,
    /// Draw a labelled coordinate grid (logical coordinates) over a copy of the image.
    pub grid: Option<bool>,
    /// Request cursor inclusion; reported unsupported when the backend cannot composite it.
    pub include_cursor: Option<bool>,
    /// Draw numbered marks for OCR-detected text (geometry verified in this shot's pixels). Requires OCR models.
    pub marks: Option<bool>,
}
#[derive(Debug, Deserialize, JsonSchema, Clone, Copy)]
pub struct LogicalRectParam { pub x: i32, pub y: i32, pub width: i32, pub height: i32 }

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GlobalsParams { /// Substring filter for interface names.
    pub filter: Option<String> }

#[derive(Debug, Serialize)]
struct DoctorCheck { name: String, status: String, detail: serde_json::Value }

#[derive(Clone)]
pub struct TraceState { pub id: String, pub path: std::path::PathBuf, pub target_id: String, pub events: Vec<serde_json::Value>, pub started_ms: u128 }
/// Upper bound for any single user-supplied input duration (long-press,
/// drag, dwell, touch hold/swipe/pinch). `wait` keeps its own 60 s bound.
pub const MAX_INPUT_DURATION_MS: u64 = 30_000;
pub fn capped_duration(value: Option<u64>, default: u64) -> u64 { value.unwrap_or(default).min(MAX_INPUT_DURATION_MS) }
/// True for argument keys whose string value is typed/entered text
/// (type_text/clipboard `text`, a11y_set_value `value`, content-ish keys).
pub fn is_text_arg_key(k: &str) -> bool {
    let k = k.to_ascii_lowercase();
    matches!(k.as_str(), "text" | "value" | "content" | "contents") || k.ends_with("_text") || k.ends_with("_content") || k.ends_with("_value")
}
/// Replace any text-entry string in recorded arguments with a character count:
/// typed/clipboard/set-value text (potentially passwords) never lands in trace files.
pub fn redact_trace_args(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => serde_json::Value::Object(map.iter().map(|(k, v)| {
            if is_text_arg_key(k) {
                if let Some(s) = v.as_str() { return (k.clone(), serde_json::json!({"redacted": true, "chars": s.chars().count()})); }
            }
            (k.clone(), redact_trace_args(v))
        }).collect()),
        serde_json::Value::Array(items) => serde_json::Value::Array(items.iter().map(redact_trace_args).collect()),
        other => other.clone(),
    }
}
tokio::task_local! {
    /// Per-task trace suppression (transcribe-l7v.6): set only around a
    /// batch's own dispatch, so concurrent tool calls in other tasks are
    /// recorded normally and nested batch events are never double-recorded.
    static TRACE_SUPPRESSED: std::cell::Cell<bool>;
}
pub fn trace_suppressed() -> bool { TRACE_SUPPRESSED.try_with(|v| v.get()).unwrap_or(false) }
/// Poison-tolerant lock: a panicking holder must not break later callers
/// (notably shutdown_cleanup, transcribe-l7v.13).
pub fn lock_unpoisoned<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> { m.lock().unwrap_or_else(|e| e.into_inner()) }
pub fn point_in_outputs(outputs: &[crate::coords::OutputInfo], x: f64, y: f64) -> bool {
    outputs.iter().any(|o| x >= o.logical.x as f64 && y >= o.logical.y as f64 && x < (o.logical.x + o.logical.width) as f64 && y < (o.logical.y + o.logical.height) as f64)
}
pub fn require_point_in_outputs(outputs: &[crate::coords::OutputInfo], x: f64, y: f64) -> Result<(), ErrorData> {
    if point_in_outputs(outputs, x, y) { Ok(()) } else { Err(err(format!("logical point ({x},{y}) is not inside any output rect {:?}; refusing to let the compositor silently clamp it to an edge", outputs.iter().map(|o| o.logical).collect::<Vec<_>>()))) }
}
#[derive(Clone)]
pub struct Server { pub session: Arc<Mutex<Session>>, pub inputs: Arc<Mutex<HashMap<String, Arc<Mutex<InputDevice>>>>>, pub clipboards: Arc<Mutex<HashMap<String, crate::clipboard::ClipboardHandle>>>, pub sandboxes: Arc<Mutex<HashMap<String, crate::apps::Sandbox>>>, pub apps: Arc<Mutex<HashMap<String, Arc<crate::apps::AppEntry>>>>, pub counters: Arc<Mutex<(u64, u64)>>, pub trace: Arc<Mutex<Option<TraceState>>>, pub recordings: Arc<Mutex<HashMap<String, crate::record::Recording>>>, pub indicator: Arc<Mutex<Option<crate::indicator::IndicatorHandle>>>, pub portals: Arc<Mutex<HashMap<String, crate::portal_session::PortalHandle>>>, pub tool_router: ToolRouter<Self>, pub input_creation: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>, pub portal_creation: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>, pub record_no_indicator: Arc<Mutex<std::collections::HashSet<String>>> }
#[derive(Debug, Deserialize, JsonSchema)]
pub struct PixelParams { pub shot_id: Option<String>, pub x: u32, pub y: u32 }
#[derive(Debug, Deserialize, JsonSchema)]
pub struct DiffParams { pub shot_id: String, pub other_shot_id: Option<String>, pub threshold: Option<u8>, pub include_highlight: Option<bool> }
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadTextParams { pub shot_id: String, pub download_models: Option<bool> }
#[derive(Debug, Deserialize, JsonSchema)]
pub struct FindTextParams { pub shot_id: String, pub query: String, pub regex: Option<bool>, pub download_models: Option<bool> }
#[derive(Debug, Deserialize, JsonSchema)]
pub struct WaitTextParams { pub query: String, pub mode: Option<String>, pub timeout_ms: Option<u64>, pub poll_ms: Option<u64>, pub download_models: Option<bool> }
#[derive(Debug, Deserialize, JsonSchema)]
pub struct AppStateParams { pub max_dimension: Option<u32>, pub max_nodes: Option<usize> }
#[derive(Debug, Deserialize, JsonSchema)]
pub struct A11yEnableParams { pub confirm_desktop_change: Option<bool> }
#[derive(Debug, Deserialize, JsonSchema)]
pub struct A11yTreeParams { pub max_nodes: Option<usize>, pub max_depth: Option<usize>, pub include_text: Option<bool> }
#[derive(Debug, Deserialize, JsonSchema)]
pub struct A11yFindParams { pub name: Option<String>, pub role: Option<String>, pub text: Option<String>, pub timeout_ms: Option<u64> }
#[derive(Debug, Deserialize, JsonSchema)]
pub struct A11yActParams { pub ref_id: String, pub action: String }
#[derive(Debug, Deserialize, JsonSchema)]
pub struct A11ySetValueParams { pub ref_id: String, pub value: serde_json::Value }
#[derive(Debug, Deserialize, JsonSchema)]
pub struct WaitScreenParams { pub mode: Option<String>, pub stable_ms: Option<u64>, pub timeout_ms: Option<u64>, pub poll_ms: Option<u64>, pub output: Option<String>, pub threshold: Option<u8> }
#[derive(Debug, Deserialize, JsonSchema)]
pub struct FrameProbeParams { pub duration_ms: Option<u64>, pub interval_ms: Option<u64>, pub output: Option<String>, pub threshold: Option<u8> }
#[derive(Debug, Deserialize, JsonSchema)]
pub struct BaselineSaveParams { pub name: String, pub overwrite: Option<bool>, pub output: Option<String> }
#[derive(Debug, Deserialize, JsonSchema)]
pub struct RegionParam { pub x: u32, pub y: u32, pub width: u32, pub height: u32 }
#[derive(Debug, Deserialize, JsonSchema)]
pub struct BaselineCompareParams { pub name: String, pub threshold: Option<u8>, pub ignored_regions: Option<Vec<RegionParam>>, pub output: Option<String>, pub include_highlight: Option<bool> }
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ClipboardReadParams { pub mime: Option<String>, pub primary: Option<bool> }
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ClipboardWriteParams { pub text: String, pub target: Option<String> }
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadSelectionParams { /// 'primary' (opt-in, may be stale/other app's) or 'copy' (clipboard-assisted copy, preserving restorable clipboard contents)
    pub via: Option<String> }
#[derive(Debug, Deserialize, JsonSchema)]
pub struct TouchTapParams { #[serde(flatten)] pub point: PointParams, pub hold_ms: Option<u64> }
#[derive(Debug, Deserialize, JsonSchema)]
pub struct TouchSwipeParams { pub points: Vec<PointParams>, pub duration_ms: Option<u64> }
#[derive(Debug, Deserialize, JsonSchema)]
pub struct TouchPinchParams { #[serde(flatten)] pub center: PointParams, pub start_distance: f64, pub end_distance: f64, pub duration_ms: Option<u64> }

pub fn data_dir() -> std::path::PathBuf {
    let base = std::env::var("XDG_DATA_HOME").map(std::path::PathBuf::from).unwrap_or_else(|_| std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".local/share"));
    base.join("wayland-mcp")
}
pub fn sanitize_name(name: &str) -> Result<String, String> {
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.') || name.contains("..") { return Err(format!("invalid artifact name '{name}' (allowed: [A-Za-z0-9._-], no '..')")); }
    Ok(name.to_string())
}
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SandboxStartParams { pub width: Option<u32>, pub height: Option<u32> }
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SandboxStopParams { pub target: Option<String> }
#[derive(Debug, Deserialize, JsonSchema)]
pub struct TargetSelectParams { pub target: String }
#[derive(Debug, Deserialize, JsonSchema)]
pub struct AppLaunchParams { pub argv: Vec<String>, pub cwd: Option<String>, pub env: Option<HashMap<String, String>>, pub target: Option<String>, pub wayland_debug: Option<bool>, pub wait_ms: Option<u64> }
#[derive(Debug, Deserialize, JsonSchema)]
pub struct AppLogsParams { pub app_id: String, pub cursor: Option<u64>, pub max_lines: Option<usize>, pub filter: Option<String> }
#[derive(Debug, Deserialize, JsonSchema)]
pub struct AppStopParams { pub app_id: String, pub timeout_ms: Option<u64> }
#[derive(Debug, Deserialize, JsonSchema)]
pub struct AppIdParams { pub app_id: String }
#[derive(Debug, Deserialize, JsonSchema)]
pub struct BatchStep { pub tool: String, pub arguments: Option<serde_json::Value> }
#[derive(Debug, Deserialize, JsonSchema)]
pub struct BatchParams { pub steps: Vec<BatchStep>, pub stop_on_error: Option<bool> }
#[derive(Debug, Deserialize, JsonSchema)]
pub struct TraceStartParams { pub include_images: Option<bool> }
#[derive(Debug, Deserialize, JsonSchema)]
pub struct TraceReplayParams { pub trace_id: Option<String>, pub path: Option<String>, pub dry_run: Option<bool> }
#[derive(Debug, Deserialize, JsonSchema)]
pub struct RecordStartParams { pub name: Option<String>, pub fps: Option<u32>, pub max_duration_ms: Option<u64>, pub output: Option<String>, pub include_indicator: Option<bool> }
#[derive(Debug, Deserialize, JsonSchema)]
pub struct RecordStopParams { pub recording_id: String }

#[derive(Debug, Deserialize, JsonSchema, Clone)]
pub struct ObserveParams { pub delays_ms: Option<Vec<u64>>, pub max_dimension: Option<u32>, pub include_tree: Option<bool> }
#[derive(Debug, Deserialize, JsonSchema, Clone)]
pub struct PointParams {
    pub x: f64, pub y: f64,
    /// 'screenshot' (requires shot_id) or 'logical' (desktop logical coordinates)
    pub space: Option<String>, pub shot_id: Option<String>, pub target: Option<String>,
    pub observe: Option<ObserveParams>,
}
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ClickParams { #[serde(flatten)] pub point: PointParams, pub button: Option<String>, pub count: Option<u32>, pub modifiers: Option<Vec<String>>, pub long_press_ms: Option<u64> }
#[derive(Debug, Deserialize, JsonSchema)]
pub struct DragParams { pub from: PointParams, pub to: PointParams, pub waypoints: Option<Vec<PointParams>>, pub duration_ms: Option<u64>, pub dwell_ms: Option<u64>, pub button: Option<String>, pub observe: Option<ObserveParams> }
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ScrollParams { pub target: Option<String>, pub x: Option<f64>, pub y: Option<f64>, /// 'screenshot' (requires shot_id) or 'logical'
    pub space: Option<String>, pub shot_id: Option<String>, pub dx: Option<f64>, pub dy: Option<f64>, pub smooth: Option<bool>, pub observe: Option<ObserveParams> }
#[derive(Debug, Deserialize, JsonSchema)]
pub struct MouseButtonParams { pub button: String, pub pressed: bool, pub target: Option<String>, pub observe: Option<ObserveParams> }
#[derive(Debug, Deserialize, JsonSchema)]
pub struct KeyParams { /// e.g. ["ctrl","l"] or ["Return"]; the whole chord is validated before any event is sent.
    pub keys: Vec<String>, pub repeat: Option<u32>, pub target: Option<String>, pub observe: Option<ObserveParams> }
#[derive(Debug, Deserialize, JsonSchema)]
pub struct KeyHoldParams { pub keys: Vec<String>, pub pressed: bool, pub target: Option<String>, pub observe: Option<ObserveParams> }
#[derive(Debug, Deserialize, JsonSchema)]
pub struct TypeTextParams { pub text: String, /// 'keys' (virtual keyboard, default) or 'paste' (clipboard-assisted)
    pub mode: Option<String>, pub target: Option<String>, pub observe: Option<ObserveParams> }
#[derive(Debug, Deserialize, JsonSchema)]
pub struct WaitParams { pub ms: u64 }
#[derive(Debug, Deserialize, JsonSchema)]
pub struct TargetOptParams { pub target: Option<String> }

impl Server {
    pub fn active_target(&self, explicit: Option<&str>) -> Result<crate::wayland::TargetSpec, ErrorData> {
        self.session.lock().unwrap().target(explicit).map_err(err)
    }
    /// Window/focus metadata via the target compositor's IPC where exposed (niri).
    /// Returns None with no fabrication on compositors without such an interface.
    pub fn niri_ipc(&self, target: &crate::wayland::TargetSpec, query: &str) -> Option<serde_json::Value> {
        let dir = target.socket.parent()?;
        // Several niri.*.sock files can exist (stale ones from dead compositors);
        // try each until one answers.
        for e in std::fs::read_dir(dir).ok()?.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if !(name.starts_with("niri.") && name.ends_with(".sock")) { continue; }
            if let Ok(out) = std::process::Command::new("niri").args(["msg", "--json", query]).env("NIRI_SOCKET", e.path()).output() {
                if out.status.success() {
                    if let Ok(v) = serde_json::from_slice(&out.stdout) { return Some(v); }
                }
            }
        }
        None
    }
    pub fn layout(&self, target: &crate::wayland::TargetSpec) -> Result<(Vec<crate::coords::OutputInfo>, (u32, u32), (i32, i32)), ErrorData> {
        let disc = wayland::discover(target).map_err(err)?;
        { let mut s = self.session.lock().unwrap(); s.note_outputs(&target.id, &disc.outputs); }
        if disc.outputs.is_empty() { return Err(err("no outputs on target")); }
        let min_x = disc.outputs.iter().map(|o| o.logical.x).min().unwrap();
        let min_y = disc.outputs.iter().map(|o| o.logical.y).min().unwrap();
        let max_x = disc.outputs.iter().map(|o| o.logical.x + o.logical.width).max().unwrap();
        let max_y = disc.outputs.iter().map(|o| o.logical.y + o.logical.height).max().unwrap();
        Ok((disc.outputs.clone(), ((max_x - min_x).max(1) as u32, (max_y - min_y).max(1) as u32), (min_x, min_y)))
    }
    pub fn input_for(&self, target: &crate::wayland::TargetSpec) -> Result<Arc<Mutex<InputDevice>>, ErrorData> {
        // A cached portal-backed device whose session ended is evicted so this
        // call establishes a fresh (newly consented) session instead of failing forever.
        let live = |this: &Self| -> Option<Arc<Mutex<InputDevice>>> {
            let cached = this.inputs.lock().unwrap().get(&target.id).cloned()?;
            if cached.lock().unwrap().portal_dead_reason().is_some() { this.inputs.lock().unwrap().remove(&target.id); None } else { Some(cached) }
        };
        if let Some(d) = live(self) { return Ok(d); }
        // Serialize creation per target without holding the global map across
        // device creation (which may block on a portal consent dialog).
        let creation = { let mut m = self.input_creation.lock().unwrap(); m.entry(target.id.clone()).or_insert_with(|| Arc::new(Mutex::new(()))).clone() };
        let _guard = creation.lock().unwrap();
        if let Some(d) = live(self) { return Ok(d); }
        let dev = InputDevice::new(target, || self.portal_for(target).map_err(|e| e.message.to_string())).map_err(err)?;
        let arc = Arc::new(Mutex::new(dev));
        self.inputs.lock().unwrap().insert(target.id.clone(), arc.clone());
        Ok(arc)
    }
    /// input_for on a blocking thread: device creation (and any portal consent
    /// wait inside it) must never stall a tokio worker or hold map locks there.
    pub async fn input_for_async(&self, target: &crate::wayland::TargetSpec) -> Result<Arc<Mutex<InputDevice>>, ErrorData> {
        let me = self.clone(); let t = target.clone();
        tokio::task::spawn_blocking(move || me.input_for(&t)).await.map_err(|e| err(e.to_string()))?
    }
    /// Refresh the known layout (discover + note_outputs) BEFORE resolving a
    /// shot, so the stale-shot generation check compares against current
    /// outputs, then resolve. Returns the layout for reuse by the caller.
    pub fn layout_then_resolve(&self, target: &crate::wayland::TargetSpec, p: &PointParams) -> Result<((f64, f64), Vec<crate::coords::OutputInfo>, (u32, u32), (i32, i32)), ErrorData> {
        let (outputs, extent, origin) = self.layout(target)?;
        let (lx, ly) = self.resolve(p, &target.id)?;
        require_point_in_outputs(&outputs, lx, ly)?;
        Ok(((lx, ly), outputs, extent, origin))
    }
    pub fn resolve(&self, p: &PointParams, target_id: &str) -> Result<(f64, f64), ErrorData> {
        let space = p.space.clone().unwrap_or_else(|| if p.shot_id.is_some() { "screenshot".into() } else { "logical".into() });
        let s = self.session.lock().unwrap();
        let (x, y, _) = s.resolve_point(p.shot_id.as_deref(), p.x, p.y, &space, target_id).map_err(err)?;
        Ok((x, y))
    }
    pub async fn with_observe(&self, action: serde_json::Value, observe: &Option<ObserveParams>) -> CallToolResult {
        self.with_observe_label(action, observe, None).await
    }
    /// `with_observe` with an explicit indicator label. The label goes only to
    /// the overlay: it is never added to the result, recorded events or traces.
    pub async fn with_observe_label(&self, mut action: serde_json::Value, observe: &Option<ObserveParams>, label: Option<String>) -> CallToolResult {
        // Observation and indicator belong to the ACTION's target, never to
        // whatever target happens to be active.
        let target_id: Option<String> = action["target"].as_str().map(|s| s.to_string());
        if action.get("action").is_some() { self.indicator_show(&action, target_id.as_deref(), label.as_deref()); }
        let Some(obs) = observe else {
            if let Some(name) = action["action"].as_str().map(|s| s.to_string()) { self.record_event(&name, action.clone(), None); }
            return json_result(&action);
        };
        let delays = obs.delays_ms.clone().unwrap_or_else(|| vec![250]);
        if let Some(last) = delays.iter().max() { tokio::time::sleep(std::time::Duration::from_millis((*last).min(5000))).await; }
        let observed = async {
            let target = self.active_target(target_id.as_deref())?;
            let (transform, img) = self.fresh_capture(&target, None).await?;
            let (bytes, mime, _) = encode_image(&img, "png", obs.max_dimension.or(Some(1024))).map_err(err)?;
            Ok::<(String, CallToolResult), ErrorData>((transform.shot_id.clone(), CallToolResult::success(vec![ContentBlock::image(STANDARD.encode(&bytes), mime), ContentBlock::text(serde_json::to_string_pretty(&serde_json::json!({"shot_id": transform.shot_id, "target": transform.target_id})).unwrap())])))
        }.await;
        match observed {
            Ok((shot_id, res)) => {
                action["observation"] = serde_json::json!({"status": "ok", "delays_ms": delays, "shot_id": shot_id, "include_tree_requested": obs.include_tree.unwrap_or(false), "tree_note": "tree is returned by get_app_state/a11y_tree when accessibility is available"});
                if let Some(name) = action["action"].as_str().map(|s| s.to_string()) { self.record_event(&name, action.clone(), None); }
                let mut contents = vec![ContentBlock::text(serde_json::to_string_pretty(&action).unwrap())];
                if let rmcp::model::CallToolResult { content, .. } = res { contents.extend(content.into_iter().filter(|c| matches!(c, ContentBlock::Image(_)))); }
                CallToolResult::success(contents)
            }
            Err(e) => {
                action["observation"] = serde_json::json!({"status": "failed", "error": format!("{e:?}"), "note": "action was dispatched; observation failure does not mean the action failed and it will not be replayed"});
                if let Some(name) = action["action"].as_str().map(|s| s.to_string()) { self.record_event(&name, action.clone(), None); }
                json_result(&action)
            }
        }
    }
    pub fn shot_image(&self, shot_id: &str) -> Result<(crate::coords::ShotTransform, Arc<RgbaImage>), ErrorData> {
        let s = self.session.lock().unwrap();
        let shot = s.get_shot(shot_id).map_err(err)?;
        Ok((shot.transform.clone(), shot.image.clone()))
    }
    pub async fn fresh_capture(&self, target: &crate::wayland::TargetSpec, output: Option<String>) -> Result<(crate::coords::ShotTransform, Arc<RgbaImage>), ErrorData> {
        self.indicator_hide();
        // Sync the layout generation with this capture so later layout checks
        // compare against the layout the shot was actually taken in.
        if let Ok(disc) = wayland::discover(target) { let mut s = self.session.lock().unwrap(); s.note_outputs(&target.id, &disc.outputs); }
        let cap = self.capture_any(target, output, false).await?;
        let img = RgbaImage::from_raw(cap.width, cap.height, cap.rgba).ok_or_else(|| err("capture buffer size mismatch"))?;
        let transform = crate::coords::ShotTransform { shot_id: String::new(), target_id: target.id.clone(), output: cap.output.name.clone(),
            image_width: img.width(), image_height: img.height(), crop_logical: cap.output.logical, crop_exact: None, layout_generation: 0, captured_at_ms: now_ms() };
        let stored = { let mut s = self.session.lock().unwrap(); s.store_shot(transform, img.clone()) };
        Ok((stored, Arc::new(img)))
    }
    /// Portal touch routing: Some only when the portal path is explicitly enabled
    /// for a non-sandbox target. Returns (handle, stream node, stream-local x, y).
    pub fn portal_touch_stream(&self, target: &crate::wayland::TargetSpec, lx: f64, ly: f64) -> Option<Result<(crate::portal_session::PortalHandle, u32, f64, f64), String>> {
        if target.kind == "sandbox" || !crate::portal_session::portal_enabled() { return None; }
        Some((|| {
            let handle = self.portal_for(target).map_err(|e| format!("{e:?}"))?;
            let (outputs, _, _) = self.layout(target).map_err(|e| format!("{e:?}"))?;
            // No silent fallback to another output: a point on no output is an error.
            let out = outputs.iter().find(|o| lx >= o.logical.x as f64 && ly >= o.logical.y as f64 && lx < (o.logical.x + o.logical.width) as f64 && ly < (o.logical.y + o.logical.height) as f64)
                .ok_or_else(|| format!("touch point ({lx},{ly}) is not inside any output on target '{}'", target.id))?;
            let stream = handle.stream_for_output(out.logical.x, out.logical.y, out.logical.width, out.logical.height)
                .ok_or_else(|| format!("portal session has no stream for output '{}' (logical {:?}); streams available: {:?}", out.name, out.logical, handle.streams.iter().map(|s| (s.node_id, s.position, s.size)).collect::<Vec<_>>()))?;
            Ok((handle.clone(), stream.node_id, lx - stream.position.0 as f64, ly - stream.position.1 as f64))
        })())
    }
    /// portal_touch_stream on a blocking thread: portal session establishment
    /// may wait up to the consent timeout and must never stall a tokio worker
    /// (transcribe-l7v.11), exactly like portal_for_async/input_for_async.
    pub async fn portal_touch_stream_async(&self, target: &crate::wayland::TargetSpec, lx: f64, ly: f64) -> Option<Result<(crate::portal_session::PortalHandle, u32, f64, f64), String>> {
        if target.kind == "sandbox" || !crate::portal_session::portal_enabled() { return None; }
        let me = self.clone(); let t = target.clone();
        match tokio::task::spawn_blocking(move || me.portal_touch_stream(&t, lx, ly)).await {
            Ok(v) => v,
            Err(e) => Some(Err(e.to_string())),
        }
    }
    pub fn portal_for(&self, target: &crate::wayland::TargetSpec) -> Result<crate::portal_session::PortalHandle, ErrorData> {
        if target.kind == "sandbox" { return Err(err("portal sessions belong to the host desktop and are never used for a private sandbox target")); }
        // Clone out under a short lock: holding the map guard across the
        // eviction below would self-deadlock (std Mutex is not reentrant).
        let existing = self.portals.lock().unwrap().get(&target.id).cloned();
        if let Some(h) = existing {
            // Evict a dead session (revoked/closed): the next explicit call
            // establishes a fresh, newly consented session instead of
            // serving stale frames/errors from the cached handle forever.
            if h.dead_reason().is_some() { self.portals.lock().unwrap().remove(&target.id); } else { return Ok(h); }
        }
        // Serialize establishment per target without holding the global map
        // lock across the (potentially minutes-long) consent dialog.
        let creation = { let mut m = self.portal_creation.lock().unwrap(); m.entry(target.id.clone()).or_insert_with(|| Arc::new(Mutex::new(()))).clone() };
        let _guard = creation.lock().unwrap();
        if let Some(h) = self.portals.lock().unwrap().get(&target.id) { return Ok(h.clone()); }
        let h = crate::portal_session::PortalHandle::start(target).map_err(err)?;
        self.portals.lock().unwrap().insert(target.id.clone(), h.clone());
        Ok(h)
    }
    pub async fn portal_for_async(&self, target: &crate::wayland::TargetSpec) -> Result<crate::portal_session::PortalHandle, ErrorData> {
        let me = self.clone(); let t = target.clone();
        tokio::task::spawn_blocking(move || me.portal_for(&t)).await.map_err(|e| err(e.to_string()))?
    }
    /// Capture via the native backend; when the compositor doesn't offer it and
    /// the operator enabled the portal path, capture via the consented portal
    /// session instead. Portal refusal is terminal (propagated, never bypassed).
    pub async fn capture_any(&self, target: &crate::wayland::TargetSpec, output_name: Option<String>, cursor: bool) -> Result<crate::wayland::Captured, ErrorData> {
        let native = {
            let t = target.clone(); let o = output_name.clone();
            tokio::task::spawn_blocking(move || wayland::capture_output_cursor(&t, o.as_deref(), cursor)).await.map_err(|e| err(e.to_string()))?
        };
        match native {
            Ok(c) => Ok(c),
            Err(e) => {
                if !e.contains("not advertised") || target.kind == "sandbox" { return Err(err(e)); }
                if !crate::portal_session::portal_enabled() {
                    return Err(err(format!("{e}. Portal capture is available but not enabled: start the server with --portal (or WAYLAND_MCP_PORTAL=1) to opt into an explicitly consented ScreenCast session (a dialog will ask the user). No other capture mechanism will be substituted.")));
                }
                // Portal capture path: establishment may wait on the consent
                // dialog, so it runs on a blocking thread, not a tokio worker.
                let t = target.clone();
                let handle = self.portal_for_async(target).await?;
                let disc = { let t2 = t.clone(); tokio::task::spawn_blocking(move || wayland::discover(&t2)).await.map_err(|e| err(e.to_string()))? }.map_err(err)?;
                let output = match &output_name {
                    Some(n) => disc.outputs.iter().find(|o| &o.name == n).cloned().ok_or_else(|| err(format!("output {n} not found")))?,
                    None => disc.outputs.first().cloned().ok_or_else(|| err("no outputs on target"))?,
                };
                let stream = handle.stream_for_output(output.logical.x, output.logical.y, output.logical.width, output.logical.height).cloned()
                    .ok_or_else(|| err(format!("portal session returned no matching screen stream for output '{}' (logical {:?}); streams available: {:?}", output.name, output.logical, handle.streams.iter().map(|s| (s.node_id, s.position, s.size)).collect::<Vec<_>>())))?;
                let frame = tokio::task::spawn_blocking(move || handle.capture(stream.node_id)).await.map_err(|e| err(e.to_string()))?.map_err(err)?;
                // Portal ScreenCast always embeds the cursor.
                Ok(crate::wayland::Captured { rgba: frame.rgba, width: frame.width, height: frame.height, output, cursor_composited: true })
            }
        }
    }
    pub async fn a11y_conn(&self, target: &crate::wayland::TargetSpec) -> Result<zbus::Connection, ErrorData> {
        crate::a11y::a11y_connection(&target.session_bus).await.map_err(err)
    }
    pub async fn ocr_words(&self, shot_id: &str, download: bool) -> Result<(crate::coords::ShotTransform, Vec<crate::ocr::OcrWord>), ErrorData> {
        let (transform, img) = self.shot_image(shot_id)?;
        let words = tokio::task::spawn_blocking(move || crate::ocr::run_ocr(&img, download)).await.map_err(|e| err(e.to_string()))?.map_err(err)?;
        Ok((transform, words))
    }
    pub async fn clipboard_for_async(&self, target: &crate::wayland::TargetSpec) -> Result<crate::clipboard::ClipboardHandle, ErrorData> {
        let me = self.clone(); let t = target.clone();
        tokio::task::spawn_blocking(move || me.clipboard_for(&t)).await.map_err(|e| err(e.to_string()))?
    }
    pub fn clipboard_for(&self, target: &crate::wayland::TargetSpec) -> Result<crate::clipboard::ClipboardHandle, ErrorData> {
        let mut map = self.clipboards.lock().unwrap();
        if let Some(h) = map.get(&target.id) {
            // A cached portal-backed handle whose session ended is evicted so this
            // call creates a fresh (newly consented) handle via portal_for below
            // instead of failing forever on a dead session. DataControl handles
            // report no portal dead reason and are reused as before.
            if h.portal_dead_reason().is_some() { map.remove(&target.id); } else { return Ok(h.clone()); }
        }
        let h = match crate::clipboard::ClipboardHandle::start(target) {
            Ok(h) => h,
            // Portal only when neither data-control protocol exists, the operator opted in, and it is not a private sandbox.
            Err(e) if crate::clipboard::ClipboardHandle::is_unsupported_error(&e) && crate::portal_session::portal_enabled() && target.kind != "sandbox" => {
                drop(map);
                let portal = self.portal_for(target)?;
                if let Some(why) = &portal.clipboard_unavailable { return Err(err(format!("{e}; portal clipboard unavailable: {why}"))); }
                let h = crate::clipboard::ClipboardHandle::portal(portal, target.id.clone());
                self.clipboards.lock().unwrap().insert(target.id.clone(), h.clone());
                return Ok(h);
            }
            Err(e) => return Err(err(e)),
        };
        map.insert(target.id.clone(), h.clone());
        Ok(h)
    }
    /// Convert a logical point to configured touchscreen device coordinates, verifying it lands on the mapped output.
    pub fn touch_point(&self, target: &crate::wayland::TargetSpec, lx: f64, ly: f64) -> Result<(crate::touch::TouchDevice, i32, i32), ErrorData> {
        let mapping_desc = crate::portal::touch_mapping(&target.kind).map_err(err)?;
        // touch_mapping returns either an error or a description; the machine-readable mapping is the same configured value.
        let cfg = crate::config::get().touch_uinput.clone().ok_or_else(|| err(mapping_desc.clone()))?;
        let mapping = crate::touch::parse_mapping(&cfg).map_err(err)?;
        let (outputs, _, _) = self.layout(target)?;
        let out = outputs.iter().find(|o| o.name == mapping.output).ok_or_else(|| err(format!("touch mapping output '{}' not present on target; mapping not verified", mapping.output)))?;
        if lx < out.logical.x as f64 || ly < out.logical.y as f64 || lx >= (out.logical.x + out.logical.width) as f64 || ly >= (out.logical.y + out.logical.height) as f64 {
            return Err(err(format!("touch point ({lx},{ly}) is outside mapped output {}", mapping.output)));
        }
        let dx = ((lx - out.logical.x as f64) / out.logical.width as f64 * mapping.max_x as f64) as i32;
        let dy = ((ly - out.logical.y as f64) / out.logical.height as f64 * mapping.max_y as f64) as i32;
        let dev = crate::touch::TouchDevice::open(mapping).map_err(err)?;
        Ok((dev, dx, dy))
    }
    /// Record a server-issued action into the active trace (opt-in only; typed
    /// text is redacted to a character count). The event's target is the
    /// action's real target (from its summary/arguments), not merely the
    /// target that was active at trace_start. While a batch is dispatching,
    /// nested tool events are suppressed: the batch_step is the single record.
    /// Target a batch/tool argument set names, in the shapes tools accept.
    pub fn arg_target(args: &serde_json::Value) -> Option<String> {
        args.get("target").and_then(|v| v.as_str()).map(|s| s.to_string())
            .or_else(|| args.pointer("/from/target").and_then(|v| v.as_str()).map(|s| s.to_string()))
            .or_else(|| args.pointer("/to/target").and_then(|v| v.as_str()).map(|s| s.to_string()))
            .or_else(|| args.pointer("/point/target").and_then(|v| v.as_str()).map(|s| s.to_string()))
            .or_else(|| args.pointer("/center/target").and_then(|v| v.as_str()).map(|s| s.to_string()))
            .or_else(|| args.pointer("/points/0/target").and_then(|v| v.as_str()).map(|s| s.to_string()))
    }
    pub fn apps_lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<crate::apps::AppEntry>>> { lock_unpoisoned(&self.apps) }
    pub fn record_event(&self, tool: &str, summary: serde_json::Value, full_arguments: Option<serde_json::Value>) {
        if trace_suppressed() && tool != "batch_step" { return; }
        let mut slot = self.trace.lock().unwrap();
        if let Some(trace) = slot.as_mut() {
            let args = full_arguments.map(|a| redact_trace_args(&a));
            let target = summary.get("target").and_then(|v| v.as_str()).map(|s| s.to_string())
                .or_else(|| args.as_ref().and_then(|a| a.get("target").and_then(|v| v.as_str()).map(|s| s.to_string())))
                .or_else(|| args.as_ref().and_then(|a| a.pointer("/from/target").and_then(|v| v.as_str()).map(|s| s.to_string())))
                .or_else(|| args.as_ref().and_then(|a| a.pointer("/point/target").and_then(|v| v.as_str()).map(|s| s.to_string())))
                .or_else(|| args.as_ref().and_then(|a| a.pointer("/center/target").and_then(|v| v.as_str()).map(|s| s.to_string())))
                .or_else(|| args.as_ref().and_then(|a| a.pointer("/points/0/target").and_then(|v| v.as_str()).map(|s| s.to_string())))
                .unwrap_or_else(|| trace.target_id.clone());
            let event = serde_json::json!({"seq": trace.events.len() + 1, "at_ms": now_ms(), "tool": tool, "summary": summary, "arguments": args, "target": target});
            trace.events.push(event.clone());
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&trace.path) {
                use std::io::Write; let _ = writeln!(f, "{}", event);
            }
        }
    }
    /// Release everything this server owns: held input, indicator, clipboards,
    /// recordings, launched apps and sandbox compositors/buses. Called when the
    /// MCP session ends so an exiting server never leaks children it started.
    pub fn shutdown_cleanup(&self) {
        // Close the sandbox-start gate first so an in-flight start aborts and
        // its already-spawned daemons are swept (transcribe-l7v.7, peer fix).
        crate::apps::begin_sandbox_shutdown();
        for dev in lock_unpoisoned(&self.inputs).values() { let _ = dev.lock().unwrap_or_else(|e| e.into_inner()).release_all(); }
        if let Some(h) = lock_unpoisoned(&self.indicator).take() { h.shutdown(); }
        for h in lock_unpoisoned(&self.clipboards).values() { h.shutdown(); }
        for h in lock_unpoisoned(&self.portals).values() { h.shutdown(); }
        for rec in lock_unpoisoned(&self.recordings).values() { rec.stop_flag.store(true, std::sync::atomic::Ordering::Relaxed); }
        let apps: Vec<Arc<crate::apps::AppEntry>> = lock_unpoisoned(&self.apps).values().cloned().collect();
        for app in apps { let _ = crate::apps::stop_app(&app, 800); }
        let sandboxes: Vec<crate::apps::Sandbox> = lock_unpoisoned(&self.sandboxes).drain().map(|(_, v)| v).collect();
        for sb in &sandboxes { crate::apps::stop_sandbox(sb); }
    }
    /// Show the activity indicator for a dispatched action (best-effort; unavailable is reported by doctor, never faked).
    pub fn indicator_show(&self, action: &serde_json::Value, target_id: Option<&str>, label: Option<&str>) {
        if !crate::config::get().indicator { return; }
        // A recording that asked for no indicator (include_indicator:false)
        // must not have one drawn into its frames.
        if !self.record_no_indicator.lock().unwrap().is_empty() { return; }
        let Ok(target) = self.active_target(target_id) else { return };
        let mut slot = self.indicator.lock().unwrap();
        if slot.as_ref().map(|h| &h.target_id) != Some(&target.id) {
            if let Some(old) = slot.take() { old.shutdown(); }
            match crate::indicator::IndicatorHandle::start(&target) {
                Ok(h) => *slot = Some(h),
                Err(_) => return,
            }
        }
        if let Some(h) = slot.as_ref() {
            let (x, y) = match (action["logical"][0].as_f64(), action["logical"][1].as_f64()) { (Some(x), Some(y)) => (Some(x), Some(y)), _ => (None, None) };
            let label = label.map_or_else(|| crate::indicator::action_label(action), str::to_owned);
            h.show(&label, x, y);
        }
    }
    /// Hide the indicator and wait for the acknowledged roundtrip before any pixel capture.
    pub fn indicator_hide(&self) {
        if let Some(h) = self.indicator.lock().unwrap().as_ref() { h.hide_ack(); }
    }
    pub fn batch_allowed(tool: &str) -> bool {
        matches!(tool, "move" | "click" | "drag" | "scroll" | "mouse_button" | "key" | "key_hold" | "type_text" | "release_all" | "wait" | "screenshot" | "touch_tap" | "touch_swipe" | "touch_pinch" | "a11y_act" | "a11y_set_value" | "clipboard_write")
    }
    pub fn validate_batch_step(&self, step: &BatchStep) -> Result<(), ErrorData> {
        if step.tool == "batch" { return Err(err("nested batches are not allowed")); }
        if !Self::batch_allowed(&step.tool) { return Err(err(format!("tool '{}' is not allowed inside a batch (no process/D-Bus/sandbox/target operations)", step.tool))); }
        // The --tools/--exclude-tools filter removes dispatch routes; batch
        // dispatch must not bypass it.
        if !self.tool_router.has_route(&step.tool) { return Err(err(format!("tool '{}' is filtered out by --tools/--exclude-tools and cannot be dispatched via batch", step.tool))); }
        let args = step.arguments.clone().unwrap_or(serde_json::json!({}));
        // Full validation: deserialize AND run the same semantic pre-checks the
        // handlers run before dispatching anything (key/button/modifier names,
        // count ranges, wait bound, point targets, shot retention/target).
        fn parse<T: serde::de::DeserializeOwned>(tool: &str, args: serde_json::Value) -> Result<T, ErrorData> {
            serde_json::from_value(args).map_err(|e| err(format!("batch step '{tool}' has invalid arguments: {e}")))
        }
        let check_target = |t: Option<&str>| -> Result<(), ErrorData> {
            if let Some(t) = t { self.active_target(Some(t))?; }
            Ok(())
        };
        let check_point = |p: &PointParams| -> Result<(), ErrorData> {
            check_target(p.target.as_deref())?;
            let space = p.space.as_deref().unwrap_or(if p.shot_id.is_some() { "screenshot" } else { "logical" });
            match space {
                "logical" => Ok(()),
                "screenshot" => {
                    let id = p.shot_id.as_deref().ok_or_else(|| err("screenshot-space point requires shot_id"))?;
                    let target = self.active_target(p.target.as_deref())?;
                    let s = self.session.lock().unwrap();
                    let shot = s.get_shot(id).map_err(err)?;
                    if shot.transform.target_id != target.id { return Err(err(format!("shot {id} belongs to target '{}', step target is '{}'", shot.transform.target_id, target.id))); }
                    let current = s.layout_generation(&target.id);
                    if shot.transform.layout_generation != current { return Err(err(format!("shot {id} is from layout generation {}, current is {}; capture a new screenshot", shot.transform.layout_generation, current))); }
                    Ok(())
                }
                other => Err(err(format!("unknown coordinate space '{other}'"))),
            }
        };
        match step.tool.as_str() {
            "move" => { let p = parse::<PointParams>(&step.tool, args)?; check_point(&p)?; }
            "click" => { let p = parse::<ClickParams>(&step.tool, args)?;
                crate::input::button_code(p.button.as_deref().unwrap_or("left")).map_err(err)?;
                if !(1..=3).contains(&p.count.unwrap_or(1)) { return Err(err("click count must be 1-3")); }
                for m in p.modifiers.as_deref().unwrap_or(&[]) { crate::input::named_key(m).map_err(err)?; if crate::input::modifier_mask(m).is_none() { return Err(err(format!("'{m}' is not a modifier"))); } }
                check_point(&p.point)?; }
            "drag" => { let p = parse::<DragParams>(&step.tool, args)?;
                crate::input::button_code(p.button.as_deref().unwrap_or("left")).map_err(err)?;
                let t0 = p.from.target.as_deref().or(p.to.target.as_deref());
                check_point(&p.from)?; check_point(&p.to)?;
                for w in p.waypoints.as_deref().unwrap_or(&[]) { check_point(w)?; if w.target.as_deref().or(t0) != t0 && w.target.is_some() { return Err(err("drag points name different targets")); } }
                if p.from.target.is_some() && p.to.target.is_some() && p.from.target != p.to.target { return Err(err("drag points name different targets")); } }
            "scroll" => { let p = parse::<ScrollParams>(&step.tool, args)?;
                check_target(p.target.as_deref())?;
                if p.x.is_some() != p.y.is_some() { return Err(err("scroll requires both x and y, or neither")); }
                if let Some(space) = p.space.as_deref() { if space != "logical" && space != "screenshot" { return Err(err(format!("unknown coordinate space '{space}'"))); } }
                if p.x.is_some() { check_point(&PointParams { x: p.x.unwrap(), y: p.y.unwrap(), space: p.space.clone(), shot_id: p.shot_id.clone(), target: p.target.clone(), observe: None })?; } }
            "mouse_button" => { let p = parse::<MouseButtonParams>(&step.tool, args)?; crate::input::button_code(&p.button).map_err(err)?; check_target(p.target.as_deref())?; }
            "key" => { let p = parse::<KeyParams>(&step.tool, args)?; if p.keys.is_empty() { return Err(err("keys must not be empty")); } for k in &p.keys { crate::input::named_key(k).map_err(err)?; } check_target(p.target.as_deref())?; }
            "key_hold" => { let p = parse::<KeyHoldParams>(&step.tool, args)?; if p.keys.is_empty() { return Err(err("keys must not be empty")); } for k in &p.keys { crate::input::named_key(k).map_err(err)?; } check_target(p.target.as_deref())?; }
            "type_text" => { let p = parse::<TypeTextParams>(&step.tool, args)?; match p.mode.as_deref().unwrap_or("keys") { "keys" | "paste" => {}, other => return Err(err(format!("unknown type_text mode '{other}'"))), } check_target(p.target.as_deref())?; }
            "release_all" => { let p = parse::<TargetOptParams>(&step.tool, args)?; check_target(p.target.as_deref())?; }
            "wait" => { let p = parse::<WaitParams>(&step.tool, args)?; if p.ms > 60_000 { return Err(err("wait is bounded to 60000ms")); } }
            "screenshot" => { parse::<ScreenshotParams>(&step.tool, args)?; }
            "touch_tap" => { let p = parse::<TouchTapParams>(&step.tool, args)?; check_point(&p.point)?; }
            "touch_swipe" => { let p = parse::<TouchSwipeParams>(&step.tool, args)?; if p.points.len() < 2 { return Err(err("touch_swipe needs at least 2 points")); }
                let t0 = p.points[0].target.as_deref(); for pt in &p.points { check_point(pt)?; if pt.target.is_some() && t0.is_some() && pt.target.as_deref() != t0 { return Err(err("touch_swipe points name different targets")); } } }
            "touch_pinch" => { let p = parse::<TouchPinchParams>(&step.tool, args)?; check_point(&p.center)?; if !(p.start_distance.is_finite() && p.end_distance.is_finite() && p.start_distance >= 0.0 && p.end_distance >= 0.0) { return Err(err("pinch distances must be finite and non-negative")); } }
            "a11y_act" => { parse::<A11yActParams>(&step.tool, args)?; }
            "a11y_set_value" => { parse::<A11ySetValueParams>(&step.tool, args)?; }
            "clipboard_write" => { let p = parse::<ClipboardWriteParams>(&step.tool, args)?; check_target(p.target.as_deref())?; }
            _ => return Err(err(format!("tool '{}' cannot be validated for batch", step.tool))),
        }
        Ok(())
    }
    pub async fn dispatch_action(&self, tool: &str, args: serde_json::Value) -> Result<CallToolResult, ErrorData> {
        fn from<T: serde::de::DeserializeOwned>(tool: &str, v: serde_json::Value) -> Result<T, ErrorData> { serde_json::from_value(v).map_err(|e| err(format!("invalid arguments for {tool}: {e}"))) }
        match tool {
            "move" => self.r#move(Parameters(from::<PointParams>(tool, args)?)).await,
            "click" => self.click(Parameters(from::<ClickParams>(tool, args)?)).await,
            "drag" => self.drag(Parameters(from::<DragParams>(tool, args)?)).await,
            "scroll" => self.scroll(Parameters(from::<ScrollParams>(tool, args)?)).await,
            "mouse_button" => self.mouse_button(Parameters(from::<MouseButtonParams>(tool, args)?)).await,
            "key" => self.key(Parameters(from::<KeyParams>(tool, args)?)).await,
            "key_hold" => self.key_hold(Parameters(from::<KeyHoldParams>(tool, args)?)).await,
            "type_text" => self.type_text(Parameters(from::<TypeTextParams>(tool, args)?)).await,
            "release_all" => self.release_all(Parameters(from::<TargetOptParams>(tool, args)?)).await,
            "wait" => self.wait(Parameters(from::<WaitParams>(tool, args)?)).await,
            "screenshot" => self.screenshot(Parameters(from::<ScreenshotParams>(tool, args)?)).await,
            "touch_tap" => self.touch_tap(Parameters(from::<TouchTapParams>(tool, args)?)).await,
            "touch_swipe" => self.touch_swipe(Parameters(from::<TouchSwipeParams>(tool, args)?)).await,
            "touch_pinch" => self.touch_pinch(Parameters(from::<TouchPinchParams>(tool, args)?)).await,
            "a11y_act" => self.a11y_act(Parameters(from::<A11yActParams>(tool, args)?)).await,
            "a11y_set_value" => self.a11y_set_value(Parameters(from::<A11ySetValueParams>(tool, args)?)).await,
            "clipboard_write" => self.clipboard_write(Parameters(from::<ClipboardWriteParams>(tool, args)?)).await,
            other => Err(err(format!("tool '{other}' cannot be dispatched here"))),
        }
    }
}

#[tool_router]
impl Server {
    pub fn new(session: Arc<Mutex<Session>>, include: Option<Vec<String>>, exclude: Vec<String>) -> Result<Self, String> {
        let mut router = Self::tool_router();
        if let Some(list) = &include {
            let all: Vec<String> = router.list_all().iter().map(|t| t.name.to_string()).collect();
            for name in list { if !all.contains(name) { return Err(format!("unknown tool in --tools: {name}")); } }
            for name in all { if !list.contains(&name) { router.remove_route(&name); } }
        }
        for name in &exclude {
            if !router.has_route(name) { return Err(format!("unknown tool in --exclude-tools: {name}")); }
            router.remove_route(name);
        }
        Ok(Self { session, inputs: Arc::new(Mutex::new(HashMap::new())), clipboards: Arc::new(Mutex::new(HashMap::new())), sandboxes: Arc::new(Mutex::new(HashMap::new())), apps: Arc::new(Mutex::new(HashMap::new())), counters: Arc::new(Mutex::new((0, 0))), trace: Arc::new(Mutex::new(None)), recordings: Arc::new(Mutex::new(HashMap::new())), indicator: Arc::new(Mutex::new(None)), portals: Arc::new(Mutex::new(HashMap::new())), tool_router: router, input_creation: Arc::new(Mutex::new(HashMap::new())), portal_creation: Arc::new(Mutex::new(HashMap::new())), record_no_indicator: Arc::new(Mutex::new(std::collections::HashSet::new())) })
    }

    /// Name and description of every tool left after --tools/--exclude-tools.
    pub fn tool_list(&self) -> Vec<(String, String)> {
        let mut tools: Vec<(String, String)> = self.tool_router.list_all().iter()
            .map(|t| (t.name.to_string(), t.description.as_deref().unwrap_or("").to_string()))
            .collect();
        tools.sort();
        tools
    }

    /// Read-only diagnostics shared by the `doctor` tool and `wayland-mcp doctor`.
    pub async fn doctor_report(&self) -> Result<serde_json::Value, String> {
        // Clone the target and drop the session lock before the Wayland
        // roundtrip: a hung compositor must not block every other tool.
        let target = { self.session.lock().unwrap().target(None)? };
        let disc = wayland::discover(&target);
        let mut checks: Vec<DoctorCheck> = Vec::new();
        let mut push = |name: &str, status: &str, detail: serde_json::Value| checks.push(DoctorCheck { name: name.into(), status: status.into(), detail });
        push("wayland_socket", if target.socket.exists() { "ok" } else { "missing" }, serde_json::json!({"socket": target.socket, "target": target.id}));
        match &disc {
            Ok(d) => {
                push("registry", "ok", serde_json::json!({"globals": d.globals.len(), "outputs": d.outputs.len()}));
                let has = |iface: &str| d.globals.iter().any(|(_, i, _)| i == iface);
                push("screencopy", if has("zwlr_screencopy_manager_v1") { "available" } else { "unavailable" }, serde_json::json!({"note": "portal ScreenCast is a separate consented path"}));
                push("virtual_pointer", if has("zwlr_virtual_pointer_manager_v1") { "available" } else { "unavailable" }, serde_json::json!({}));
                push("virtual_keyboard", if has("zwp_virtual_keyboard_manager_v1") { "available" } else { "unavailable" }, serde_json::json!({}));
                push("layer_shell", if has("zwlr_layer_shell_v1") { "available" } else { "unavailable" }, serde_json::json!({"note": "activity indicator requires layer-shell"}));
                let data_control = if has("zwlr_data_control_manager_v1") { Some("zwlr_data_control_manager_v1") } else if has("ext_data_control_manager_v1") { Some("ext_data_control_manager_v1") } else { None };
                push("data_control", if data_control.is_some() { "available" } else { "unavailable" }, serde_json::json!({"protocol": data_control, "note": "without data-control the clipboard needs the portal (--portal)"}));
            }
            Err(e) => push("registry", "error", serde_json::json!({"error": e})),
        }
        // Look the binaries up on PATH directly: `which` is not installed everywhere.
        let path_var = std::env::var_os("PATH").unwrap_or_default();
        for bin in ["niri", "ffmpeg", "ffprobe"] {
            let found = std::env::split_paths(&path_var).map(|d| d.join(bin)).find(|p| {
                use std::os::unix::fs::PermissionsExt;
                std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            });
            let used_for = match bin { "niri" => "sandbox targets", "ffmpeg" => "record_start", _ => "recording checks" };
            match found {
                Some(p) => push(bin, "available", serde_json::json!({"path": p})),
                None => push(bin, "missing", serde_json::json!({"needed_for": used_for})),
            }
        }
        // Touch fallback: the server writes to the configured evdev node
        // (--touch-uinput), never to /dev/uinput. Checked with access(), not opened.
        match &crate::config::get().touch_uinput {
            None => push("touch_device", "not-configured", serde_json::json!({"note": "touch uses the portal when it grants a touchscreen; --touch-uinput sets a device fallback"})),
            Some(cfg) => match crate::touch::parse_mapping(cfg) {
                Err(e) => push("touch_device", "error", serde_json::json!({"mapping": cfg, "error": e})),
                Ok(m) => {
                    let c_path = std::ffi::CString::new(m.device.clone()).unwrap_or_default();
                    // SAFETY: valid NUL-terminated path; access() only checks permissions.
                    let writable = unsafe { libc::access(c_path.as_ptr(), libc::W_OK) } == 0;
                    let exists = std::path::Path::new(&m.device).exists();
                    let output_present = disc.as_ref().ok().map(|d| d.outputs.iter().any(|o| o.name == m.output));
                    let status = if !exists { "missing" } else if !writable { "no-permission" } else if output_present == Some(false) { "output-missing" } else { "available" };
                    push("touch_device", status, serde_json::json!({"device": m.device, "output": m.output, "output_present": output_present}));
                }
            },
        }
        let (det, rec) = crate::ocr::model_paths();
        let non_empty = |p: &std::path::Path| std::fs::metadata(p).is_ok_and(|m| m.len() > 0);
        push("ocr_models", if non_empty(&det) && non_empty(&rec) { "installed" } else { "missing" }, serde_json::json!({"dir": crate::ocr::model_dir(), "note": "doctor never downloads models; an OCR call with download_models:true fetches them"}));
        let (a11y_status, a11y_detail) = crate::a11y::probe(&target.session_bus).await;
        push("accessibility", a11y_status, a11y_detail);
        let (portal_status, portal_detail) = crate::portal::probe(&target.session_bus).await;
        push("portal", portal_status, portal_detail);
        push("portal_enabled", if crate::portal_session::portal_enabled() { "yes" } else { "no" }, serde_json::json!({"note": "--portal opts into the consented screen-sharing portal"}));
        let layer_shell = disc.as_ref().is_ok_and(|d| d.globals.iter().any(|(_, i, _)| i == "zwlr_layer_shell_v1"));
        push("indicator", if !crate::config::get().indicator { "off" } else if layer_shell { "on" } else { "unavailable" }, serde_json::json!({"note": if !crate::config::get().indicator { "disabled by --no-indicator" } else if layer_shell { "drawn with layer-shell" } else { "needs layer-shell, which this desktop lacks" }}));
        Ok(serde_json::json!({"target": target.id, "checks": checks}))
    }

    /// Structured read-only diagnostics. Never prompts for consent, downloads models, or changes settings.
    #[tool(description = "Read-only capability diagnostics for the selected target and host tools", annotations(read_only_hint = true, destructive_hint = false, idempotent_hint = true))]
    async fn doctor(&self) -> Result<CallToolResult, ErrorData> {
        Ok(json_result(&self.doctor_report().await.map_err(err)?))
    }

    /// Outputs, logical geometry, scales/transforms, active target and capability summary.
    #[tool(description = "Outputs with logical origins, physical sizes, scales/transforms, active target and backend capabilities. Unknown metadata is null, never fabricated.", annotations(read_only_hint = true, destructive_hint = false, idempotent_hint = true))]
    async fn desktop_info(&self) -> Result<CallToolResult, ErrorData> {
        let (outputs, globals_count, target_id, generation) = {
            let target = { self.session.lock().unwrap().target(None).map_err(err)? };
            let disc = wayland::discover(&target).map_err(err)?;
            let mut s = self.session.lock().unwrap();
            s.note_outputs(&target.id, &disc.outputs);
            (disc.outputs.clone(), disc.globals.len(), target.id.clone(), s.layout_generation(&target.id))
        };
        let caps = {
            let target = { self.session.lock().unwrap().target(None).map_err(err)? };
            let d = wayland::discover(&target).ok();
            d.as_ref().map(|d| {
                let has = |i: &str| d.globals.iter().any(|(_, n, _)| n == i);
                serde_json::json!({
                    "capture_screencopy": has("zwlr_screencopy_manager_v1"),
                    "virtual_pointer": has("zwlr_virtual_pointer_manager_v1"),
                    "virtual_keyboard": has("zwp_virtual_keyboard_manager_v1"),
                    "layer_shell": has("zwlr_layer_shell_v1"),
                    "globals_count": globals_count,
                })
            }).unwrap_or(serde_json::json!({"error": "discovery failed"}))
        };
        let target_for_ipc = { self.session.lock().unwrap().target(None).ok() };
        let (windows, focused) = match &target_for_ipc {
            Some(t) => (self.niri_ipc(t, "windows"), self.niri_ipc(t, "focused-window")),
            None => (None, None),
        };
        Ok(json_result(&serde_json::json!({"target": target_id, "layout_generation": generation, "outputs": outputs, "capabilities": caps, "windows": windows, "focused_window": focused, "windows_note": if windows.is_none() { "window/focus metadata not exposed by this compositor (null, not fabricated)" } else { "via compositor IPC" }})))
    }

    /// Wayland registry globals (interface + version) on the selected target's display.
    #[tool(description = "List registry-advertised Wayland protocol interfaces and versions on the selected target; optional substring filter for presence/absence diagnostics.", annotations(read_only_hint = true, destructive_hint = false, idempotent_hint = true))]
    async fn wayland_globals(&self, Parameters(p): Parameters<GlobalsParams>) -> Result<CallToolResult, ErrorData> {
        let target = { self.session.lock().unwrap().target(None).map_err(err)? };
        let disc = wayland::discover(&target).map_err(err)?;
        let mut globals: Vec<_> = disc.globals.iter().map(|(name, iface, ver)| serde_json::json!({"name": name, "interface": iface, "version": ver})).collect();
        if let Some(f) = &p.filter { globals.retain(|g| g["interface"].as_str().map(|i| i.contains(f.as_str())).unwrap_or(false)); }
        Ok(json_result(&serde_json::json!({"target": target.id, "globals": globals})))
    }

    /// Capture an output (or crop) as an MCP image with an exact shot_id coordinate transform.
    #[tool(description = "Capture a screenshot: output/all geometry via crop, PNG/JPEG, bounded preview, optional logical-coordinate grid. Returns an MCP image plus shot_id and the exact image->logical mapping. An old shot_id is never rebound to a newer capture.", annotations(read_only_hint = true, destructive_hint = false, idempotent_hint = true))]
    async fn screenshot(&self, Parameters(p): Parameters<ScreenshotParams>) -> Result<CallToolResult, ErrorData> {
        let want_cursor = p.include_cursor.unwrap_or(false);
        let target = { self.session.lock().unwrap().target(None).map_err(err)? };
        self.indicator_hide();
        let cap = self.capture_any(&target, p.output.clone(), want_cursor).await?;
        let cursor_composited = cap.cursor_composited;
        let mut img = RgbaImage::from_raw(cap.width, cap.height, cap.rgba).ok_or_else(|| err("capture buffer size mismatch"))?;
        // Sync the layout generation with this capture (full discovery), so the
        // stored shot carries the generation its pixels belong to.
        if let Ok(disc) = wayland::discover(&target) { let mut s = self.session.lock().unwrap(); s.note_outputs(&target.id, &disc.outputs); }
        let mut transform = crate::coords::ShotTransform {
            shot_id: String::new(), target_id: target.id.clone(), output: cap.output.name.clone(),
            image_width: img.width(), image_height: img.height(),
            crop_logical: cap.output.logical, crop_exact: None, layout_generation: 0, captured_at_ms: now_ms(),
        };
        if let Some(c) = p.crop {
            if c.width <= 0 || c.height <= 0 { return Err(err("crop width/height must be positive")); }
            // Map logical crop -> pixels via full-output transform.
            let full = transform.clone();
            // Bound the crop by the output's logical rect first (i64 math:
            // tool-supplied i32 values cannot overflow this).
            if c.x as i64 + c.width as i64 > full.crop_logical.x as i64 + full.crop_logical.width as i64
                || c.y as i64 + c.height as i64 > full.crop_logical.y as i64 + full.crop_logical.height as i64 {
                return Err(err("crop exceeds captured output"));
            }
            // Edge-based mapping: a W-logical-unit crop spans W * (image/logical) pixels.
            full.logical_to_pixel(c.x as f64, c.y as f64).map_err(err)?;
            let px = (((c.x - full.crop_logical.x) as f64) / full.crop_logical.width as f64 * img.width() as f64).round().max(0.0) as u32;
            let py = (((c.y - full.crop_logical.y) as f64) / full.crop_logical.height as f64 * img.height() as f64).round().max(0.0) as u32;
            let pw = ((c.width as f64) / full.crop_logical.width as f64 * img.width() as f64).round().max(1.0) as u32;
            let ph = ((c.height as f64) / full.crop_logical.height as f64 * img.height() as f64).round().max(1.0) as u32;
            // u64 math: px+pw can overflow u32 for extreme crop values.
            if px as u64 + pw as u64 > img.width() as u64 || py as u64 + ph as u64 > img.height() as u64 { return Err(err("crop exceeds captured output")); }
            transform = full.cropped(String::new(), px, py, pw, ph).map_err(err)?;
            img = image::imageops::crop_imm(&img, px, py, pw, ph).to_image();
            transform.image_width = img.width(); transform.image_height = img.height();
        }
        let (bytes, mime, display_img) = encode_image(&img, p.format.as_deref().unwrap_or("png"), p.max_dimension).map_err(err)?;
        // The stored shot IS the returned image: one shot_id always identifies exactly
        // the pixels the caller received. Omit max_dimension for native resolution.
        transform.image_width = display_img.width(); transform.image_height = display_img.height();
        let stored = {
            let mut s = self.session.lock().unwrap();
            s.store_shot(transform, display_img.clone())
        };
        let returned = stored.clone();
        let mut out_img = display_img;
        let mut marks_meta = serde_json::Value::Null;
        if p.grid == Some(true) { draw_grid(&mut out_img, &returned); }
        if p.marks == Some(true) {
            match self.ocr_words(&stored.shot_id, false).await {
                Ok((_, words)) => {
                    let mut list = Vec::new();
                    for (i, w) in words.iter().enumerate() {
                        draw_mark(&mut out_img, w, i + 1);
                        list.push(serde_json::json!({"mark": i + 1, "text": w.text, "box_px": w.box_px, "center_px": w.center_px}));
                    }
                    marks_meta = serde_json::json!(list);
                }
                Err(e) => marks_meta = serde_json::json!({"error": format!("marks unavailable: {e:?}")}),
            }
        }
        let (bytes, mime) = if p.grid == Some(true) || p.marks == Some(true) {
            let (b, m, _) = encode_image(&out_img, p.format.as_deref().unwrap_or("png"), None).map_err(err)?; (b, m)
        } else { (bytes, mime) };
        let meta = serde_json::json!({
            "shot_id": stored.shot_id, "target": target.id, "output": stored.output,
            "transform": returned, "marks": marks_meta, "cursor_composited": cursor_composited,
            "note": "Coordinates for actions: use space='screenshot' with this shot_id and pixel coordinates of the returned image, or space='logical'. Marks come only from OCR text in this shot; unlabeled icons are not identified."
        });
        Ok(CallToolResult::success(vec![
            ContentBlock::image(STANDARD.encode(&bytes), mime),
            ContentBlock::text(serde_json::to_string_pretty(&meta).unwrap()),
        ]))
    }

    /// Move/hover the pointer at screenshot-space or logical coordinates.
    #[tool(name = "move", description = "Move the pointer (hover). Coordinates: space='screenshot' with shot_id, or space='logical'. Validated against the shot transform/layout generation before any input.", annotations(destructive_hint = true))]
    async fn r#move(&self, Parameters(p): Parameters<PointParams>) -> Result<CallToolResult, ErrorData> {
        let target = self.active_target(p.target.as_deref())?;
        let ((lx, ly), _, extent, origin) = self.layout_then_resolve(&target, &p)?;
        let dev = self.input_for_async(&target).await?;
        let route = { let mut d = dev.lock().unwrap(); d.move_absolute(lx - origin.0 as f64, ly - origin.1 as f64, extent.0, extent.1).map_err(err)?; d.route() };
        Ok(self.with_observe(serde_json::json!({"dispatched": true, "action": "move", "target": target.id, "logical": [lx, ly], "route": route}), &p.observe).await)
    }

    /// Click (count 1-3, any button, optional modifiers / long press).
    #[tool(description = "Click left/right/middle/back/forward, count 1-3, optional held modifiers and long-press. Coordinate rules as for move.", annotations(destructive_hint = true))]
    async fn click(&self, Parameters(p): Parameters<ClickParams>) -> Result<CallToolResult, ErrorData> {
        let count = p.count.unwrap_or(1);
        if !(1..=3).contains(&count) { return Err(err("click count must be 1-3")); }
        // Validate button, modifiers and coordinates before dispatching anything.
        let code = crate::input::button_code(p.button.as_deref().unwrap_or("left")).map_err(err)?;
        for m in p.modifiers.as_deref().unwrap_or(&[]) { crate::input::named_key(m).map_err(err)?; if crate::input::modifier_mask(m).is_none() { return Err(err(format!("'{m}' is not a modifier"))); } }
        let target = self.active_target(p.point.target.as_deref())?;
        let ((lx, ly), _, extent, origin) = self.layout_then_resolve(&target, &p.point)?;
        let dev = self.input_for_async(&target).await?;
        let modifiers: Vec<String> = p.modifiers.clone().unwrap_or_default();
        let long_press = capped_duration(p.long_press_ms, 30);
        // The sequence (with its sleeps) runs on a blocking thread; on any
        // error the button and all modifiers pressed here are released.
        tokio::task::spawn_blocking(move || -> Result<(), String> {
            let mut d = dev.lock().unwrap();
            let run = (|| -> Result<(), String> {
                d.move_absolute(lx - origin.0 as f64, ly - origin.1 as f64, extent.0, extent.1)?;
                for m in &modifiers { d.hold_key(m)?; }
                for _ in 0..count {
                    d.button(code, true)?;
                    std::thread::sleep(std::time::Duration::from_millis(long_press));
                    d.button(code, false)?;
                    std::thread::sleep(std::time::Duration::from_millis(70));
                }
                Ok(())
            })();
            if run.is_err() { let _ = d.button(code, false); }
            for m in modifiers.iter().rev() { let _ = d.release_key(m); }
            run
        }).await.map_err(|e| err(e.to_string()))?.map_err(err)?;
        Ok(self.with_observe(serde_json::json!({"dispatched": true, "action": "click", "target": target.id, "logical": [lx, ly], "button": p.button.as_deref().unwrap_or("left"), "count": count, "modifiers": p.modifiers, "long_press_ms": p.long_press_ms}), &p.point.observe).await)
    }

    /// Drag with optional waypoints/dwell/duration; releases its pressed button on error.
    #[tool(description = "Drag from->to with optional waypoints, dwell and duration. On error the button pressed by this drag is released.", annotations(destructive_hint = true))]
    async fn drag(&self, Parameters(p): Parameters<DragParams>) -> Result<CallToolResult, ErrorData> {
        // All points must name the same target (or none): never run a drag
        // silently on the first point's target.
        let named: Vec<Option<String>> = std::iter::once(&p.from).chain(p.waypoints.as_deref().unwrap_or(&[]).iter()).chain(std::iter::once(&p.to)).map(|pt| pt.target.clone()).collect();
        let distinct: std::collections::HashSet<&String> = named.iter().flatten().collect();
        if distinct.len() > 1 { return Err(err(format!("drag points name different targets {distinct:?}; a drag runs on exactly one target"))); }
        let target = self.active_target(named.iter().flatten().next().map(|s| s.as_str()))?;
        let code = crate::input::button_code(p.button.as_deref().unwrap_or("left")).map_err(err)?;
        let (outputs, extent, origin) = self.layout(&target)?;
        let resolve_validated = |pt: &PointParams| -> Result<(f64, f64), ErrorData> { let (x, y) = self.resolve(pt, &target.id)?; require_point_in_outputs(&outputs, x, y)?; Ok((x, y)) };
        let (fx, fy) = resolve_validated(&p.from)?;
        let (tx, ty) = resolve_validated(&p.to)?;
        let mut points = vec![(fx, fy)];
        for w in p.waypoints.as_deref().unwrap_or(&[]) { points.push(resolve_validated(w)?); }
        points.push((tx, ty));
        let dev = self.input_for_async(&target).await?;
        let duration = capped_duration(p.duration_ms, 350);
        let dwell = capped_duration(p.dwell_ms, 0);
        let has_dwell = p.dwell_ms.is_some();
        let n_points = points.len();
        let waypoint_points: Vec<(f64, f64)> = if points.len() > 2 { points[1..points.len() - 1].to_vec() } else { Vec::new() };
        tokio::task::spawn_blocking(move || -> Result<(), String> {
            let mut d = dev.lock().unwrap();
            let run = (|| -> Result<(), String> {
                d.move_absolute(fx - origin.0 as f64, fy - origin.1 as f64, extent.0, extent.1)?;
                d.button(code, true)?;
                let steps = 16u64;
                for seg in points.windows(2) {
                    let (x0, y0) = seg[0]; let (x1, y1) = seg[1];
                    for i in 1..=steps {
                        let t = i as f64 / steps as f64;
                        d.move_absolute(x0 + (x1 - x0) * t - origin.0 as f64, y0 + (y1 - y0) * t - origin.1 as f64, extent.0, extent.1)?;
                        std::thread::sleep(std::time::Duration::from_millis((duration / (steps * (points.len() as u64 - 1).max(1))).max(1)));
                    }
                    if has_dwell { std::thread::sleep(std::time::Duration::from_millis(dwell)); }
                }
                d.button(code, false)?;
                Ok(())
            })();
            if run.is_err() { let _ = d.button(code, false); }
            run
        }).await.map_err(|e| err(e.to_string()))?.map_err(err)?;
        Ok(self.with_observe(serde_json::json!({"dispatched": true, "action": "drag", "target": target.id, "from": [fx, fy], "to": [tx, ty], "waypoints": n_points - 2, "waypoint_points": waypoint_points, "button": p.button.as_deref().unwrap_or("left"), "duration_ms": p.duration_ms, "dwell_ms": p.dwell_ms}), &p.observe).await)
    }

    /// Scroll horizontally/vertically; reports wheel vs smooth granularity.
    #[tool(description = "Scroll (dx/dy in wheel steps by default, smooth=true for continuous). Optional target position. Reports backend granularity.", annotations(destructive_hint = true))]
    async fn scroll(&self, Parameters(p): Parameters<ScrollParams>) -> Result<CallToolResult, ErrorData> {
        // Explicit fields (no serde flatten): target/coords are always honoured.
        let target = self.active_target(p.target.as_deref())?;
        let (dx, dy, smooth) = (p.dx.unwrap_or(0.0), p.dy.unwrap_or(0.0), p.smooth.unwrap_or(false));
        let (resolved, extent, origin) = match (p.x, p.y) {
            (Some(x), Some(y)) => {
                let pt = PointParams { x, y, space: p.space.clone(), shot_id: p.shot_id.clone(), target: p.target.clone(), observe: None };
                let ((lx, ly), _, extent, origin) = self.layout_then_resolve(&target, &pt)?;
                (Some((lx, ly)), extent, origin)
            }
            (None, None) => { let (_, extent, origin) = self.layout(&target)?; (None, extent, origin) }
            _ => return Err(err("scroll requires both x and y, or neither")),
        };
        let dev = self.input_for_async(&target).await?;
        let granularity = tokio::task::spawn_blocking(move || -> Result<String, String> {
            let mut d = dev.lock().unwrap();
            if let Some((lx, ly)) = resolved { d.move_absolute(lx - origin.0 as f64, ly - origin.1 as f64, extent.0, extent.1)?; }
            d.scroll(dx, dy, smooth)
        }).await.map_err(|e| err(e.to_string()))?.map_err(err)?;
        Ok(self.with_observe(serde_json::json!({"dispatched": true, "action": "scroll", "target": target.id, "granularity": granularity, "dx": dx, "dy": dy, "smooth": smooth, "logical": resolved}), &p.observe).await)
    }

    /// Explicit mouse button down/up held across calls (owned by this server).
    #[tool(description = "Press or release a mouse button and hold it across calls. Only buttons pressed by this server are tracked/released.", annotations(destructive_hint = true))]
    async fn mouse_button(&self, Parameters(p): Parameters<MouseButtonParams>) -> Result<CallToolResult, ErrorData> {
        let target = self.active_target(p.target.as_deref())?;
        let code = crate::input::button_code(&p.button).map_err(err)?;
        let dev = self.input_for_async(&target).await?;
        { let mut d = dev.lock().unwrap(); d.button(code, p.pressed).map_err(err)?; }
        Ok(self.with_observe(serde_json::json!({"dispatched": true, "action": "mouse_button", "target": target.id, "button": p.button, "pressed": p.pressed}), &p.observe).await)
    }

    /// Named key chord; whole chord validated before dispatch.
    #[tool(description = "Send a named key chord, e.g. keys=[\"ctrl\",\"l\"] or [\"Return\"]. Entire chord validated before any event; repeats supported.", annotations(destructive_hint = true))]
    async fn key(&self, Parameters(p): Parameters<KeyParams>) -> Result<CallToolResult, ErrorData> {
        if p.keys.is_empty() { return Err(err("keys must not be empty")); }
        // Validate everything first.
        let mut resolved = Vec::new();
        for k in &p.keys { resolved.push((k.clone(), crate::input::named_key(k).map_err(err)?)); }
        let target = self.active_target(p.target.as_deref())?;
        let dev = self.input_for_async(&target).await?;
        let repeat = p.repeat.unwrap_or(1).min(100);
        tokio::task::spawn_blocking(move || -> Result<(), String> {
            let mut d = dev.lock().unwrap();
            // Track everything pressed in this chord so an error midway
            // releases it all (modifiers by name, other keys by evdev code).
            let mut held_mods: Vec<String> = Vec::new();
            let mut held_codes: Vec<u32> = Vec::new();
            let run = (|| -> Result<(), String> {
                for _ in 0..repeat {
                    held_mods.clear(); held_codes.clear();
                    // Modifiers go through hold_key so the virtual keyboard's explicit
                    // modifiers mask is set. Final key taps last; release reversed.
                    let (mods, last) = resolved.split_at(resolved.len().saturating_sub(1));
                    for (name, (code, _)) in mods {
                        if crate::input::modifier_mask(name).is_some() { d.hold_key(name)?; held_mods.push(name.clone()); } else { d.key_event_pub(*code, true)?; held_codes.push(*code); }
                    }
                    if let Some((name, (code, shift))) = last.first() {
                        if resolved.len() == 1 && crate::input::modifier_mask(name).is_some() {
                            d.key_event(*code, true, 0)?; d.key_event(*code, false, 0)?;
                        } else {
                            d.key_event(*code, true, if *shift { 1 } else { 0 })?; d.key_event(*code, false, if *shift { 1 } else { 0 })?;
                        }
                    }
                    for (name, (code, _)) in mods.iter().rev() {
                        if crate::input::modifier_mask(name).is_some() { d.release_key(name)?; held_mods.retain(|m| m != name); } else { d.key_event_pub(*code, false)?; held_codes.retain(|c| c != code); }
                    }
                }
                Ok(())
            })();
            if run.is_err() {
                for code in held_codes.iter().rev() { let _ = d.key_event_pub(*code, false); }
                for name in held_mods.iter().rev() { let _ = d.release_key(name); }
            }
            run
        }).await.map_err(|e| err(e.to_string()))?.map_err(err)?;
        Ok(self.with_observe(serde_json::json!({"dispatched": true, "action": "key", "target": target.id, "keys": p.keys, "repeat": p.repeat}), &p.observe).await)
    }

    /// Hold/release keys or modifiers across calls.
    #[tool(description = "Hold (pressed=true) or release keys/modifiers across calls. Releasing a key this server does not hold is an error. Keymap is never replaced while keys are held.", annotations(destructive_hint = true))]
    async fn key_hold(&self, Parameters(p): Parameters<KeyHoldParams>) -> Result<CallToolResult, ErrorData> {
        for k in &p.keys { crate::input::named_key(k).map_err(err)?; }
        let target = self.active_target(p.target.as_deref())?;
        let dev = self.input_for_async(&target).await?;
        {
            let mut d = dev.lock().unwrap();
            for k in &p.keys {
                if p.pressed { d.hold_key(k).map_err(err)?; } else { d.release_key(k).map_err(err)?; }
            }
        }
        Ok(self.with_observe(serde_json::json!({"dispatched": true, "action": "key_hold", "target": target.id, "keys": p.keys, "pressed": p.pressed}), &p.observe).await)
    }

    /// Unicode text via virtual-keyboard keys, or clipboard paste mode.
    #[tool(description = "Type Unicode text. mode='keys': virtual keyboard (preflighted; partial-dispatch errors never replay). mode='paste': clipboard-assisted, restores prior clipboard when preservable.", annotations(destructive_hint = true))]
    async fn type_text(&self, Parameters(p): Parameters<TypeTextParams>) -> Result<CallToolResult, ErrorData> {
        let mode = p.mode.as_deref().unwrap_or("keys");
        let target = self.active_target(p.target.as_deref())?;
        match mode {
            "keys" => {
                let dev = self.input_for_async(&target).await?;
                let text = p.text.clone();
                let n = tokio::task::spawn_blocking(move || { let mut d = dev.lock().unwrap(); d.type_chars(&text) }).await.map_err(|e| err(e.to_string()))?.map_err(err)?;
                Ok(self.with_observe_label(serde_json::json!({"dispatched": true, "action": "type_text", "target": target.id, "route": "keys", "characters": n}), &p.observe, Some(crate::indicator::typed_label(false, &p.text))).await)
            }
            "paste" => {
                // Resolve the input device BEFORE touching the clipboard:
                // input setup failure (e.g. portal refusal) must never leave
                // the user's clipboard overwritten.
                let dev = self.input_for_async(&target).await?;
                // Preflight: preserve ALL advertised MIME payloads or refuse before overwriting.
                let handle = self.clipboard_for_async(&target).await?;
                let preserved = tokio::task::spawn_blocking({ let h = handle.clone(); move || h.read_all_preservable() }).await.map_err(|e| err(e.to_string()))?.map_err(err)?;
                let was_empty = preserved.is_empty();
                let text_payload = vec![("text/plain;charset=utf-8".to_string(), p.text.clone().into_bytes())];
                // If the tracked write itself fails after (possibly) taking
                // ownership, restore the preserved payloads before returning.
                let marker = match tokio::task::spawn_blocking({ let h = handle.clone(); move || h.write_tracked(text_payload) }).await.map_err(|e| e.to_string()).and_then(|r| r) {
                    Ok(m) => m,
                    Err(e) => {
                        let restore = tokio::task::spawn_blocking({ let h = handle.clone(); move || h.write(preserved) }).await.map_err(|e2| e2.to_string()).and_then(|r| r);
                        return Err(err(format!("clipboard write for paste failed: {e}; prior clipboard restore {}", if restore.is_ok() { "succeeded" } else { "also failed" })));
                    }
                };
                // From here the clipboard is overwritten: EVERY exit path
                // (paste error, wait error, join error) restores it first.
                let paste_result = tokio::task::spawn_blocking(move || -> Result<(), String> { let mut d = dev.lock().unwrap(); d.hold_key("ctrl")?; let r = (|| -> Result<(), String> { d.key_event_pub(47, true)?; d.key_event_pub(47, false) })(); let _ = d.release_key("ctrl"); r }).await.map_err(|e| e.to_string());
                // The app requests the selection asynchronously after ctrl+v: restoring now would serve it the OLD
                // content. Wait (bounded) until our source was asked for data and the transfer finished writing.
                let consumed = if paste_result.as_ref().map(|r| r.is_ok()).unwrap_or(false) { tokio::task::spawn_blocking({ let h = handle.clone(); move || h.wait_consumed(marker, std::time::Duration::from_millis(2000)) }).await.unwrap_or(false) } else { false };
                let restore = tokio::task::spawn_blocking(move || handle.write(preserved)).await.map_err(|e| e.to_string()).and_then(|r| r);
                let restored = restore.is_ok();
                paste_result.map_err(err)?.map_err(err)?;
                if let Err(e) = &restore { return Err(err(format!("paste dispatched but restoring the prior clipboard failed: {e}"))); }
                let note = if !consumed { "no application requested the clipboard within 2000 ms after ctrl+v, so the paste likely did not land (focused widget may not accept paste, or focus is elsewhere); prior clipboard was restored anyway" } else if was_empty { "clipboard was empty before the paste; the restore re-established an empty clipboard, so the pasted text is no longer the selection" } else { "prior clipboard payloads restored after the app consumed the pasted text" };
                Ok(self.with_observe_label(serde_json::json!({"dispatched": true, "action": "type_text", "target": target.id, "route": "paste", "characters": p.text.chars().count(), "paste_consumed": consumed, "clipboard_restored": restored, "clipboard_was_empty": was_empty, "note": note}), &p.observe, Some(crate::indicator::typed_label(true, &p.text))).await)
            }
            other => Err(err(format!("unknown type_text mode '{other}'"))),
        }
    }

    /// Release all input held by this server on the active target.
    #[tool(description = "Release every button/key/touch contact this server holds on the active target. Does not touch the user's physical input.", annotations(destructive_hint = true))]
    async fn release_all(&self, Parameters(p): Parameters<TargetOptParams>) -> Result<CallToolResult, ErrorData> {
        let target = self.active_target(p.target.as_deref())?;
        let released = {
            let map = self.inputs.lock().unwrap();
            match map.get(&target.id) { Some(dev) => dev.lock().unwrap().release_all().map_err(err)?, None => serde_json::json!({"buttons": [], "keys": [], "note": "no input device had been created for this target"}) }
        };
        Ok(json_result(&serde_json::json!({"released": released, "target": target.id})))
    }

    /// Bounded pause.
    #[tool(description = "Wait for a bounded duration (max 60s).", annotations(read_only_hint = false, destructive_hint = false, idempotent_hint = true))]
    async fn wait(&self, Parameters(p): Parameters<WaitParams>) -> Result<CallToolResult, ErrorData> {
        if p.ms > 60_000 { return Err(err("wait is bounded to 60000ms")); }
        tokio::time::sleep(std::time::Duration::from_millis(p.ms)).await;
        Ok(json_result(&serde_json::json!({"waited_ms": p.ms})))
    }

    /// Sample a pixel color from a retained screenshot or a fresh capture.
    #[tool(description = "Sample RGBA at image-pixel coordinates from a shot_id, or from a fresh capture when shot_id is omitted. Returns source identity and coordinate space.", annotations(read_only_hint = true, destructive_hint = false, idempotent_hint = true))]
    async fn pixel(&self, Parameters(p): Parameters<PixelParams>) -> Result<CallToolResult, ErrorData> {
        let (transform, img) = match &p.shot_id {
            Some(id) => self.shot_image(id)?,
            None => { let t = self.active_target(None)?; self.fresh_capture(&t, None).await? }
        };
        if p.x >= img.width() || p.y >= img.height() { return Err(err(format!("pixel ({},{}) outside {}x{}", p.x, p.y, img.width(), img.height()))); }
        let px = img.get_pixel(p.x, p.y).0;
        let (lx, ly) = transform.pixel_to_logical(p.x as f64, p.y as f64).map_err(err)?;
        Ok(json_result(&serde_json::json!({"shot_id": transform.shot_id, "target": transform.target_id, "output": transform.output, "pixel_xy": [p.x, p.y], "logical_xy": [lx, ly], "rgba": px, "hex": format!("#{:02X}{:02X}{:02X}{:02X}", px[0], px[1], px[2], px[3])})))
    }

    /// Compare two retained screenshots (or a shot vs a fresh capture).
    #[tool(description = "Diff two screenshots: changed ratio, changed regions, optional highlighted diff image. Size mismatch is explicit, never rescaled.", annotations(read_only_hint = true, destructive_hint = false, idempotent_hint = true))]
    async fn diff(&self, Parameters(p): Parameters<DiffParams>) -> Result<CallToolResult, ErrorData> {
        let (_, a) = self.shot_image(&p.shot_id)?;
        let (tb, b) = match &p.other_shot_id {
            Some(id) => self.shot_image(id)?,
            None => { let t = self.active_target(None)?; self.fresh_capture(&t, None).await? }
        };
        let threshold = p.threshold.unwrap_or(8);
        let d = crate::media::diff_images(&a, &b, threshold, &[]);
        let mut meta = serde_json::to_value(&d).unwrap();
        meta["shot_id"] = p.shot_id.clone().into(); meta["other_shot_id"] = tb.shot_id.clone().into(); meta["threshold"] = threshold.into();
        if p.include_highlight == Some(true) && d.size_mismatch.is_none() {
            let hl = crate::media::highlight_diff(&a, &b, threshold, &[]);
            let (bytes, mime, _) = encode_image(&hl, "png", Some(1024)).map_err(err)?;
            return Ok(CallToolResult::success(vec![ContentBlock::image(STANDARD.encode(bytes), mime), ContentBlock::text(serde_json::to_string_pretty(&meta).unwrap())]));
        }
        Ok(json_result(&meta))
    }

    /// OCR a retained screenshot (CPU, ocrs). Boxes/centers are in that shot's pixel space.
    #[tool(description = "CPU OCR of a retained screenshot: visible text with boxes and centers in that shot's pixels. Set download_models:true to fetch missing ocrs models on this OCR request only.", annotations(read_only_hint = true, destructive_hint = false, idempotent_hint = true))]
    async fn read_text(&self, Parameters(p): Parameters<ReadTextParams>) -> Result<CallToolResult, ErrorData> {
        let (transform, words) = self.ocr_words(&p.shot_id, p.download_models.unwrap_or(false)).await?;
        let mut out = Vec::new();
        for w in &words {
            let (lx, ly) = transform.pixel_to_logical(w.center_px[0], w.center_px[1]).unwrap_or((0.0, 0.0));
            out.push(serde_json::json!({"text": w.text, "box_px": w.box_px, "center_px": w.center_px, "center_logical": [lx, ly]}));
        }
        Ok(json_result(&serde_json::json!({"shot_id": p.shot_id, "target": transform.target_id, "words": out, "full_text": words.iter().map(|w| w.text.clone()).collect::<Vec<_>>().join(" ")})))
    }

    /// Find OCR text by literal or regex query.
    #[tool(description = "Search a screenshot's OCR text (literal substring, or regex:true). Returns all matching boxes/centers.", annotations(read_only_hint = true, destructive_hint = false, idempotent_hint = true))]
    async fn find_text(&self, Parameters(p): Parameters<FindTextParams>) -> Result<CallToolResult, ErrorData> {
        let (transform, words) = self.ocr_words(&p.shot_id, p.download_models.unwrap_or(false)).await?;
        let re = if p.regex == Some(true) { Some(regex::Regex::new(&p.query).map_err(|e| err(format!("invalid regex: {e}")))?) } else { None };
        let mut matches = Vec::new();
        for w in &words {
            let hit = match &re { Some(r) => r.is_match(&w.text), None => w.text.contains(&p.query) };
            if hit { let (lx, ly) = transform.pixel_to_logical(w.center_px[0], w.center_px[1]).unwrap_or((0.0, 0.0)); matches.push(serde_json::json!({"text": w.text, "box_px": w.box_px, "center_px": w.center_px, "center_logical": [lx, ly]})); }
        }
        Ok(json_result(&serde_json::json!({"shot_id": p.shot_id, "query": p.query, "matches": matches})))
    }

    /// Bounded wait for OCR text to appear/disappear on fresh captures.
    #[tool(description = "Wait (bounded) for text to appear (mode='appear', default) or disappear on the active target, polling fresh captures with OCR. Timeout returns satisfied:false, not an error.", annotations(read_only_hint = true, destructive_hint = false))]
    async fn wait_for_text(&self, Parameters(p): Parameters<WaitTextParams>) -> Result<CallToolResult, ErrorData> {
        let timeout = p.timeout_ms.unwrap_or(10_000).min(60_000);
        let poll = p.poll_ms.unwrap_or(500).max(100);
        let target = self.active_target(None)?;
        let started = std::time::Instant::now();
        let mut last_shot = String::new();
        loop {
            let (t, _) = self.fresh_capture(&target, None).await?;
            last_shot = t.shot_id.clone();
            let (_, words) = self.ocr_words(&last_shot, p.download_models.unwrap_or(false)).await?;
            let found = words.iter().any(|w| w.text.contains(&p.query));
            let satisfied = if p.mode.as_deref() == Some("disappear") { !found } else { found };
            if satisfied { return Ok(json_result(&serde_json::json!({"satisfied": true, "query": p.query, "shot_id": last_shot, "elapsed_ms": started.elapsed().as_millis()}))); }
            if started.elapsed().as_millis() as u64 >= timeout { return Ok(json_result(&serde_json::json!({"satisfied": false, "query": p.query, "shot_id": last_shot, "elapsed_ms": started.elapsed().as_millis(), "note": "timeout is a normal result, not an observation failure"}))); }
            tokio::time::sleep(std::time::Duration::from_millis(poll)).await;
        }
    }

    /// Combined observation: screenshot + bounded accessibility snapshot (no implicit activation).
    #[tool(description = "One observation: screenshot image plus a bounded accessibility tree when available. Tree unavailability is reported separately; no window is activated.", annotations(read_only_hint = true, destructive_hint = false, idempotent_hint = true))]
    async fn get_app_state(&self, Parameters(p): Parameters<AppStateParams>) -> Result<CallToolResult, ErrorData> {
        let shot_res = self.screenshot(Parameters(ScreenshotParams { output: None, format: Some("png".into()), max_dimension: p.max_dimension.or(Some(1024)), crop: None, grid: Some(false), include_cursor: Some(false), marks: Some(false) })).await?;
        let target = self.active_target(None)?;
        let tree = match self.a11y_conn(&target).await {
            Ok(conn) => match crate::a11y::fetch_tree(&conn, &crate::a11y::TreeOpts { max_nodes: p.max_nodes.unwrap_or(200), max_depth: 6, timeout: std::time::Duration::from_secs(5), include_text: true }).await {
                Ok(t) => serde_json::to_value(&t).unwrap_or(serde_json::json!({"error": "serialize tree"})),
                Err(e) => serde_json::json!({"unavailable": e}),
            },
            Err(e) => serde_json::json!({"unavailable": format!("{e:?}")}),
        };
        let mut contents = Vec::new();
        if let rmcp::model::CallToolResult { content, .. } = shot_res { contents.extend(content); }
        contents.push(ContentBlock::text(serde_json::to_string_pretty(&serde_json::json!({"accessibility": tree})).unwrap()));
        Ok(CallToolResult::success(contents))
    }

    /// Explicitly establish accessibility, reporting desktop-setting changes before applying them.
    #[tool(description = "Establish AT-SPI accessibility for the active target. If the desktop setting is off, this first reports the pending change; pass confirm_desktop_change:true to apply it. Private sandbox targets apply within their own bus.", annotations(destructive_hint = true))]
    async fn a11y_enable(&self, Parameters(p): Parameters<A11yEnableParams>) -> Result<CallToolResult, ErrorData> {
        let target = self.active_target(None)?;
        let status = crate::a11y::status(&target.session_bus).await.map_err(err)?;
        if status["is_enabled"].as_bool() == Some(true) {
            let conn_ok = self.a11y_conn(&target).await.is_ok();
            return Ok(json_result(&serde_json::json!({"enabled": true, "already_enabled": true, "connection_ok": conn_ok, "status": status})));
        }
        if target.kind != "sandbox" && p.confirm_desktop_change != Some(true) {
            return Ok(json_result(&serde_json::json!({"enabled": false, "pending_desktop_change": {"setting": "org.a11y.Status IsEnabled", "from": status["is_enabled"], "to": true, "scope": "host desktop accessibility preference"}, "note": "No setting was changed. Re-run with confirm_desktop_change:true to apply."})));
        }
        crate::a11y::set_enabled(&target.session_bus, true).await.map_err(err)?;
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let conn_ok = self.a11y_conn(&target).await.is_ok();
        Ok(json_result(&serde_json::json!({"enabled": true, "applied_change": "org.a11y.Status IsEnabled=true", "connection_ok": conn_ok})))
    }

    /// Bounded AT-SPI tree with roles/names/states/text/actions and per-node failures.
    #[tool(description = "Bounded accessibility tree (node/depth/time limits; truncation and per-node errors explicit). Refs are 'bus|/path' and are the only handles a11y_act/a11y_set_value accept.", annotations(read_only_hint = true, destructive_hint = false))]
    async fn a11y_tree(&self, Parameters(p): Parameters<A11yTreeParams>) -> Result<CallToolResult, ErrorData> {
        let target = self.active_target(None)?;
        let conn = self.a11y_conn(&target).await?;
        let tree = crate::a11y::fetch_tree(&conn, &crate::a11y::TreeOpts { max_nodes: p.max_nodes.unwrap_or(300).min(2000), max_depth: p.max_depth.unwrap_or(8).min(20), timeout: std::time::Duration::from_secs(8), include_text: p.include_text.unwrap_or(true) }).await.map_err(err)?;
        Ok(json_result(&serde_json::json!({"target": target.id, "tree": tree, "bounds_note": "extents_screen may be absent/client-relative on Wayland; semantic refs do not depend on them"})))
    }

    /// Find accessibility nodes by name/role/text, with optional bounded wait.
    #[tool(description = "Find accessibility nodes matching name/role/text substrings (case-insensitive). Optional bounded wait; returns refs usable by a11y_act/a11y_set_value.", annotations(read_only_hint = true, destructive_hint = false))]
    async fn a11y_find(&self, Parameters(p): Parameters<A11yFindParams>) -> Result<CallToolResult, ErrorData> {
        let target = self.active_target(None)?;
        let timeout = p.timeout_ms.unwrap_or(0).min(30_000);
        let started = std::time::Instant::now();
        loop {
            let conn = self.a11y_conn(&target).await?;
            let tree = crate::a11y::fetch_tree(&conn, &crate::a11y::TreeOpts { max_nodes: 500, max_depth: 12, timeout: std::time::Duration::from_secs(5), include_text: true }).await.map_err(err)?;
            let mut flat = Vec::new(); crate::a11y::flatten(&tree, &mut flat);
            let total = flat.len();
            let matches: Vec<_> = flat.into_iter().filter(|n| {
                let hit = |q: &Option<String>, v: &Option<String>| q.as_ref().map(|q| v.as_ref().map(|v| v.to_ascii_lowercase().contains(&q.to_ascii_lowercase())).unwrap_or(false)).unwrap_or(true);
                hit(&p.name, &n.name) && hit(&p.role, &n.role) && hit(&p.text, &n.text)
            }).collect();
            if !matches.is_empty() || started.elapsed().as_millis() as u64 >= timeout {
                return Ok(json_result(&serde_json::json!({"matches": matches, "count": matches.len(), "scanned_nodes": total})));
            }
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        }
    }

    /// Invoke a named accessibility action on exactly one live element (semantic, not physical input).
    #[tool(description = "Invoke a named exposed accessibility action on a ref from a11y_tree/a11y_find. Semantic activation: can bypass a broken pointer handler and does NOT prove physical input works. Stale refs fail without mutation.", annotations(destructive_hint = true))]
    async fn a11y_act(&self, Parameters(p): Parameters<A11yActParams>) -> Result<CallToolResult, ErrorData> {
        let target = self.active_target(None)?;
        let conn = self.a11y_conn(&target).await?;
        let res = crate::a11y::do_action(&conn, &p.ref_id, &p.action).await.map_err(err)?;
        Ok(json_result(&res))
    }

    /// Set editable text/numeric value semantically and read it back.
    #[tool(description = "Set an editable text or numeric accessibility value and read it back: verification is verified/mismatch/unavailable, never just 'call succeeded'. Semantic, not physical typing.", annotations(destructive_hint = true))]
    async fn a11y_set_value(&self, Parameters(p): Parameters<A11ySetValueParams>) -> Result<CallToolResult, ErrorData> {
        let target = self.active_target(None)?;
        let conn = self.a11y_conn(&target).await?;
        let res = crate::a11y::set_value(&conn, &p.ref_id, &p.value).await.map_err(err)?;
        Ok(json_result(&res))
    }

    /// Wait for screen change, or for a stable interval, on fresh captures.
    #[tool(description = "Wait for visual change (mode='change', default) or a stable interval (mode='stable', stable_ms) using bounded polling. Returns the final shot_id; a still screen on 'change' times out as satisfied:false.", annotations(read_only_hint = true, destructive_hint = false))]
    async fn wait_screen(&self, Parameters(p): Parameters<WaitScreenParams>) -> Result<CallToolResult, ErrorData> {
        let target = self.active_target(None)?;
        let timeout = p.timeout_ms.unwrap_or(10_000).min(60_000);
        let poll = p.poll_ms.unwrap_or(250).max(50);
        let threshold = p.threshold.unwrap_or(8);
        let mode = p.mode.as_deref().unwrap_or("change");
        let started = std::time::Instant::now();
        let (_, mut prev) = self.fresh_capture(&target, p.output.clone()).await?;
        let mut stable_since = std::time::Instant::now();
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(poll)).await;
            let (t, img) = self.fresh_capture(&target, p.output.clone()).await?;
            let d = crate::media::diff_images(&prev, &img, threshold, &[]);
            let changed = d.changed_ratio > 0.0005;
            if mode == "change" && changed {
                return Ok(json_result(&serde_json::json!({"satisfied": true, "mode": mode, "shot_id": t.shot_id, "changed_ratio": d.changed_ratio, "elapsed_ms": started.elapsed().as_millis()})));
            }
            if mode == "stable" {
                if changed { stable_since = std::time::Instant::now(); } else if stable_since.elapsed().as_millis() as u64 >= p.stable_ms.unwrap_or(1000) {
                    return Ok(json_result(&serde_json::json!({"satisfied": true, "mode": mode, "shot_id": t.shot_id, "stable_ms": stable_since.elapsed().as_millis(), "elapsed_ms": started.elapsed().as_millis(), "note": "sampling still frames cannot prove absence of between-sample flicker"})));
                }
            }
            if started.elapsed().as_millis() as u64 >= timeout {
                return Ok(json_result(&serde_json::json!({"satisfied": false, "mode": mode, "shot_id": t.shot_id, "elapsed_ms": started.elapsed().as_millis(), "last_changed_ratio": d.changed_ratio})));
            }
            prev = img;
        }
    }

    /// Sample frames for a bounded period: cadence and consecutive-frame changes (not an FPS profiler).
    #[tool(description = "Sample screenshots for a bounded period: timestamps, achieved sampling rate, consecutive-image changes and repeated patterns. Measures capture cadence/visual change, NOT app FPS, GPU use or presentation deadlines.", annotations(read_only_hint = true, destructive_hint = false))]
    async fn frame_probe(&self, Parameters(p): Parameters<FrameProbeParams>) -> Result<CallToolResult, ErrorData> {
        let target = self.active_target(None)?;
        let duration = p.duration_ms.unwrap_or(2000).min(30_000);
        let interval = p.interval_ms.unwrap_or(200).max(50);
        let threshold = p.threshold.unwrap_or(8);
        let started = std::time::Instant::now();
        let mut samples = Vec::new();
        let mut prev: Option<Arc<RgbaImage>> = None;
        while (started.elapsed().as_millis() as u64) < duration {
            let (t, img) = self.fresh_capture(&target, p.output.clone()).await?;
            let changed_ratio = prev.as_ref().map(|pv| crate::media::diff_images(pv, &img, threshold, &[]).changed_ratio);
            samples.push(serde_json::json!({"shot_id": t.shot_id, "at_ms": started.elapsed().as_millis(), "changed_ratio_vs_prev": changed_ratio}));
            prev = Some(img);
            tokio::time::sleep(std::time::Duration::from_millis(interval)).await;
        }
        let elapsed = started.elapsed().as_secs_f64();
        let rate = if elapsed > 0.0 { samples.len() as f64 / elapsed } else { 0.0 };
        let changes: Vec<f64> = samples.iter().filter_map(|s| s["changed_ratio_vs_prev"].as_f64()).collect();
        let pattern = if changes.iter().all(|c| *c <= 0.0005) { "still" } else if changes.iter().all(|c| *c > 0.0005) { "continuing-change" } else { "intermittent-change" };
        Ok(json_result(&serde_json::json!({"samples": samples, "achieved_sampling_hz": rate, "pattern": pattern, "note": "descriptive observation of sampled frames; not an FPS/quality verdict"})))
    }

    /// Save a named native-resolution baseline screenshot with capture metadata.
    #[tool(description = "Save a named baseline (native resolution, unannotated). Replacing an existing baseline requires overwrite:true.", annotations(destructive_hint = true))]
    async fn baseline_save(&self, Parameters(p): Parameters<BaselineSaveParams>) -> Result<CallToolResult, ErrorData> {
        let name = sanitize_name(&p.name).map_err(err)?;
        let target = self.active_target(None)?;
        let dir = data_dir().join("baselines");
        std::fs::create_dir_all(&dir).map_err(|e| err(e.to_string()))?;
        let png_path = dir.join(format!("{name}.png"));
        if png_path.exists() && p.overwrite != Some(true) { return Err(err(format!("baseline '{name}' exists; pass overwrite:true to replace it"))); }
        let (t, img) = self.fresh_capture(&target, p.output).await?;
        img.save(&png_path).map_err(|e| err(e.to_string()))?;
        let meta = serde_json::json!({"name": name, "transform": t, "saved_at_ms": now_ms(), "note": "baselines are pixel comparisons: fonts/themes/toolkit versions affect results"});
        std::fs::write(dir.join(format!("{name}.json")), serde_json::to_string_pretty(&meta).unwrap()).map_err(|e| err(e.to_string()))?;
        Ok(json_result(&serde_json::json!({"saved": name, "path": png_path, "transform": t})))
    }

    /// Compare the current capture against a named baseline.
    #[tool(description = "Compare current pixels to a named baseline with threshold and ignored regions. Returns pass/fail, size/layout mismatch and changed regions.", annotations(read_only_hint = true, destructive_hint = false))]
    async fn baseline_compare(&self, Parameters(p): Parameters<BaselineCompareParams>) -> Result<CallToolResult, ErrorData> {
        let name = sanitize_name(&p.name).map_err(err)?;
        let dir = data_dir().join("baselines");
        let png_path = dir.join(format!("{name}.png"));
        if !png_path.exists() { return Err(err(format!("baseline '{name}' not found; use baseline_save first"))); }
        let base_meta: serde_json::Value = std::fs::read_to_string(dir.join(format!("{name}.json"))).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(serde_json::json!({}));
        let base = image::open(&png_path).map_err(|e| err(e.to_string()))?.to_rgba8();
        let target = self.active_target(None)?;
        let (t, img) = self.fresh_capture(&target, p.output).await?;
        let ignored: Vec<crate::media::ChangedRegion> = p.ignored_regions.unwrap_or_default().into_iter().map(|r| crate::media::ChangedRegion { x: r.x, y: r.y, width: r.width, height: r.height }).collect();
        let threshold = p.threshold.unwrap_or(8);
        let d = crate::media::diff_images(&base, &img, threshold, &ignored);
        let layout_mismatch = base_meta["transform"]["layout_generation"].as_u64().map(|g| g != t.layout_generation);
        let pass = d.size_mismatch.is_none() && d.changed_ratio <= 0.001;
        let mut out = serde_json::to_value(&d).unwrap();
        out["pass"] = pass.into(); out["baseline"] = name.clone().into(); out["current_shot_id"] = t.shot_id.into(); out["layout_generation_mismatch"] = layout_mismatch.into(); out["baseline_meta"] = base_meta;
        if p.include_highlight == Some(true) && d.size_mismatch.is_none() {
            let hl = crate::media::highlight_diff(&base, &img, threshold, &ignored);
            let diff_path = dir.join(format!("{name}.last-diff.png"));
            let _ = hl.save(&diff_path);
            out["diff_artifact"] = serde_json::json!(diff_path);
        }
        Ok(json_result(&out))
    }

    /// Read clipboard text or a requested MIME type, with the MIME inventory.
    #[tool(description = "Read the active target's clipboard: requested MIME type or text, plus full MIME inventory. Empty clipboard is a normal state. primary:true only where a primary-selection device is verified.", annotations(read_only_hint = true, destructive_hint = false))]
    async fn clipboard_read(&self, Parameters(p): Parameters<ClipboardReadParams>) -> Result<CallToolResult, ErrorData> {
        let target = self.active_target(None)?;
        let handle = self.clipboard_for_async(&target).await?;
        let primary = p.primary.unwrap_or(false);
        let (inventory, read) = {
            let h = handle.clone();
            tokio::task::spawn_blocking(move || -> Result<_, String> { Ok((h.inventory(primary)?, h.read(p.mime.clone(), primary)?)) }).await.map_err(|e| err(e.to_string()))?.map_err(err)?
        };
        let (mime, bytes) = read;
        let text = if mime.starts_with("text/") { String::from_utf8(bytes.clone()).ok() } else { None };
        Ok(json_result(&serde_json::json!({"target": target.id, "mime_inventory": inventory, "mime": if mime.is_empty() { serde_json::Value::Null } else { mime.clone().into() }, "empty": bytes.is_empty(), "text": text, "bytes_base64": if text.is_some() { serde_json::Value::Null } else { STANDARD.encode(&bytes).into() }, "size": bytes.len()})))
    }

    /// Own the target clipboard with text until replaced or target shutdown (explicitly destructive).
    #[tool(description = "Replace the active target's clipboard with text. This server owns the selection until it is replaced or the target shuts down. Explicitly destructive: it does not preserve prior contents (callers wanting preservation use type_text mode='paste' semantics).", annotations(destructive_hint = true))]
    async fn clipboard_write(&self, Parameters(p): Parameters<ClipboardWriteParams>) -> Result<CallToolResult, ErrorData> {
        let target = self.active_target(p.target.as_deref())?;
        let handle = self.clipboard_for_async(&target).await?;
        let payloads = vec![("text/plain;charset=utf-8".to_string(), p.text.into_bytes())];
        let h = handle.clone();
        tokio::task::spawn_blocking(move || h.write(payloads)).await.map_err(|e| err(e.to_string()))?.map_err(err)?;
        Ok(json_result(&serde_json::json!({"written": true, "target": target.id, "owned_until": "replaced or target shutdown"})))
    }

    /// Read selected text via primary selection (opt-in) or clipboard-assisted copy.
    #[tool(description = "Read selected text. via='primary' is opt-in: it may be stale or belong to another app, and is only used where verified. via='copy' sends Ctrl+C and reads the clipboard, restoring prior contents when fully preservable; unpreservable contents cause refusal before any overwrite.", annotations(destructive_hint = true))]
    async fn read_selection(&self, Parameters(p): Parameters<ReadSelectionParams>) -> Result<CallToolResult, ErrorData> {
        let target = self.active_target(None)?;
        match p.via.as_deref().unwrap_or("primary") {
            "primary" => {
                let handle = self.clipboard_for_async(&target).await?;
                let (mime, bytes) = tokio::task::spawn_blocking(move || handle.read(None, true)).await.map_err(|e| err(e.to_string()))?.map_err(err)?;
                Ok(json_result(&serde_json::json!({"via": "primary", "mime": mime, "text": String::from_utf8_lossy(&bytes), "note": "primary selection may be stale or belong to another application"})))
            }
            "copy" => {
                let handle = self.clipboard_for_async(&target).await?;
                let preserved = tokio::task::spawn_blocking({ let h = handle.clone(); move || h.read_all_preservable() }).await.map_err(|e| err(e.to_string()))?.map_err(err)?;
                let dev = self.input_for_async(&target).await?;
                let sel_gen = handle.selection_generation();
                { let mut d = dev.lock().unwrap(); d.hold_key("ctrl").map_err(err)?; let _ = d.key_event_pub(46, true); let _ = d.key_event_pub(46, false); let _ = d.release_key("ctrl"); }
                // Event-driven: the app copies by taking selection ownership asynchronously; wait for that owner change.
                let changed = tokio::task::spawn_blocking({ let h = handle.clone(); move || h.wait_selection_change(sel_gen, std::time::Duration::from_millis(2000)) }).await.map_err(|e| err(e.to_string()))?;
                if !changed {
                    return Ok(json_result(&serde_json::json!({"via": "copy", "copied": false, "text": serde_json::Value::Null, "clipboard_restored": false, "note": "no application took clipboard ownership within 2000 ms after ctrl+c (nothing selected, or focus elsewhere); clipboard left untouched"})));
                }
                // The app now owns the clipboard: restore the preserved
                // payloads on EVERY path, including a failed read, and only
                // then report the read outcome.
                let read_result = tokio::task::spawn_blocking({ let h = handle.clone(); move || h.read(None, false) }).await.map_err(|e| e.to_string()).and_then(|r| r);
                let restore = tokio::task::spawn_blocking(move || handle.write(preserved)).await.map_err(|e| e.to_string()).and_then(|r| r);
                let result = read_result.map_err(|e| err(format!("clipboard read after copy failed: {e}; prior clipboard restore {}", if restore.is_ok() { "succeeded" } else { "also failed" })))?;
                Ok(json_result(&serde_json::json!({"via": "copy", "copied": true, "mime": result.0, "text": String::from_utf8_lossy(&result.1), "clipboard_restored": restore.is_ok()})))
            }
            other => Err(err(format!("unknown read_selection via '{other}'"))),
        }
    }

    /// Touch tap on a verified touchscreen mapping (never faked via pointer).
    #[tool(description = "Touch tap (contact down/up, optional long hold). Requires a granted portal touch capability or the explicitly configured verified uinput touchscreen mapping; the output mapping is reported before dispatch. Unsupported mapping = unsupported, never pointer fallback.", annotations(destructive_hint = true))]
    async fn touch_tap(&self, Parameters(p): Parameters<TouchTapParams>) -> Result<CallToolResult, ErrorData> {
        let target = self.active_target(p.point.target.as_deref())?;
        let ((lx, ly), _, _, _) = self.layout_then_resolve(&target, &p.point)?;
        let hold = capped_duration(p.hold_ms, 60);
        if let Some(res) = self.portal_touch_stream_async(&target, lx, ly).await {
            let (handle, node, x, y) = res.map_err(err)?;
            tokio::task::spawn_blocking(move || -> Result<(), String> {
                let run = (|| -> Result<(), String> {
                    handle.notify(crate::portal_session::Notify::TouchDown { stream: node, slot: 0, x, y })?;
                    std::thread::sleep(std::time::Duration::from_millis(hold));
                    handle.notify(crate::portal_session::Notify::TouchUp { slot: 0 })
                })();
                if run.is_err() { let _ = handle.notify(crate::portal_session::Notify::TouchUp { slot: 0 }); }
                run
            }).await.map_err(|e| err(e.to_string()))?.map_err(err)?;
            return Ok(self.with_observe(serde_json::json!({"dispatched": true, "action": "touch_tap", "target": target.id, "mapping": format!("portal RemoteDesktop stream {node}"), "logical": [lx, ly], "hold_ms": p.hold_ms}), &p.point.observe).await);
        }
        let (mut dev, dx, dy) = self.touch_point(&target, lx, ly)?;
        let mapping = format!("{} -> device({dx},{dy}) on {}", dev.mapping.device, dev.mapping.output);
        tokio::task::spawn_blocking(move || -> Result<(), String> {
            let run = (|| -> Result<(), String> {
                dev.frame(&[(0, dx, dy, true)])?;
                std::thread::sleep(std::time::Duration::from_millis(hold));
                dev.frame(&[(0, dx, dy, false)])
            })();
            if run.is_err() { let _ = dev.frame(&[(0, dx, dy, false)]); }
            run
        }).await.map_err(|e| err(e.to_string()))?.map_err(err)?;
        Ok(self.with_observe(serde_json::json!({"dispatched": true, "action": "touch_tap", "target": target.id, "mapping": mapping, "logical": [lx, ly], "hold_ms": p.hold_ms}), &p.point.observe).await)
    }

    /// Coordinated touch swipe along points.
    #[tool(description = "One touch contact following the given points over duration_ms, on a verified touchscreen mapping only.", annotations(destructive_hint = true))]
    async fn touch_swipe(&self, Parameters(p): Parameters<TouchSwipeParams>) -> Result<CallToolResult, ErrorData> {
        if p.points.len() < 2 { return Err(err("touch_swipe needs at least 2 points")); }
        let distinct: std::collections::HashSet<&String> = p.points.iter().filter_map(|pt| pt.target.as_ref()).collect();
        if distinct.len() > 1 { return Err(err(format!("touch_swipe points name different targets {distinct:?}"))); }
        let target = self.active_target(p.points[0].target.as_deref())?;
        // Refresh layout BEFORE resolving any shot-space point (stale-shot check).
        let (outputs, _, _) = self.layout(&target)?;
        let mut logical = Vec::new();
        for pt in &p.points { let (x, y) = self.resolve(pt, &target.id)?; require_point_in_outputs(&outputs, x, y)?; logical.push((x, y)); }
        // Every point (and hence every interpolated step, outputs being
        // rectangular) must lie on the first point's output.
        let first_out = outputs.iter().find(|o| point_in_outputs(std::slice::from_ref(*o), logical[0].0, logical[0].1)).cloned().ok_or_else(|| err("touch start point is not inside any output"))?;
        for (x, y) in &logical { if !point_in_outputs(std::slice::from_ref(&first_out), *x, *y) { return Err(err(format!("touch_swipe point ({x},{y}) is outside output {} used by the first point; swipes stay on one output", first_out.name))); } }
        let duration = capped_duration(p.duration_ms, 300);
        let n_points = logical.len();
        if let Some(res) = self.portal_touch_stream_async(&target, logical[0].0, logical[0].1).await {
            let (handle, node, x0, y0) = res.map_err(err)?;
            let (origin_x, origin_y) = (logical[0].0 - x0, logical[0].1 - y0);
            let pts = logical.clone();
            tokio::task::spawn_blocking(move || -> Result<(), String> {
                let run = (|| -> Result<(), String> {
                    handle.notify(crate::portal_session::Notify::TouchDown { stream: node, slot: 0, x: x0, y: y0 })?;
                    let steps = 12u64;
                    for seg in pts.windows(2) {
                        for i in 1..=steps {
                            let t = i as f64 / steps as f64;
                            let (lx, ly) = (seg[0].0 + (seg[1].0 - seg[0].0) * t, seg[0].1 + (seg[1].1 - seg[0].1) * t);
                            handle.notify(crate::portal_session::Notify::TouchMotion { stream: node, slot: 0, x: lx - origin_x, y: ly - origin_y })?;
                            std::thread::sleep(std::time::Duration::from_millis((duration / (steps * (pts.len() as u64 - 1).max(1))).max(1)));
                        }
                    }
                    handle.notify(crate::portal_session::Notify::TouchUp { slot: 0 })
                })();
                if run.is_err() { let _ = handle.notify(crate::portal_session::Notify::TouchUp { slot: 0 }); }
                run
            }).await.map_err(|e| err(e.to_string()))?.map_err(err)?;
            return Ok(self.with_observe(serde_json::json!({"dispatched": true, "action": "touch_swipe", "target": target.id, "mapping": format!("portal RemoteDesktop stream {node}"), "points": n_points, "points_logical": logical, "duration_ms": p.duration_ms}), &p.points[0].observe).await);
        }
        let (mut dev, dx, dy) = self.touch_point(&target, logical[0].0, logical[0].1)?;
        let mapping = format!("{} on {}", dev.mapping.device, dev.mapping.output);
        let out = outputs.iter().find(|o| o.name == dev.mapping.output).cloned().ok_or_else(|| err("mapped output vanished"))?;
        let pts_logical = logical.clone();
        // Layout was computed once, before the loop; device coords derive from it.
        tokio::task::spawn_blocking(move || -> Result<(), String> {
            let to_dev = |lx: f64, ly: f64, dev: &crate::touch::TouchDevice| (((lx - out.logical.x as f64) / out.logical.width as f64 * dev.mapping.max_x as f64) as i32, ((ly - out.logical.y as f64) / out.logical.height as f64 * dev.mapping.max_y as f64) as i32);
            let run = (|| -> Result<(), String> {
                dev.frame(&[(0, dx, dy, true)])?;
                let steps = 12u64;
                for seg in logical.windows(2) {
                    for i in 1..=steps {
                        let t = i as f64 / steps as f64;
                        let (lx, ly) = (seg[0].0 + (seg[1].0 - seg[0].0) * t, seg[0].1 + (seg[1].1 - seg[0].1) * t);
                        let (ddx, ddy) = to_dev(lx, ly, &dev);
                        dev.move_contact(0, ddx, ddy)?;
                        std::thread::sleep(std::time::Duration::from_millis((duration / (steps * (logical.len() as u64 - 1).max(1))).max(1)));
                    }
                }
                let (lx1, ly1) = *logical.last().unwrap();
                let (edx, edy) = to_dev(lx1, ly1, &dev);
                dev.frame(&[(0, edx, edy, false)])
            })();
            if run.is_err() { let _ = dev.frame(&[(0, dx, dy, false)]); }
            run
        }).await.map_err(|e| err(e.to_string()))?.map_err(err)?;
        Ok(self.with_observe(serde_json::json!({"dispatched": true, "action": "touch_swipe", "target": target.id, "mapping": mapping, "points": n_points, "points_logical": pts_logical, "duration_ms": p.duration_ms}), &p.points[0].observe).await)
    }

    /// Two-contact pinch around a center.
    #[tool(description = "Two contacts moving apart/together around a center on a verified touchscreen mapping only.", annotations(destructive_hint = true))]
    async fn touch_pinch(&self, Parameters(p): Parameters<TouchPinchParams>) -> Result<CallToolResult, ErrorData> {
        let target = self.active_target(p.center.target.as_deref())?;
        let ((cx, cy), outputs, _, _) = self.layout_then_resolve(&target, &p.center)?;
        // Both contacts, at both distances, must land on an output.
        for dist in [p.start_distance, p.end_distance] {
            require_point_in_outputs(&outputs, cx - dist / 2.0, cy)?;
            require_point_in_outputs(&outputs, cx + dist / 2.0, cy)?;
        }
        let duration = capped_duration(p.duration_ms, 300);
        if let Some(res) = self.portal_touch_stream_async(&target, cx, cy).await {
            let (handle, node, pcx, pcy) = res.map_err(err)?;
            let (ox, oy) = (cx - pcx, cy - pcy);
            let (start_d, end_d) = (p.start_distance, p.end_distance);
            tokio::task::spawn_blocking(move || -> Result<(), String> {
                let run = (|| -> Result<(), String> {
                    handle.notify(crate::portal_session::Notify::TouchDown { stream: node, slot: 0, x: cx - start_d / 2.0 - ox, y: cy - oy })?;
                    handle.notify(crate::portal_session::Notify::TouchDown { stream: node, slot: 1, x: cx + start_d / 2.0 - ox, y: cy - oy })?;
                    let steps = 10u64;
                    for i in 1..=steps {
                        let t = i as f64 / steps as f64;
                        let dist = start_d + (end_d - start_d) * t;
                        handle.notify(crate::portal_session::Notify::TouchMotion { stream: node, slot: 0, x: cx - dist / 2.0 - ox, y: cy - oy })?;
                        handle.notify(crate::portal_session::Notify::TouchMotion { stream: node, slot: 1, x: cx + dist / 2.0 - ox, y: cy - oy })?;
                        std::thread::sleep(std::time::Duration::from_millis((duration / steps).max(1)));
                    }
                    handle.notify(crate::portal_session::Notify::TouchUp { slot: 0 })?;
                    handle.notify(crate::portal_session::Notify::TouchUp { slot: 1 })
                })();
                if run.is_err() { let _ = handle.notify(crate::portal_session::Notify::TouchUp { slot: 0 }); let _ = handle.notify(crate::portal_session::Notify::TouchUp { slot: 1 }); }
                run
            }).await.map_err(|e| err(e.to_string()))?.map_err(err)?;
            return Ok(self.with_observe(serde_json::json!({"dispatched": true, "action": "touch_pinch", "target": target.id, "mapping": format!("portal RemoteDesktop stream {node}"), "center": [cx, cy], "start_distance": start_d, "end_distance": end_d, "duration_ms": p.duration_ms}), &p.center.observe).await);
        }
        let (mut dev, _, _) = self.touch_point(&target, cx, cy)?;
        let mapping = format!("{} on {}", dev.mapping.device, dev.mapping.output);
        let out = outputs.iter().find(|o| o.name == dev.mapping.output).cloned().ok_or_else(|| err("mapped output vanished"))?;
        let (start_d, end_d) = (p.start_distance, p.end_distance);
        tokio::task::spawn_blocking(move || -> Result<(), String> {
            let to_dev = |lx: f64, ly: f64, dev: &crate::touch::TouchDevice| (((lx - out.logical.x as f64) / out.logical.width as f64 * dev.mapping.max_x as f64) as i32, ((ly - out.logical.y as f64) / out.logical.height as f64 * dev.mapping.max_y as f64) as i32);
            let (sx0, sy0) = to_dev(cx - start_d / 2.0, cy, &dev);
            let (sx1, sy1) = to_dev(cx + start_d / 2.0, cy, &dev);
            let run = (|| -> Result<(), String> {
                dev.frame(&[(0, sx0, sy0, true), (1, sx1, sy1, true)])?;
                let steps = 10u64;
                for i in 1..=steps {
                    let t = i as f64 / steps as f64;
                    let dist = start_d + (end_d - start_d) * t;
                    let (x0, y0) = to_dev(cx - dist / 2.0, cy, &dev);
                    let (x1, y1) = to_dev(cx + dist / 2.0, cy, &dev);
                    dev.move_contact(0, x0, y0)?;
                    dev.move_contact(1, x1, y1)?;
                    std::thread::sleep(std::time::Duration::from_millis((duration / steps).max(1)));
                }
                let (ex0, ey0) = to_dev(cx - end_d / 2.0, cy, &dev);
                let (ex1, ey1) = to_dev(cx + end_d / 2.0, cy, &dev);
                dev.frame(&[(0, ex0, ey0, false), (1, ex1, ey1, false)])
            })();
            if run.is_err() { let _ = dev.frame(&[(0, sx0, sy0, false), (1, sx1, sy1, false)]); }
            run
        }).await.map_err(|e| err(e.to_string()))?.map_err(err)?;
        Ok(self.with_observe(serde_json::json!({"dispatched": true, "action": "touch_pinch", "target": target.id, "mapping": mapping, "center": [cx, cy], "start_distance": start_d, "end_distance": end_d, "duration_ms": p.duration_ms}), &p.center.observe).await)
    }

    /// Start one nested-niri private test target (own Wayland socket, D-Bus, a11y bus, clipboard).
    #[tool(description = "Start a nested niri compositor as a private target: separate Wayland socket, private D-Bus session, accessibility bus and clipboard. This isolates desktop interaction, NOT filesystem/network access. Missing niri is a prerequisite error; no other compositor is substituted and the host is never taken over as a fallback.", annotations(destructive_hint = true))]
    async fn sandbox_start(&self, Parameters(p): Parameters<SandboxStartParams>) -> Result<CallToolResult, ErrorData> {
        let id = { let mut c = self.counters.lock().unwrap(); c.0 += 1; format!("sandbox-{}-{}", std::process::id(), c.0) };
        // Registration happens inside a detached task that owns the maps:
        // even if this request is cancelled while the blocking start runs,
        // the finished sandbox is registered (and thus reachable by
        // shutdown_cleanup) instead of being dropped and orphaned.
        let (tx, rx) = tokio::sync::oneshot::channel::<Result<(String, String), String>>();
        {
            let me = self.clone();
            let id2 = id.clone();
            tokio::spawn(async move {
                let started = tokio::task::spawn_blocking({ let id3 = id2.clone(); move || crate::apps::start_sandbox(&id3, p.width.unwrap_or(1280), p.height.unwrap_or(720)) }).await;
                let result = match started {
                    Ok(Ok((sb, spec, bus))) => {
                        let socket = spec.socket.clone();
                        { let mut s = me.session.lock().unwrap(); s.targets.insert(id2.clone(), spec); }
                        me.sandboxes.lock().unwrap().insert(id2.clone(), sb);
                        Ok((socket.to_string_lossy().to_string(), bus))
                    }
                    Ok(Err(e)) => Err(e),
                    Err(e) => Err(e.to_string()),
                };
                let _ = tx.send(result);
            });
        }
        let (socket, bus) = rx.await.map_err(|_| err("sandbox start task ended without reporting a result"))?.map_err(err)?;
        // Report effective geometry from the target itself.
        let info = wayland::discover(&self.active_target(Some(&id))?).map(|d| serde_json::to_value(&d.outputs).unwrap_or_default()).unwrap_or(serde_json::json!({"error": "discovery after start failed"}));
        Ok(json_result(&serde_json::json!({"target": id, "kind": "sandbox", "socket": socket, "session_bus": bus, "outputs": info, "note": "Use target_select to make it active, or pass target explicitly where supported. Desktop-interaction isolation only."})))
    }

    /// Stop a sandbox target: owned apps/compositor/buses only.
    #[tool(description = "Stop the sandbox target (default: active sandbox): stops only apps this server launched on it, its nested compositor, its private buses and every service D-Bus-activated on its private bus; releases held input and detaches the target. Never stops host processes.", annotations(destructive_hint = true))]
    async fn sandbox_stop(&self, Parameters(p): Parameters<SandboxStopParams>) -> Result<CallToolResult, ErrorData> {
        let target_id = p.target.clone().unwrap_or_else(|| self.session.lock().unwrap().active_target.clone());
        if target_id == "host" { return Err(err("sandbox_stop requires a sandbox target; the host target cannot be stopped")); }
        // Stop owned apps on this target first, and drop their entries so a
        // later stop can never re-signal a reaped pid/pgid.
        let app_ids: Vec<String> = self.apps_lock().iter().filter(|(_, a)| a.target_id == target_id).map(|(k, _)| k.clone()).collect();
        for app_id in &app_ids { if let Some(entry) = self.apps_lock().remove(app_id) { let _ = crate::apps::stop_app(&entry, 1000); } }
        // Release held input and shut down clipboard ownership.
        if let Some(dev) = self.inputs.lock().unwrap().remove(&target_id) { let _ = dev.lock().unwrap().release_all(); }
        if let Some(h) = self.clipboards.lock().unwrap().remove(&target_id) { h.shutdown(); }
        let sb = self.sandboxes.lock().unwrap().remove(&target_id).ok_or_else(|| err(format!("target '{target_id}' is not a running sandbox started by this server")))?;
        tokio::task::spawn_blocking(move || crate::apps::stop_sandbox(&sb)).await.map_err(|e| err(e.to_string()))?;
        {
            let mut s = self.session.lock().unwrap();
            s.targets.remove(&target_id);
            if s.active_target == target_id { s.active_target = "host".into(); }
        }
        Ok(json_result(&serde_json::json!({"stopped": target_id, "apps_stopped": app_ids, "active_target_now": "host (explicit; no automatic retargeting of future calls occurred beyond this detach)"})))
    }

    /// Explicitly choose host or a running sandbox as the active target. No automatic fallback.
    #[tool(description = "Select the active target ('host' or a sandbox id from sandbox_start). No automatic fallback: if an isolated target is unavailable, calls fail rather than touching the real desktop.", annotations(destructive_hint = true))]
    async fn target_select(&self, Parameters(p): Parameters<TargetSelectParams>) -> Result<CallToolResult, ErrorData> {
        {
            let mut s = self.session.lock().unwrap();
            if !s.targets.contains_key(&p.target) { return Err(err(format!("unknown target '{}'; known: {:?}", p.target, s.targets.keys().collect::<Vec<_>>()))); }
            s.active_target = p.target.clone();
        }
        let target = self.active_target(None)?;
        Ok(json_result(&serde_json::json!({"active_target": p.target, "kind": target.kind, "socket": target.socket})))
    }

    /// Launch an app (argv, no shell) on the selected target with captured stdout/stderr.
    #[tool(description = "Launch argv (no shell) with cwd/env on the target, optionally with WAYLAND_DEBUG=client protocol diagnostics captured into its logs. Returns the owned process identity. Only server-launched processes can be listed/logged/stopped.", annotations(destructive_hint = true))]
    async fn app_launch(&self, Parameters(p): Parameters<AppLaunchParams>) -> Result<CallToolResult, ErrorData> {
        let target = self.active_target(p.target.as_deref())?;
        let id = { let mut c = self.counters.lock().unwrap(); c.1 += 1; format!("app-{}", c.1) };
        let entry = crate::apps::launch_app(&id, &target, &p.argv, &p.cwd, &p.env.clone().unwrap_or_default(), p.wayland_debug.unwrap_or(false), now_ms()).map_err(err)?;
        let pid = entry.child.lock().unwrap().id();
        let arc = Arc::new(entry);
        self.apps_lock().insert(id.clone(), arc);
        if let Some(wait) = p.wait_ms { tokio::time::sleep(std::time::Duration::from_millis(wait.min(10_000))).await; }
        // The entry may have been removed during the wait (e.g. a concurrent
        // sandbox_stop): report it as removed, never panic holding the lock.
        let entry = self.apps_lock().get(&id).cloned();
        match entry {
            Some(e) => { let (status, code) = crate::apps::app_status(&e); Ok(json_result(&serde_json::json!({"app_id": id, "pid": pid, "target": target.id, "argv": p.argv, "status": status, "exit_code": code, "window_discovery": "window/focus metadata depends on compositor-exposed protocols; use screenshot/a11y_find to locate the app window"}))) }
            None => Ok(json_result(&serde_json::json!({"app_id": id, "pid": pid, "target": target.id, "argv": p.argv, "status": "removed", "exit_code": serde_json::Value::Null, "note": "app entry was removed during wait_ms (e.g. sandbox_stop stopped its target); the app is no longer tracked by this server"}))),
        }
    }

    /// List only server-launched processes with launch specs and status.
    #[tool(description = "List processes launched by this server (never arbitrary system processes), with launch specs, status and exit info.", annotations(read_only_hint = true, destructive_hint = false))]
    async fn app_list(&self) -> Result<CallToolResult, ErrorData> {
        let apps = self.apps_lock();
        let list: Vec<_> = apps.values().map(|a| { let (status, code) = crate::apps::app_status(a); serde_json::json!({"app_id": a.id, "target": a.target_id, "argv": a.argv, "cwd": a.cwd, "status": status, "exit_code": code, "started_ms": a.started_ms, "wayland_debug": a.wayland_debug}) }).collect();
        Ok(json_result(&serde_json::json!({"apps": list})))
    }

    /// Cursor-based bounded stdout/stderr retrieval with filtering.
    #[tool(description = "Read an owned app's captured stdout/stderr by cursor (returns next_cursor; explicit truncation). Includes WAYLAND_DEBUG=client protocol lines when launched with wayland_debug:true — client-side diagnostics, not a global compositor sniffer.", annotations(read_only_hint = true, destructive_hint = false))]
    async fn app_logs(&self, Parameters(p): Parameters<AppLogsParams>) -> Result<CallToolResult, ErrorData> {
        let entry = self.apps_lock().get(&p.app_id).cloned().ok_or_else(|| err(format!("unknown app '{}'; only server-launched apps have logs", p.app_id)))?;
        let cursor = p.cursor.unwrap_or(0);
        let max = p.max_lines.unwrap_or(200).min(2000);
        let store = entry.logs.lock().unwrap();
        let mut lines = Vec::new(); let mut next = cursor; let mut truncated = false;
        for (seq, stream, line) in store.lines.iter() {
            if *seq <= cursor { continue; }
            if let Some(f) = &p.filter { if !line.contains(f.as_str()) { next = *seq; continue; } }
            if lines.len() >= max { truncated = true; break; }
            lines.push(serde_json::json!({"seq": seq, "stream": stream, "line": line}));
            next = *seq;
        }
        Ok(json_result(&serde_json::json!({"app_id": p.app_id, "lines": lines, "next_cursor": next, "truncated": truncated, "buffered_lines": store.lines.len()})))
    }

    /// Stop an owned process group; escalate after timeout. Unknown processes are refused.
    #[tool(description = "Signal an owned app's process group and wait for exit, escalating to SIGKILL after timeout_ms. Refuses any process this server did not launch.", annotations(destructive_hint = true))]
    async fn app_stop(&self, Parameters(p): Parameters<AppStopParams>) -> Result<CallToolResult, ErrorData> {
        let entry = self.apps_lock().get(&p.app_id).cloned().ok_or_else(|| err(format!("refusing to stop unknown app '{}': only server-launched processes can be stopped", p.app_id)))?;
        let res = tokio::task::spawn_blocking({ let e = entry.clone(); move || crate::apps::stop_app(&e, p.timeout_ms.unwrap_or(2000).min(30_000)) }).await.map_err(|e| err(e.to_string()))?.map_err(err)?;
        Ok(json_result(&res))
    }

    /// Restart a stored owned launch spec, returning the new identity.
    #[tool(description = "Restart a server-launched app from its stored spec on the same target; returns the new app_id/pid. The old process must already be stopped or is stopped first.", annotations(destructive_hint = true))]
    async fn app_restart(&self, Parameters(p): Parameters<AppIdParams>) -> Result<CallToolResult, ErrorData> {
        let old = self.apps_lock().get(&p.app_id).cloned().ok_or_else(|| err(format!("unknown app '{}'", p.app_id)))?;
        let (status, _) = crate::apps::app_status(&old);
        if status == "running" { let e = old.clone(); tokio::task::spawn_blocking(move || crate::apps::stop_app(&e, 2000)).await.map_err(|e| err(e.to_string()))?.map_err(err)?; }
        let target = self.active_target(Some(&old.target_id))?;
        let id = { let mut c = self.counters.lock().unwrap(); c.1 += 1; format!("app-{}", c.1) };
        let entry = crate::apps::launch_app(&id, &target, &old.argv, &old.cwd, &old.env, old.wayland_debug, now_ms()).map_err(err)?;
        let pid = entry.child.lock().unwrap().id();
        self.apps_lock().insert(id.clone(), Arc::new(entry));
        Ok(json_result(&serde_json::json!({"app_id": id, "pid": pid, "restarted_from": p.app_id, "target": target.id})))
    }

    /// Typed ordered action batch: fully validated before starting, sequential, per-step status.
    #[tool(description = "Run a typed ordered action list. EVERY step is validated before the first dispatch; execution is sequential with per-step status and completed count; stops at the first error by default (stop_on_error:false to continue). No nested batches and no process/D-Bus/sandbox/target operations inside a batch. No rollback is claimed.", annotations(destructive_hint = true))]
    async fn batch(&self, Parameters(p): Parameters<BatchParams>) -> Result<CallToolResult, ErrorData> {
        if p.steps.is_empty() { return Err(err("batch steps must not be empty")); }
        if p.steps.len() > 100 { return Err(err("batch is bounded to 100 steps")); }
        // Validate everything before dispatching anything.
        for (i, step) in p.steps.iter().enumerate() {
            if let Err(e) = self.validate_batch_step(step) { return Ok(json_result(&serde_json::json!({"dispatched": false, "validation_error_at_step": i, "error": format!("{e:?}"), "completed": 0}))); }
        }
        let stop = p.stop_on_error.unwrap_or(true);
        let mut results = Vec::new();
        let mut completed = 0usize;
        for (i, step) in p.steps.iter().enumerate() {
            let args = step.arguments.clone().unwrap_or(serde_json::json!({}));
            // The target this step actually dispatches on: its explicit
            // target, else the ACTIVE target now — never the trace-start
            // target (transcribe-l7v.1).
            let step_target = Self::arg_target(&args).unwrap_or_else(|| self.session.lock().unwrap().active_target.clone());
            // Dispatch with this task's trace suppression set (task-local,
            // transcribe-l7v.6): the batch_step recorded below is the single
            // record, and concurrent calls in other tasks still record.
            let dispatched = TRACE_SUPPRESSED.scope(std::cell::Cell::new(true), self.dispatch_action(&step.tool, args.clone())).await;
            match dispatched {
                Ok(res) => {
                    // Only a successfully completed step is recorded, hence
                    // replayable; failed/partial steps leave no event
                    // (transcribe-l7v.3).
                    self.record_event("batch_step", serde_json::json!({"tool": step.tool, "target": step_target, "status": "ok"}), Some(args));
                    let text: String = res.content.iter().filter_map(|c| match c { ContentBlock::Text(t) => Some(t.text.clone()), ContentBlock::Image(_) => Some("[image]".into()), _ => None }).collect::<Vec<_>>().join(" ").chars().take(300).collect();
                    results.push(serde_json::json!({"step": i, "tool": step.tool, "status": "ok", "result": text}));
                    completed += 1;
                }
                Err(e) => {
                    results.push(serde_json::json!({"step": i, "tool": step.tool, "status": "error", "error": format!("{e:?}")}));
                    if stop { break; }
                }
            }
        }
        Ok(json_result(&serde_json::json!({"completed": completed, "total": p.steps.len(), "stopped_on_error": stop, "results": results, "note": "completed prefix is real; no rollback is claimed for it"})))
    }

    /// Start an opt-in trace of server-issued actions.
    #[tool(description = "Start recording a trace of actions THIS server issues (timestamps, target/output geometry per event; images are NOT embedded — events reference shot_ids). It does not record human keyboard activity. Typed/clipboard text is redacted to a character count. Traces can contain sensitive context and stay local under the private data dir until explicitly read/exported.", annotations(destructive_hint = true))]
    async fn trace_start(&self, Parameters(p): Parameters<TraceStartParams>) -> Result<CallToolResult, ErrorData> {
        let target = self.active_target(None)?;
        let outputs = wayland::discover(&target).map(|d| serde_json::to_value(&d.outputs).unwrap_or_default()).unwrap_or(serde_json::json!([]));
        let id = format!("trace-{}", now_ms());
        let dir = data_dir().join("traces");
        std::fs::create_dir_all(&dir).map_err(|e| err(e.to_string()))?;
        let path = dir.join(format!("{id}.jsonl"));
        // Header line: recorded target + output layout, persisted so
        // trace_replay can verify both before dispatching anything.
        let header = serde_json::json!({"tool": "trace_header", "trace_id": id, "target": target.id, "outputs": outputs, "started_ms": now_ms()});
        std::fs::write(&path, format!("{header}\n")).map_err(|e| err(e.to_string()))?;
        *self.trace.lock().unwrap() = Some(TraceState { id: id.clone(), path: path.clone(), target_id: target.id.clone(), events: Vec::new(), started_ms: now_ms() });
        Ok(json_result(&serde_json::json!({"trace_id": id, "path": path, "target": target.id, "include_images": false, "note": if p.include_images == Some(true) { "include_images requested, but images are not embedded in traces; events reference shot_ids instead" } else { "images are not embedded in traces; events reference shot_ids" }})))
    }

    /// Stop the active trace and return its events/path.
    #[tool(description = "Stop the active trace. Returns the recorded events and file path. Not a recorder of all desktop activity — only server-issued actions while the trace was active.", annotations(read_only_hint = true, destructive_hint = false))]
    async fn trace_stop(&self) -> Result<CallToolResult, ErrorData> {
        let trace = self.trace.lock().unwrap().take().ok_or_else(|| err("no trace is active"))?;
        Ok(json_result(&serde_json::json!({"trace_id": trace.id, "path": trace.path, "events": trace.events, "event_count": trace.events.len(), "started_ms": trace.started_ms})))
    }

    /// Explicit replay of a trace; dry-run supported; layout mismatch is rejected. Potentially destructive.
    #[tool(description = "Replay a recorded trace (by trace_id or path). Explicit and potentially destructive: dry_run:true only reports the plan. Non-dry-run requires the recorded target and output layout to match the current ones, or it refuses. Never replays automatically after a failure. Typed text is redacted in traces and is skipped, reported as non-replayable.", annotations(destructive_hint = true))]
    async fn trace_replay(&self, Parameters(p): Parameters<TraceReplayParams>) -> Result<CallToolResult, ErrorData> {
        let path = match (&p.path, &p.trace_id) {
            (Some(path), _) => std::path::PathBuf::from(path),
            (None, Some(id)) => data_dir().join("traces").join(format!("{}.jsonl", sanitize_name(id).map_err(err)?)),
            (None, None) => return Err(err("trace_replay needs trace_id or path")),
        };
        let content = std::fs::read_to_string(&path).map_err(|e| err(format!("read trace {}: {e}", path.display())))?;
        let lines: Vec<serde_json::Value> = content.lines().filter(|l| !l.trim().is_empty()).filter_map(|l| serde_json::from_str(l).ok()).collect();
        let header = lines.iter().find(|l| l["tool"] == "trace_header").cloned();
        let events: Vec<serde_json::Value> = lines.into_iter().filter(|l| l["tool"] != "trace_header").collect();
        if events.is_empty() { return Err(err("trace contains no replayable events")); }
        // The replay target is the recorded one (header, else first event).
        // Every recorded action must have been recorded on that same target:
        // a trace spanning targets is refused, never partially retargeted.
        let recorded_target = header.as_ref().and_then(|h| h["target"].as_str().map(|s| s.to_string()))
            .or_else(|| events.iter().find_map(|e| e["target"].as_str().map(|s| s.to_string()))).unwrap_or_default();
        let distinct_targets: Vec<String> = { let mut v: Vec<String> = events.iter().filter_map(|e| e["target"].as_str().map(|s| s.to_string())).collect(); v.sort(); v.dedup(); v };
        // Inject the recorded target into every planned step, in the shape
        // that tool's parameters accept, so dispatch cannot fall back to
        // whatever target is currently active.
        fn inject_target(tool: &str, args: &mut serde_json::Value, target: &str) {
            let set = |obj: &mut serde_json::Value| { if let Some(o) = obj.as_object_mut() { o.insert("target".into(), serde_json::json!(target)); } };
            match tool {
                "drag" => { set(&mut args["from"]); set(&mut args["to"]); if let Some(wps) = args["waypoints"].as_array_mut() { for w in wps { set(w); } } }
                "touch_swipe" => { if let Some(pts) = args["points"].as_array_mut() { for pt in pts { set(pt); } } }
                "touch_pinch" => set(&mut args["center"]),
                // screenshot and a11y_* take no target parameter.
                "screenshot" | "a11y_act" | "a11y_set_value" => {}
                _ => set(args),
            }
        }
        const TARGETLESS: &[&str] = &["screenshot", "a11y_act", "a11y_set_value"];
        // Plan: expand batch steps and exactly-reconstructable summaries.
        // `skipped` = deliberately non-replayable (redacted text, documented);
        // `unreplayable` = cannot be reproduced exactly: replay refuses
        // BEFORE dispatching anything if any such event is present
        // (transcribe-l7v.4).
        let mut plan: Vec<(String, serde_json::Value)> = Vec::new();
        let mut skipped: Vec<String> = Vec::new();
        let mut unreplayable: Vec<String> = Vec::new();
        for event in &events {
            let event_target = event["target"].as_str().unwrap_or(&recorded_target).to_string();
            let tool_name = event["tool"].as_str().unwrap_or("").to_string();
            let s = &event["summary"];
            let has = |k: &str| s.as_object().map(|o| o.contains_key(k)).unwrap_or(false);
            let pt = |v: &serde_json::Value, target: &str| serde_json::json!({"x": v[0], "y": v[1], "space": "logical", "target": target});
            match tool_name.as_str() {
                "batch_step" => {
                    let tool = s["tool"].as_str().unwrap_or("").to_string();
                    if s.get("status").and_then(|v| v.as_str()).map(|st| st != "ok").unwrap_or(false) || event.get("status").and_then(|v| v.as_str()).map(|st| st != "ok").unwrap_or(false) {
                        unreplayable.push(format!("batch_step {tool} (recorded with non-ok status; failed/partial steps are never replayed)")); continue;
                    }
                    // Typed/clipboard text is redacted in traces: never replayable.
                    if tool == "type_text" || tool == "clipboard_write" { skipped.push(format!("{tool} (text redacted in trace; non-replayable)")); continue; }
                    if let Some(args) = event.get("arguments").filter(|a| !a.is_null()) {
                        if args.to_string().contains("\"redacted\"") { skipped.push(format!("{tool} (arguments redacted in trace; non-replayable)")); continue; }
                        let mut args = args.clone();
                        inject_target(&tool, &mut args, &event_target);
                        plan.push((tool, args));
                    } else { unreplayable.push("batch_step without arguments".into()); }
                }
                "move" => {
                    if s["logical"].is_array() { plan.push(("move".into(), pt(&s["logical"], &event_target))); } else { unreplayable.push("move (no logical point recorded)".into()); }
                }
                "click" => {
                    if s["logical"].is_array() && has("modifiers") && has("long_press_ms") {
                        let mut args = pt(&s["logical"], &event_target);
                        args["button"] = s["button"].clone(); args["count"] = s["count"].clone(); args["modifiers"] = s["modifiers"].clone(); args["long_press_ms"] = s["long_press_ms"].clone();
                        plan.push(("click".into(), args));
                    } else { unreplayable.push("click (recorded summary lacks modifiers/long_press_ms; cannot reconstruct exactly)".into()); }
                }
                "key" => {
                    if s.get("keys").is_some() && has("repeat") { plan.push(("key".into(), serde_json::json!({"keys": s["keys"], "repeat": s["repeat"], "target": event_target}))); } else { unreplayable.push("key (recorded summary lacks keys/repeat)".into()); }
                }
                "key_hold" => {
                    if s.get("keys").is_some() && s.get("pressed").is_some() { plan.push(("key_hold".into(), serde_json::json!({"keys": s["keys"], "pressed": s["pressed"], "target": event_target}))); } else { unreplayable.push("key_hold (recorded summary lacks keys/pressed)".into()); }
                }
                "mouse_button" => {
                    if s.get("button").is_some() && s.get("pressed").is_some() { plan.push(("mouse_button".into(), serde_json::json!({"button": s["button"], "pressed": s["pressed"], "target": event_target}))); } else { unreplayable.push("mouse_button (recorded summary lacks button/pressed)".into()); }
                }
                "drag" => {
                    if s.get("from").map(|v| v.is_array()).unwrap_or(false) && s.get("to").map(|v| v.is_array()).unwrap_or(false) && has("button") && has("waypoint_points") && has("duration_ms") && has("dwell_ms") {
                        let wps: Vec<serde_json::Value> = s["waypoint_points"].as_array().cloned().unwrap_or_default().iter().map(|w| pt(w, &event_target)).collect();
                        plan.push(("drag".into(), serde_json::json!({"from": pt(&s["from"], &event_target), "to": pt(&s["to"], &event_target), "waypoints": wps, "button": s["button"], "duration_ms": s["duration_ms"], "dwell_ms": s["dwell_ms"]})));
                    } else { unreplayable.push("drag (recorded summary lacks button/waypoints/duration/dwell; cannot reconstruct exactly)".into()); }
                }
                "scroll" => {
                    if has("dx") && has("dy") && has("smooth") && has("logical") {
                        let mut args = serde_json::json!({"dx": s["dx"], "dy": s["dy"], "smooth": s["smooth"], "target": event_target});
                        if s["logical"].is_array() { args["x"] = s["logical"][0].clone(); args["y"] = s["logical"][1].clone(); args["space"] = serde_json::json!("logical"); }
                        plan.push(("scroll".into(), args));
                    } else { unreplayable.push("scroll (recorded summary lacks dx/dy/smooth/position)".into()); }
                }
                "touch_tap" => {
                    if s["logical"].is_array() && has("hold_ms") { let mut a = pt(&s["logical"], &event_target); a["hold_ms"] = s["hold_ms"].clone(); plan.push(("touch_tap".into(), a)); } else { unreplayable.push("touch_tap (recorded summary lacks logical/hold_ms)".into()); }
                }
                "touch_swipe" => {
                    if s["points_logical"].is_array() && has("duration_ms") {
                        let pts: Vec<serde_json::Value> = s["points_logical"].as_array().unwrap().iter().map(|w| pt(w, &event_target)).collect();
                        plan.push(("touch_swipe".into(), serde_json::json!({"points": pts, "duration_ms": s["duration_ms"]})));
                    } else { unreplayable.push("touch_swipe (recorded summary lacks points/duration)".into()); }
                }
                "touch_pinch" => {
                    if s["center"].is_array() && has("start_distance") && has("end_distance") && has("duration_ms") {
                        plan.push(("touch_pinch".into(), serde_json::json!({"center": pt(&s["center"], &event_target), "start_distance": s["start_distance"], "end_distance": s["end_distance"], "duration_ms": s["duration_ms"]})));
                    } else { unreplayable.push("touch_pinch (recorded summary lacks center/distances/duration)".into()); }
                }
                "type_text" => skipped.push("type_text (text redacted in trace; non-replayable)".into()),
                other => unreplayable.push(format!("{other} (not exactly reconstructable from its recorded summary)")),
            }
        }
        if p.dry_run == Some(true) {
            return Ok(json_result(&serde_json::json!({"dry_run": true, "recorded_target": recorded_target, "recorded_targets": distinct_targets, "planned": plan.iter().map(|(t, a)| serde_json::json!({"tool": t, "arguments": a})).collect::<Vec<_>>(), "skipped_non_replayable": skipped, "unreplayable": unreplayable})));
        }
        if !unreplayable.is_empty() { return Err(err(format!("trace contains events that cannot be reconstructed exactly {unreplayable:?}; refusing replay before dispatching anything (no partial replay of a different action sequence)"))); }
        if recorded_target.is_empty() { return Err(err("trace records no target; refusing replay")); }
        if distinct_targets.len() > 1 { return Err(err(format!("trace spans multiple recorded targets {distinct_targets:?}; refusing replay — replay only onto the single recorded target '{recorded_target}' (record a new trace per target)"))); }
        // The recorded target must still exist; there is no fallback to
        // another desktop (in particular, never to the host by default).
        let target = self.active_target(Some(&recorded_target)).map_err(|_| err(format!("recorded target '{recorded_target}' is not available; refusing replay (no fallback to another desktop)")))?;
        // Layout check: recorded outputs (trace header) must match current.
        let recorded_outputs = header.as_ref().map(|h| h["outputs"].clone()).filter(|o| !o.is_null());
        let recorded_outputs = match recorded_outputs {
            Some(o) => o,
            None => return Err(err("trace has no recorded output-layout header (recorded before layout headers existed, or by hand); refusing replay because the layout cannot be verified")),
        };
        let current_outputs = serde_json::to_value(&wayland::discover(&target).map_err(err)?.outputs).unwrap_or_default();
        if current_outputs != recorded_outputs {
            return Err(err(format!("output layout changed since the trace was recorded; refusing replay. recorded: {recorded_outputs} current: {current_outputs}")));
        }
        // Tools that take no target parameter dispatch on the active target:
        // they may only run when the active target IS the recorded one.
        if plan.iter().any(|(t, _)| TARGETLESS.contains(&t.as_str())) {
            let active = self.active_target(None)?;
            if active.id != recorded_target {
                return Err(err(format!("trace contains tools that take no target parameter (screenshot/a11y); they dispatch on the active target, which is '{}', not the recorded '{}'. target_select the recorded target first; refusing to replay onto a different desktop", active.id, recorded_target)));
            }
        }
        // The tool filter applies to replay exactly as to direct dispatch,
        // checked for the whole plan before anything executes.
        for (tool, _) in &plan {
            if !self.tool_router.has_route(tool) { return Err(err(format!("tool '{tool}' is filtered out by --tools/--exclude-tools and cannot be dispatched via trace_replay; refusing before any replay step executes"))); }
        }
        // Preflight the WHOLE plan before any input: every step must still
        // validate (including that any referenced shot_id is the retained
        // capture it was recorded with — shot ids carry a per-process nonce,
        // so a different process's capture can never satisfy this,
        // transcribe-l7v.2) and match the recorded target/layout generation.
        for (idx, (tool, args)) in plan.iter().enumerate() {
            if !Self::batch_allowed(tool) { return Err(err(format!("replay step {} ({tool}) is not a dispatchable action tool; refusing before any replay step executes", idx + 1))); }
            let step = BatchStep { tool: tool.clone(), arguments: Some(args.clone()) };
            if let Err(e) = self.validate_batch_step(&step) { return Err(err(format!("replay step {} ({tool}) no longer validates, refusing before any replay step executes: {e:?}", idx + 1))); }
        }
        let mut executed = 0usize;
        for (tool, args) in &plan {
            self.dispatch_action(tool, args.clone()).await.map_err(|e| err(format!("replay stopped at step {} ({tool}): {e:?}; completed prefix stays executed, nothing is auto-replayed", executed + 1)))?;
            executed += 1;
        }
        Ok(json_result(&serde_json::json!({"replayed": executed, "skipped_non_replayable": skipped, "target": target.id})))
    }

    /// Start an ffmpeg screen recording (runs independently of this response).
    #[tool(description = "Start recording the active target output to an mp4 under the private data dir via ffmpeg. Runs independently of the tool response lifetime; record_stop reports actual duration/frames and encoder errors. Recording can contain secrets and stays local.", annotations(destructive_hint = true))]
    async fn record_start(&self, Parameters(p): Parameters<RecordStartParams>) -> Result<CallToolResult, ErrorData> {
        let target = self.active_target(None)?;
        let name = sanitize_name(p.name.as_deref().unwrap_or("recording")).map_err(err)?;
        let id = format!("{name}-{}", now_ms());
        let dir = data_dir().join("recordings");
        std::fs::create_dir_all(&dir).map_err(|e| err(e.to_string()))?;
        let path = dir.join(format!("{id}.mp4"));
        let fps = p.fps.unwrap_or(10).min(30);
        let maxd = p.max_duration_ms.unwrap_or(120_000).min(600_000);
        // Default behaviour shows the indicator (indicator_show runs for
        // actions unless suppressed), so the reported default is true.
        let include_indicator = p.include_indicator.unwrap_or(true);
        if !include_indicator {
            // Hide (with compositor ack) any already-visible indicator BEFORE
            // record::start captures its first frame (transcribe-l7v.12).
            self.indicator_hide();
        }
        let rec = tokio::task::spawn_blocking(move || crate::record::start(id.clone(), target, path, fps, maxd, p.output)).await.map_err(|e| err(e.to_string()))?.map_err(err)?;
        let rid = rec.id.clone(); let rpath = rec.path.clone();
        // include_indicator:false is honoured: indicator_show is suppressed
        // while such a recording is active (screenshots hide it anyway).
        if !include_indicator { self.record_no_indicator.lock().unwrap().insert(rid.clone()); }
        self.recordings.lock().unwrap().insert(rid.clone(), rec);
        Ok(json_result(&serde_json::json!({"recording_id": rid, "path": rpath, "include_indicator": include_indicator, "note": "screenshots exclude the activity indicator; with include_indicator:false the indicator is hidden before the first frame and suppressed during recording"})))
    }

    /// Stop a recording and report actual duration/frames/encoder errors.
    #[tool(description = "Stop a recording started by record_start. Returns path, actual duration, captured frames and any encoder/capture error.", annotations(destructive_hint = true))]
    async fn record_stop(&self, Parameters(p): Parameters<RecordStopParams>) -> Result<CallToolResult, ErrorData> {
        self.record_no_indicator.lock().unwrap().remove(&p.recording_id);
        let mut rec = self.recordings.lock().unwrap().remove(&p.recording_id).ok_or_else(|| err(format!("unknown recording '{}'", p.recording_id)))?;
        let duration_ms = rec.started.elapsed().as_millis();
        rec.stop_flag.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(handle) = rec.handle.take() { let _ = tokio::task::spawn_blocking(move || handle.join()).await; }
        let frames = rec.frames.load(std::sync::atomic::Ordering::Relaxed);
        let error = rec.error.lock().unwrap().clone();
        Ok(json_result(&serde_json::json!({"recording_id": rec.id, "path": rec.path, "duration_ms": duration_ms, "frames_captured": frames, "fps_configured": rec.fps, "error": error})))
    }
}

pub fn draw_mark(img: &mut RgbaImage, word: &crate::ocr::OcrWord, number: usize) {
    let [x, y, w, h] = word.box_px;
    let color = image::Rgba([255, 0, 255, 255]);
    for px in x..(x + w).min(img.width()) { for t in 0..3 { if y + t < img.height() { img.put_pixel(px, y + t, color); } if y + h > t && y + h - 1 - t < img.height() { img.put_pixel(px, y + h - 1 - t, color); } } }
    for py in y..(y + h).min(img.height()) { for t in 0..3 { if x + t < img.width() { img.put_pixel(x + t, py, color); } if x + w > t && x + w - 1 - t < img.width() { img.put_pixel(x + w - 1 - t, py, color); } } }
    let (iw, ih, stride) = (img.width(), img.height(), img.width() * 4);
    crate::indicator::draw_text_at(img.as_mut(), stride, iw, ih, x, y.saturating_sub(16), 2, &number.to_string(), [255, 0, 255, 255]);
}

pub fn draw_grid(img: &mut RgbaImage, t: &crate::coords::ShotTransform) {
    // Vertical/horizontal lines every ~100 logical units, labelled by coordinate in the image corner text is not rendered (no font); lines only + metadata carries mapping.
    let step_logical = 100.0f64;
    let mut lx = (t.crop_logical.x as f64 / step_logical).ceil() * step_logical;
    let color = image::Rgba([255, 0, 255, 160]);
    while lx < (t.crop_logical.x + t.crop_logical.width) as f64 {
        if let Ok((px, _)) = t.logical_to_pixel(lx, t.crop_logical.y as f64) {
            let x = px.round() as i32;
            for y in 0..img.height() as i32 { if x >= 0 && (x as u32) < img.width() { img.put_pixel(x as u32, y as u32, color); } }
        }
        lx += step_logical;
    }
    let mut ly = (t.crop_logical.y as f64 / step_logical).ceil() * step_logical;
    while ly < (t.crop_logical.y + t.crop_logical.height) as f64 {
        if let Ok((_, py)) = t.logical_to_pixel(t.crop_logical.x as f64, ly) {
            let y = py.round() as i32;
            for x in 0..img.width() { if y >= 0 && (y as u32) < img.height() { img.put_pixel(x, y as u32, color); } }
        }
        ly += step_logical;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn encode_image_preserves_aspect_ratio() {
        let img = RgbaImage::new(1920, 1080);
        let (_, _, out) = encode_image(&img, "png", Some(1024)).unwrap();
        assert_eq!((out.width(), out.height()), (1024, 576));
        let img = RgbaImage::new(100, 50);
        let (_, _, out) = encode_image(&img, "png", Some(1024)).unwrap();
        assert_eq!((out.width(), out.height()), (100, 50), "no upscale when within bound");
    }
    #[test]
    fn trace_args_redact_text_everywhere() {
        let v = serde_json::json!({"text": "hunter2", "steps": [{"text": "also secret", "x": 1}]});
        let r = redact_trace_args(&v);
        assert!(!r.to_string().contains("hunter2") && !r.to_string().contains("also secret"));
        assert_eq!(r["text"]["chars"], 7);
        assert_eq!(r["steps"][0]["x"], 1);
    }
    #[test]
    fn trace_args_redact_value_and_content_keys() {
        // Batch a11y_set_value step: `value` is text-entry, stored only as a count.
        let v = serde_json::json!({"tool": "a11y_set_value", "arguments": {"ref_id": "r1", "value": "hunter2"}, "content": "hello", "n": 3, "numeric_value": 42});
        let r = redact_trace_args(&v);
        assert!(!r.to_string().contains("hunter2") && !r.to_string().contains("hello"), "{r}");
        assert_eq!(r["arguments"]["value"], serde_json::json!({"redacted": true, "chars": 7}));
        assert_eq!(r["content"]["chars"], 5);
        assert_eq!(r["numeric_value"], 42, "non-string values are not redacted");
        assert_eq!(r["arguments"]["ref_id"], "r1");
    }
    #[test]
    fn arg_target_prefers_explicit_over_nested() {
        assert_eq!(Server::arg_target(&serde_json::json!({"target": "sandbox-1", "x": 1})).as_deref(), Some("sandbox-1"));
        assert_eq!(Server::arg_target(&serde_json::json!({"from": {"target": "sandbox-2"}})).as_deref(), Some("sandbox-2"));
        assert_eq!(Server::arg_target(&serde_json::json!({"points": [{"target": "sandbox-3"}]})).as_deref(), Some("sandbox-3"));
        assert_eq!(Server::arg_target(&serde_json::json!({"x": 1})), None);
    }
    #[tokio::test]
    async fn trace_suppression_is_task_local() {
        assert!(!trace_suppressed());
        TRACE_SUPPRESSED.scope(std::cell::Cell::new(true), async {
            assert!(trace_suppressed());
            // A concurrent task (a parallel tool call) is NOT suppressed.
            let other = tokio::spawn(async { trace_suppressed() }).await.unwrap();
            assert!(!other);
        }).await;
        assert!(!trace_suppressed());
    }
    #[test]
    fn durations_are_capped() {
        assert_eq!(capped_duration(Some(u64::MAX), 30), MAX_INPUT_DURATION_MS);
        assert_eq!(capped_duration(None, 30), 30);
        assert_eq!(capped_duration(Some(5), 30), 5);
    }
    #[test]
    fn scroll_params_are_explicit_not_flattened() {
        // Only one of x/y is a deserialization success now (the handler
        // rejects it explicitly instead of silently dropping the point).
        let p: ScrollParams = serde_json::from_value(serde_json::json!({"dy": 3, "target": "sandbox-1"})).unwrap();
        assert_eq!(p.target.as_deref(), Some("sandbox-1"));
        assert!(p.x.is_none() && p.y.is_none());
        let p: ScrollParams = serde_json::from_value(serde_json::json!({"x": 10, "dy": 3})).unwrap();
        assert_eq!(p.x, Some(10.0));
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for Server {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(rmcp::model::Implementation::new("wayland-mcp", env!("CARGO_PKG_VERSION")))
            .with_instructions("Wayland computer-use server. Observe (screenshot/desktop_info) before acting; use shot_id screenshot-space coordinates or explicit logical coordinates. Unsupported capabilities are reported explicitly, never silently faked.")
    }
}
