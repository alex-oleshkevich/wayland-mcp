use image::RgbaImage;
use serde::Serialize;

#[derive(Debug, Serialize, Clone, PartialEq)]
pub struct ChangedRegion { pub x: u32, pub y: u32, pub width: u32, pub height: u32 }
#[derive(Debug, Serialize)]
pub struct DiffResult { pub changed_ratio: f64, pub changed_pixels: u64, pub total_pixels: u64, pub regions: Vec<ChangedRegion>, pub size_mismatch: Option<String> }

/// Compare two same-sized images. Size mismatch is explicit, never silently rescaled.
pub fn diff_images(a: &RgbaImage, b: &RgbaImage, threshold: u8, ignored: &[ChangedRegion]) -> DiffResult {
    if a.dimensions() != b.dimensions() {
        return DiffResult { changed_ratio: 1.0, changed_pixels: 0, total_pixels: 0, regions: vec![],
            size_mismatch: Some(format!("size mismatch: {}x{} vs {}x{} (regression comparisons are never rescaled)", a.width(), a.height(), b.width(), b.height())) };
    }
    let cell = 32u32;
    let cols = a.width().div_ceil(cell);
    let mut changed_cells = vec![false; (cols * a.height().div_ceil(cell)) as usize];
    let mut changed = 0u64;
    for y in 0..a.height() {
        for x in 0..a.width() {
            if ignored.iter().any(|r| x >= r.x && x < r.x.saturating_add(r.width) && y >= r.y && y < r.y.saturating_add(r.height)) { continue; }
            let pa = a.get_pixel(x, y).0; let pb = b.get_pixel(x, y).0;
            let delta = pa.iter().zip(pb.iter()).map(|(u, v)| u.abs_diff(*v)).max().unwrap_or(0);
            if delta > threshold {
                changed += 1;
                changed_cells[(y / cell * cols + x / cell) as usize] = true;
            }
        }
    }
    // Merge horizontally/vertically adjacent changed cells into regions (simple row runs merged by identical x-span).
    let mut regions: Vec<ChangedRegion> = Vec::new();
    let rows = a.height().div_ceil(cell);
    for cy in 0..rows {
        let mut cx = 0u32;
        while cx < cols {
            if changed_cells[(cy * cols + cx) as usize] {
                let start = cx;
                while cx < cols && changed_cells[(cy * cols + cx) as usize] { cx += 1; }
                let region = ChangedRegion { x: start * cell, y: cy * cell, width: ((cx - start) * cell).min(a.width() - start * cell), height: cell.min(a.height() - cy * cell) };
                if let Some(prev) = regions.iter_mut().find(|r: &&mut ChangedRegion| r.x == region.x && r.width == region.width && r.y + r.height == region.y) {
                    prev.height += region.height;
                } else { regions.push(region); }
            } else { cx += 1; }
        }
    }
    let total = (a.width() * a.height()) as u64;
    DiffResult { changed_ratio: changed as f64 / total.max(1) as f64, changed_pixels: changed, total_pixels: total, regions, size_mismatch: None }
}

pub fn highlight_diff(a: &RgbaImage, b: &RgbaImage, threshold: u8, ignored: &[ChangedRegion]) -> RgbaImage {
    let mut out = a.clone();
    if a.dimensions() != b.dimensions() { return out; }
    for y in 0..a.height() { for x in 0..a.width() {
        if ignored.iter().any(|r| x >= r.x && x < r.x.saturating_add(r.width) && y >= r.y && y < r.y.saturating_add(r.height)) { continue; }
        let pa = a.get_pixel(x, y).0; let pb = b.get_pixel(x, y).0;
        if pa.iter().zip(pb.iter()).map(|(u, v)| u.abs_diff(*v)).max().unwrap_or(0) > threshold { out.put_pixel(x, y, image::Rgba([255, 0, 0, 255])); }
    } }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn identical_no_change() { let a = RgbaImage::new(64, 64); let d = diff_images(&a, &a.clone(), 8, &[]); assert_eq!(d.changed_ratio, 0.0); assert!(d.regions.is_empty()); }
    #[test] fn size_mismatch_explicit() { let d = diff_images(&RgbaImage::new(10, 10), &RgbaImage::new(20, 10), 8, &[]); assert!(d.size_mismatch.is_some()); }
    #[test] fn local_change_region() {
        let a = RgbaImage::new(64, 64); let mut b = a.clone();
        for y in 0..10 { for x in 0..10 { b.put_pixel(x, y, image::Rgba([255, 255, 255, 255])); } }
        let d = diff_images(&a, &b, 8, &[]);
        assert!(d.changed_pixels == 100); assert_eq!(d.regions.len(), 1); assert_eq!((d.regions[0].x, d.regions[0].y), (0, 0));
    }
    #[test] fn ignored_region_excluded() {
        let a = RgbaImage::new(64, 64); let mut b = a.clone(); b.put_pixel(5, 5, image::Rgba([255, 0, 0, 255]));
        let d = diff_images(&a, &b, 8, &[ChangedRegion { x: 0, y: 0, width: 32, height: 32 }]);
        assert_eq!(d.changed_pixels, 0);
    }
}
