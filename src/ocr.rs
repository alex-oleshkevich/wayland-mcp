//! CPU OCR via ocrs. Models live in $WAYLAND_MCP_OCR_DIR or $XDG_CACHE_HOME/ocrs
//! (text-detection.rten / text-recognition.rten). They are downloaded ONLY on an OCR
//! request that opts into downloading; `doctor` never triggers this.
use image::RgbaImage;
use serde::Serialize;
use std::path::PathBuf;

#[derive(Debug, Serialize, Clone)]
pub struct OcrWord { pub text: String, pub box_px: [u32; 4], pub center_px: [f64; 2] }

pub fn model_dir() -> PathBuf {
    if let Ok(d) = std::env::var("WAYLAND_MCP_OCR_DIR") { return PathBuf::from(d); }
    let cache = std::env::var("XDG_CACHE_HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".cache"));
    cache.join("ocrs")
}
pub fn model_paths() -> (PathBuf, PathBuf) { let d = model_dir(); (d.join("text-detection.rten"), d.join("text-recognition.rten")) }

pub fn ensure_models(download: bool) -> Result<(PathBuf, PathBuf), String> {
    let (det, rec) = model_paths();
    if det.exists() && rec.exists() { return Ok((det, rec)); }
    if !download {
        return Err(format!("OCR models not installed at {} (missing {}). Re-run with download_models:true to fetch them from the ocrs model host, or set WAYLAND_MCP_OCR_DIR to a directory containing text-detection.rten and text-recognition.rten. The bundled ocrs alphabet covers Latin scripts; other scripts need a matching custom model/alphabet.", model_dir().display(),
            [(!det.exists()).then_some("text-detection.rten"), (!rec.exists()).then_some("text-recognition.rten")].into_iter().flatten().collect::<Vec<_>>().join(", ")));
    }
    std::fs::create_dir_all(model_dir()).map_err(|e| format!("create OCR model dir: {e}"))?;
    for (url, path) in [("https://ocrs-models.s3-accelerate.amazonaws.com/text-detection.rten", &det), ("https://ocrs-models.s3-accelerate.amazonaws.com/text-recognition.rten", &rec)] {
        let tmp = path.with_extension("rten.part");
        let status = std::process::Command::new("curl").args(["-fL", "--max-time", "300", "-o"]).arg(&tmp).arg(url).status().map_err(|e| format!("curl for OCR model: {e}"))?;
        if !status.success() { let _ = std::fs::remove_file(&tmp); return Err(format!("downloading OCR model from {url} failed (curl exit {status})")); }
        std::fs::rename(&tmp, path).map_err(|e| format!("install OCR model: {e}"))?;
    }
    Ok((det, rec))
}

pub fn run_ocr(img: &RgbaImage, download_models: bool) -> Result<Vec<OcrWord>, String> {
    let (det, rec) = ensure_models(download_models)?;
    let det_model = rten::Model::load_file(&det).map_err(|e| format!("load detection model: {e}"))?;
    let rec_model = rten::Model::load_file(&rec).map_err(|e| format!("load recognition model: {e}"))?;
    let engine = ocrs::OcrEngine::new(ocrs::OcrEngineParams { detection_model: Some(det_model), recognition_model: Some(rec_model), ..Default::default() })
        .map_err(|e| format!("init OCR engine: {e}"))?;
    // ocrs consumes RGB (3-channel HWC).
    use ocrs::TextItem as _;
    let mut rgb = Vec::with_capacity((img.width() * img.height() * 3) as usize);
    for p in img.pixels() { rgb.extend_from_slice(&[p.0[0], p.0[1], p.0[2]]); }
    let source = ocrs::ImageSource::from_bytes(&rgb, img.dimensions()).map_err(|e| format!("OCR image source: {e}"))?;
    let input = engine.prepare_input(source).map_err(|e| format!("OCR prepare: {e}"))?;
    let word_rects = engine.detect_words(&input).map_err(|e| format!("OCR detect: {e}"))?;
    let lines = engine.find_text_lines(&input, &word_rects);
    let recognized = engine.recognize_text(&input, &lines).map_err(|e| format!("OCR recognize: {e}"))?;
    let mut words = Vec::new();
    for line in recognized.into_iter().flatten() {
        for word in line.words() {
            let text = word.to_string();
            if text.is_empty() { continue; }
            // Union of character rects gives the word box in image pixels.
            let chars: Vec<_> = word.chars().to_vec();
            if chars.is_empty() { continue; }
            let (mut x0, mut y0, mut x1, mut y1) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
            for c in &chars { x0 = x0.min(c.rect.left()); y0 = y0.min(c.rect.top()); x1 = x1.max(c.rect.right()); y1 = y1.max(c.rect.bottom()); }
            let box_px = [x0.max(0) as u32, y0.max(0) as u32, (x1 - x0).max(0) as u32, (y1 - y0).max(0) as u32];
            words.push(OcrWord { text, box_px, center_px: [x0 as f64 + (x1 - x0) as f64 / 2.0, y0 as f64 + (y1 - y0) as f64 / 2.0] });
        }
    }
    Ok(words)
}
