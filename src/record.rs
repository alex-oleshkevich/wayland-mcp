//! Screen recording: screencopy frames piped to ffmpeg (libx264/mp4). Runs
//! independently of any single tool response; stop reports actual frames,
//! duration and encoder errors. Artifacts live under the private data dir.
use crate::wayland::{capture_output, TargetSpec};
use std::{io::Write, process::{Child, Command, Stdio}, sync::{atomic::{AtomicBool, AtomicU64, Ordering}, Arc, Mutex}, time::Instant};

pub struct Recording {
    pub id: String,
    pub path: std::path::PathBuf,
    pub target_id: String,
    pub started: Instant,
    pub frames: Arc<AtomicU64>,
    pub stop_flag: Arc<AtomicBool>,
    pub error: Arc<Mutex<Option<String>>>,
    pub handle: Option<std::thread::JoinHandle<()>>,
    pub fps: u32,
}

pub fn start(id: String, target: TargetSpec, path: std::path::PathBuf, fps: u32, max_duration_ms: u64, output: Option<String>) -> Result<Recording, String> {
    // Probe geometry with a first capture (also proves capture works before claiming to record).
    let first = capture_output(&target, output.as_deref())?;
    let (w, h) = (first.width, first.height);
    let mut child: Child = Command::new("ffmpeg")
        .args(["-y", "-nostats", "-loglevel", "error", "-f", "rawvideo", "-pix_fmt", "rgba", "-s", &format!("{w}x{h}"), "-r", &fps.to_string(), "-i", "-", "-an", "-vf", "pad=ceil(iw/2)*2:ceil(ih/2)*2", "-c:v", "libx264", "-pix_fmt", "yuv420p", "-preset", "ultrafast"])
        .arg(&path).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped())
        .spawn().map_err(|e| format!("start ffmpeg (is ffmpeg installed?): {e}"))?;
    // Drain stderr continuously into a bounded buffer: an undrained pipe fills
    // after a few minutes, ffmpeg then blocks and the recording thread hangs.
    let stderr_tail = Arc::new(Mutex::new(Vec::<u8>::new()));
    let stderr_thread = child.stderr.take().map(|mut err| {
        let tail = stderr_tail.clone();
        std::thread::spawn(move || {
            use std::io::Read;
            let mut chunk = [0u8; 8192];
            loop {
                match err.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut t = tail.lock().unwrap();
                        t.extend_from_slice(&chunk[..n]);
                        const CAP: usize = 8192;
                        if t.len() > CAP { let drop = t.len() - CAP; t.drain(..drop); }
                    }
                }
            }
        })
    });
    let mut stdin = child.stdin.take().ok_or("ffmpeg stdin unavailable")?;
    let frames = Arc::new(AtomicU64::new(0));
    let stop_flag = Arc::new(AtomicBool::new(false));
    let error = Arc::new(Mutex::new(None));
    let target_id = target.id.clone();
    let handle = {
        let frames = frames.clone(); let stop_flag = stop_flag.clone(); let error = error.clone();
        std::thread::spawn(move || {
            let started = Instant::now();
            let frame_interval = std::time::Duration::from_millis((1000 / fps.max(1)) as u64);
            let mut next = Instant::now();
            // Write the probed first frame.
            if stdin.write_all(&first.rgba).is_ok() { frames.fetch_add(1, Ordering::Relaxed); }
            loop {
                if stop_flag.load(Ordering::Relaxed) { break; }
                if started.elapsed().as_millis() as u64 >= max_duration_ms { break; }
                if Instant::now() < next { std::thread::sleep(std::time::Duration::from_millis(5)); continue; }
                next = Instant::now() + frame_interval;
                match capture_output(&target, output.as_deref()) {
                    Ok(cap) => {
                        if cap.width != w || cap.height != h { *error.lock().unwrap() = Some(format!("output size changed mid-recording ({}x{} -> {}x{}); recording stopped", w, h, cap.width, cap.height)); break; }
                        if stdin.write_all(&cap.rgba).is_err() { *error.lock().unwrap() = Some("ffmpeg stdin closed unexpectedly".into()); break; }
                        frames.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => { *error.lock().unwrap() = Some(format!("capture failed mid-recording: {e}")); break; }
                }
            }
            drop(stdin);
            let status = child.wait();
            if let Some(t) = stderr_thread { let _ = t.join(); }
            match status {
                Ok(s) if !s.success() => {
                    let tail = stderr_tail.lock().unwrap();
                    let stderr = String::from_utf8_lossy(tail.as_slice()).chars().take(1000).collect::<String>();
                    let mut slot = error.lock().unwrap();
                    *slot = Some(format!("ffmpeg exited {s}: {stderr}"));
                }
                Err(e) => { *error.lock().unwrap() = Some(format!("wait ffmpeg: {e}")); }
                _ => {}
            }
        })
    };
    Ok(Recording { id, path, target_id, started: Instant::now(), frames, stop_flag, error, handle: Some(handle), fps })
}
