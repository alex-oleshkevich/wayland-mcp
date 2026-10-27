use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LogicalRect { pub x: i32, pub y: i32, pub width: i32, pub height: i32 }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutputInfo {
    pub name: String,
    pub logical: LogicalRect,
    pub physical_width: u32,
    pub physical_height: u32,
    pub scale: f64,
    pub transform: String,
    pub make: Option<String>,
    pub model: Option<String>,
    /// wl_output transform enum value (0 Normal, 1 90, 2 180, 3 270, 4 Flipped, 5 Flipped90, 6 Flipped180, 7 Flipped270)
    pub transform_code: i32,
}

/// Exact image-pixel -> desktop-logical mapping for one captured image.
/// `crop_logical` is the logical rect the image pixels cover (full output or a crop of it).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShotTransform {
    pub shot_id: String,
    pub target_id: String,
    pub output: String,
    pub image_width: u32,
    pub image_height: u32,
    pub crop_logical: LogicalRect,
    /// Exact (fractional) logical rect covered by the image, set for crops of a
    /// parent shot where rounding crop_logical to integers would lose up to a
    /// logical pixel at fractional scales. None = crop_logical is exact.
    #[serde(default)]
    pub crop_exact: Option<[f64; 4]>,
    pub layout_generation: u64,
    pub captured_at_ms: u128,
}
impl ShotTransform {
    /// Center-of-pixel mapping; never rounds a pixel corner into a different logical cell silently.
    fn rect_f64(&self) -> (f64, f64, f64, f64) {
        match self.crop_exact {
            Some([x, y, w, h]) => (x, y, w, h),
            None => (self.crop_logical.x as f64, self.crop_logical.y as f64, self.crop_logical.width as f64, self.crop_logical.height as f64),
        }
    }
    /// Center-of-pixel mapping; never rounds a pixel corner into a different logical cell silently.
    pub fn pixel_to_logical(&self, px: f64, py: f64) -> Result<(f64, f64), String> {
        if px < 0.0 || py < 0.0 || px >= self.image_width as f64 || py >= self.image_height as f64 {
            return Err(format!("pixel ({px},{py}) outside image {}x{}", self.image_width, self.image_height));
        }
        let (cx, cy, cw, ch) = self.rect_f64();
        Ok((
            cx + (px + 0.5) / self.image_width as f64 * cw,
            cy + (py + 0.5) / self.image_height as f64 * ch,
        ))
    }
    pub fn logical_to_pixel(&self, lx: f64, ly: f64) -> Result<(f64, f64), String> {
        let (cx, cy, cw, ch) = self.rect_f64();
        if lx < cx || ly < cy || lx >= cx + cw || ly >= cy + ch {
            return Err(format!("logical ({lx},{ly}) outside shot rect {:?}", self.crop_logical));
        }
        Ok((
            (lx - cx) / cw * self.image_width as f64 - 0.5,
            (ly - cy) / ch * self.image_height as f64 - 0.5,
        ))
    }
    /// Crop a sub-transform; the child covers `sub` in image pixels of this shot.
    pub fn cropped(&self, new_id: String, px: u32, py: u32, w: u32, h: u32) -> Result<ShotTransform, String> {
        // Checked arithmetic: tool-derived px/w can be near u32::MAX and must not wrap.
        let inside = w > 0 && h > 0
            && px.checked_add(w).map_or(false, |e| e <= self.image_width)
            && py.checked_add(h).map_or(false, |e| e <= self.image_height);
        if !inside { return Err("crop outside image".into()); }
        let (cx, cy, cw, ch) = self.rect_f64();
        // Exact fractional logical rect (pixel-edge mapping); crop_logical keeps a
        // rounded integer view for display/back-compat, crop_exact drives mapping.
        let lx = cx + px as f64 / self.image_width as f64 * cw;
        let ly = cy + py as f64 / self.image_height as f64 * ch;
        let lw = w as f64 / self.image_width as f64 * cw;
        let lh = h as f64 / self.image_height as f64 * ch;
        Ok(ShotTransform { shot_id: new_id, target_id: self.target_id.clone(), output: self.output.clone(),
            image_width: w, image_height: h,
            crop_logical: LogicalRect { x: lx.round() as i32, y: ly.round() as i32, width: lw.round().max(1.0) as i32, height: lh.round().max(1.0) as i32 },
            crop_exact: Some([lx, ly, lw, lh]),
            layout_generation: self.layout_generation, captured_at_ms: self.captured_at_ms })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn t() -> ShotTransform { ShotTransform { shot_id: "s".into(), target_id: "host".into(), output: "DP-2".into(), image_width: 3072, image_height: 1728, crop_logical: LogicalRect { x: -3072, y: 0, width: 2458, height: 1382 }, crop_exact: None, layout_generation: 1, captured_at_ms: 0 } }
    #[test] fn negative_origin_maps() { let (x, y) = t().pixel_to_logical(0.0, 0.0).unwrap(); assert!(x < -3071.0 && x > -3072.0); assert!(y >= 0.0); }
    #[test] fn roundtrip() { let tr = t(); let (lx, ly) = tr.pixel_to_logical(100.0, 200.0).unwrap(); let (px, py) = tr.logical_to_pixel(lx, ly).unwrap(); assert!((px - 100.0).abs() < 1e-6 && (py - 200.0).abs() < 1e-6); }
    #[test] fn out_of_bounds_rejected() { assert!(t().pixel_to_logical(3072.0, 0.0).is_err()); assert!(t().logical_to_pixel(0.0, 0.0).is_err()); }
    #[test] fn crop_stays_inside() { let c = t().cropped("c".into(), 100, 100, 500, 400).unwrap(); assert_eq!((c.image_width, c.image_height), (500, 400)); assert!(c.crop_logical.x >= -3072); assert!(t().cropped("c".into(), 3000, 0, 100, 10).is_err()); }
    #[test] fn crop_huge_values_rejected() { assert!(t().cropped("c".into(), 3, 0, u32::MAX, 10).is_err()); assert!(t().cropped("c".into(), u32::MAX, 0, 10, 10).is_err()); }
    #[test] fn crop_fractional_scale_exact() {
        // scale 1.25-like: parent covers 8.75 logical px in 7 image px etc. Use direct exact rect.
        let parent = ShotTransform { image_width: 10, image_height: 10, crop_logical: LogicalRect { x: 0, y: 0, width: 8, height: 8 }, crop_exact: Some([0.0, 0.0, 8.0, 8.0]), ..t() };
        let c = parent.cropped("c".into(), 3, 0, 7, 10).unwrap();
        // pixel 0 centre must map to lx + 0.5/7*lw = 2.4 + 0.4 = 2.8, not the rounded 2.43.
        let (x, _) = c.pixel_to_logical(0.0, 0.0).unwrap();
        assert!((x - 2.8).abs() < 1e-9, "got {x}");
    }
}
