//! Runtime options from the command line (each flag can also be set by its
//! environment variable, see `--help`). Set once at startup; code that runs
//! without `main` (unit tests) sees the defaults.
use std::path::PathBuf;
use std::sync::OnceLock;

#[derive(Debug, Clone)]
pub struct Config {
    /// Opt into the consented RemoteDesktop/ScreenCast portal (GNOME, KDE).
    pub portal: bool,
    /// How long portal establishment waits for the consent dialog.
    pub portal_timeout_ms: u64,
    /// Verified uinput touchscreen mapping, `/dev/input/eventN:WIDTHxHEIGHT@OUTPUT`.
    pub touch_uinput: Option<String>,
    /// OCR model directory override.
    pub ocr_dir: Option<PathBuf>,
    /// Draw the activity indicator overlay.
    pub indicator: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self { portal: false, portal_timeout_ms: 150_000, touch_uinput: None, ocr_dir: None, indicator: true }
    }
}

static CONFIG: OnceLock<Config> = OnceLock::new();

pub fn init(config: Config) {
    CONFIG.set(config).expect("config initialised twice");
}

pub fn get() -> &'static Config {
    CONFIG.get_or_init(Config::default)
}
