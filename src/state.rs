use crate::{coords::{OutputInfo, ShotTransform}, wayland::TargetSpec};
use image::RgbaImage;
use std::{collections::{HashMap, VecDeque}, sync::{atomic::{AtomicU64, Ordering}, Arc}};

pub const MAX_SHOTS: usize = 32;
pub struct Shot { pub transform: ShotTransform, pub image: Arc<RgbaImage> }

#[derive(Clone, Default)]
pub struct TargetLayout { pub generation: u64, pub outputs: Vec<OutputInfo> }

pub struct Session {
    pub targets: HashMap<String, TargetSpec>,
    pub active_target: String,
    pub shots: VecDeque<Shot>,
    pub shot_counter: u64,
    /// Per-process nonce embedded in every shot_id, so an id minted by one
    /// server process can never collide with (and be rebound to) a capture
    /// made by another process (transcribe-l7v.2).
    pub process_nonce: String,
    /// Layout generation/outputs tracked PER TARGET (transcribe-l7v.9):
    /// discovery on one target never invalidates another target's shots.
    pub layouts: HashMap<String, TargetLayout>,
}
static NONCE_COUNTER: AtomicU64 = AtomicU64::new(0);
fn fresh_nonce() -> String {
    let n = NONCE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0);
    format!("{:x}{:x}{:x}", std::process::id(), nanos, n)
}
impl Session {
    pub fn new() -> Self {
        let mut targets = HashMap::new();
        if let Ok(h) = TargetSpec::host() { targets.insert("host".into(), h); }
        Self { targets, active_target: "host".into(), shots: VecDeque::new(), shot_counter: 0, process_nonce: fresh_nonce(), layouts: HashMap::new() }
    }
    pub fn target(&self, id: Option<&str>) -> Result<TargetSpec, String> {
        let id = id.unwrap_or(&self.active_target);
        self.targets.get(id).cloned().ok_or_else(|| format!("unknown target '{id}'; known: {:?}", self.targets.keys().collect::<Vec<_>>()))
    }
    pub fn layout_generation(&self, target_id: &str) -> u64 {
        self.layouts.get(target_id).map(|l| l.generation).unwrap_or(1)
    }
    /// Note discovered outputs for ONE target. Returns true if that target's
    /// layout changed (its generation was bumped).
    pub fn note_outputs(&mut self, target_id: &str, outputs: &[OutputInfo]) -> bool {
        let entry = self.layouts.entry(target_id.to_string()).or_insert_with(|| TargetLayout { generation: 1, outputs: Vec::new() });
        let changed = entry.outputs != outputs.to_vec();
        if changed { entry.generation += 1; entry.outputs = outputs.to_vec(); }
        changed
    }
    pub fn store_shot(&mut self, mut transform: ShotTransform, image: RgbaImage) -> ShotTransform {
        self.shot_counter += 1;
        transform.shot_id = format!("shot-{}-{}", self.process_nonce, self.shot_counter);
        transform.layout_generation = self.layout_generation(&transform.target_id);
        let t = transform.clone();
        self.shots.push_back(Shot { transform, image: Arc::new(image) });
        while self.shots.len() > MAX_SHOTS { self.shots.pop_front(); }
        t
    }
    pub fn get_shot(&self, id: &str) -> Result<&Shot, String> {
        self.shots.iter().rev().find(|s| s.transform.shot_id == id)
            .ok_or_else(|| format!("screenshot '{id}' not retained (retention: last {MAX_SHOTS} shots); capture a new screenshot"))
    }
    /// Resolve a coordinate argument: shot-space pixel or explicit desktop logical.
    pub fn resolve_point(&self, shot_id: Option<&str>, x: f64, y: f64, space: &str, target_id: &str) -> Result<(f64, f64, Option<String>), String> {
        match space {
            "logical" => Ok((x, y, None)),
            "screenshot" => {
                let id = shot_id.ok_or("shot_id is required for screenshot-space coordinates")?;
                let shot = self.get_shot(id)?;
                if shot.transform.target_id != target_id { return Err(format!("shot {id} belongs to target '{}', active target is '{target_id}'; refusing to rebind it", shot.transform.target_id)); }
                let current = self.layout_generation(target_id);
                if shot.transform.layout_generation != current { return Err(format!("shot {id} is from layout generation {}, current is {}; capture a new screenshot", shot.transform.layout_generation, current)); }
                let (lx, ly) = shot.transform.pixel_to_logical(x, y)?;
                Ok((lx, ly, Some(id.to_string())))
            }
            other => Err(format!("unknown coordinate space '{other}' (expected 'screenshot' or 'logical')")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coords::LogicalRect;
    fn out(name: &str, w: i32) -> OutputInfo {
        OutputInfo { name: name.into(), logical: LogicalRect { x: 0, y: 0, width: w, height: 100 }, physical_width: w as u32, physical_height: 100, scale: 1.0, transform: "normal".into(), make: None, model: None, transform_code: 0 }
    }
    fn transform(target: &str) -> ShotTransform {
        ShotTransform { shot_id: String::new(), target_id: target.into(), output: "O".into(), image_width: 100, image_height: 100, crop_logical: LogicalRect { x: 0, y: 0, width: 100, height: 100 }, crop_exact: None, layout_generation: 0, captured_at_ms: 0 }
    }
    #[test]
    fn other_target_discovery_does_not_stale_shot() {
        let mut s = Session::new();
        s.note_outputs("host", &[out("H", 100)]);
        let shot = s.store_shot(transform("host"), RgbaImage::new(100, 100));
        // Discovery on a different target with a different layout.
        s.note_outputs("sandbox-1", &[out("S", 50)]);
        s.note_outputs("sandbox-1", &[out("S", 60)]);
        assert!(s.resolve_point(Some(&shot.shot_id), 10.0, 10.0, "screenshot", "host").is_ok(), "host shot must survive sandbox layout changes");
        // A real layout change on the shot's OWN target still stales it.
        s.note_outputs("host", &[out("H", 200)]);
        assert!(s.resolve_point(Some(&shot.shot_id), 10.0, 10.0, "screenshot", "host").is_err());
    }
    #[test]
    fn shot_ids_are_unique_across_sessions() {
        let a = Session::new();
        let b = Session::new();
        assert_ne!(a.process_nonce, b.process_nonce);
        let mut a = a;
        let shot = a.store_shot(transform("host"), RgbaImage::new(1, 1));
        assert!(shot.shot_id.starts_with("shot-") && shot.shot_id.contains(&a.process_nonce));
        // A fresh session cannot resolve the other session's id by rebinding.
        assert!(b.get_shot(&shot.shot_id).is_err());
        assert!(b.resolve_point(Some(&shot.shot_id), 0.0, 0.0, "screenshot", "host").is_err());
    }
}
