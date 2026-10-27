#[cfg(target_os = "linux")]
mod coords;
#[cfg(target_os = "linux")]
mod a11y;
#[cfg(target_os = "linux")]
mod apps;
#[cfg(target_os = "linux")]
mod clipboard;
#[cfg(target_os = "linux")]
mod input;
#[cfg(target_os = "linux")]
mod indicator;
#[cfg(target_os = "linux")]
mod dbus;
#[cfg(target_os = "linux")]
mod server;
#[cfg(target_os = "linux")]
mod media;
#[cfg(target_os = "linux")]
mod record;
#[cfg(target_os = "linux")]
mod ocr;
#[cfg(target_os = "linux")]
mod portal;
#[cfg(target_os = "linux")]
mod portal_session;
#[cfg(target_os = "linux")]
mod state;
#[cfg(target_os = "linux")]
mod wayland;
#[cfg(target_os = "linux")]
mod touch;
#[cfg(target_os = "linux")]
mod config;

#[cfg(target_os = "linux")]
use clap::Parser;
#[cfg(target_os = "linux")]
use rmcp::{transport::{async_rw::AsyncRwTransport, stdio}, ServiceExt};

/// rmcp's default 16 MiB line cap silently drops oversize requests (no response at all).
/// Raised so large clipboard_write/type_text payloads reach the tools and are handled there.
#[cfg(target_os = "linux")]
const MAX_REQUEST_LINE_BYTES: usize = 64 * 1024 * 1024;

#[cfg(target_os = "linux")]
use state::Session;
#[cfg(target_os = "linux")]
use std::sync::{Arc, Mutex};

/// Wayland computer-use MCP server (stdio). Logs go to stderr; stdout is MCP only.
/// Without a subcommand it runs the MCP server.
#[cfg(target_os = "linux")]
#[derive(Parser, Debug)]
#[command(name = "wayland-mcp", version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    /// Comma-separated exact tool names to include (unknown names fail startup).
    #[arg(long, global = true, value_delimiter = ',')]
    tools: Option<Vec<String>>,
    /// Comma-separated exact tool names to exclude (unknown names fail startup).
    #[arg(long, global = true, value_delimiter = ',', default_value = "")]
    exclude_tools: Vec<String>,
    /// Allow the screen-sharing portal (needed on GNOME and KDE). A consent
    /// dialog asks before anything is captured or injected. Never used for
    /// sandbox targets.
    #[arg(long, global = true, env = "WAYLAND_MCP_PORTAL", value_parser = clap::builder::BoolishValueParser::new())]
    portal: bool,
    /// How long to wait for the portal consent dialog before cancelling the
    /// request and closing the dialog.
    #[arg(long, global = true, env = "WAYLAND_MCP_PORTAL_TIMEOUT_MS", default_value_t = 150_000, value_name = "MS",
          value_parser = clap::value_parser!(u64).range(1_000..=600_000))]
    portal_timeout_ms: u64,
    /// Touchscreen device for touch input when the portal grants none:
    /// /dev/input/eventN:WIDTHxHEIGHT@OUTPUT.
    #[arg(long, global = true, env = "WAYLAND_MCP_TOUCH_UINPUT", value_name = "MAPPING", value_parser = parse_touch_mapping)]
    touch_uinput: Option<String>,
    /// Directory with the OCR models (default: $XDG_CACHE_HOME/ocrs).
    #[arg(long, global = true, env = "WAYLAND_MCP_OCR_DIR", value_name = "DIR")]
    ocr_dir: Option<std::path::PathBuf>,
    /// Do not draw the on-screen overlay that shows what the agent is doing.
    #[arg(long, global = true, env = "WAYLAND_MCP_NO_INDICATOR", value_parser = clap::builder::BoolishValueParser::new())]
    no_indicator: bool,
}

#[cfg(target_os = "linux")]
#[derive(clap::Subcommand, Debug)]
enum Command {
    /// Check what works on this desktop (read-only; never prompts, downloads
    /// or changes settings). Exits 1 when the Wayland session is unreachable.
    Doctor {
        /// Print the raw JSON report instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// List the tools a client would see, after --tools/--exclude-tools.
    Tools {
        /// Print only the names, one per line.
        #[arg(long)]
        names: bool,
    },
}

/// True when the server could not work at all: no Wayland socket or registry.
#[cfg(target_os = "linux")]
fn doctor_fatal(report: &serde_json::Value) -> bool {
    report["checks"].as_array().is_some_and(|cs| cs.iter().any(|c| matches!(
        (c["name"].as_str(), c["status"].as_str()),
        (Some("wayland_socket"), Some("missing")) | (Some("registry"), Some("error"))
    )))
}

/// Print the doctor report as an aligned table.
#[cfg(target_os = "linux")]
fn print_doctor(report: &serde_json::Value) {
    let checks = report["checks"].as_array().cloned().unwrap_or_default();
    println!("target: {}", report["target"].as_str().unwrap_or("?"));
    let width = checks.iter().filter_map(|c| c["name"].as_str()).map(str::len).max().unwrap_or(0);
    for c in &checks {
        let (name, status) = (c["name"].as_str().unwrap_or(""), c["status"].as_str().unwrap_or(""));
        let detail = match &c["detail"] {
            serde_json::Value::Object(m) => m.iter()
                .filter(|(k, v)| k.as_str() != "note" && !v.is_null())
                .map(|(k, v)| format!("{k}={}", v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())))
                .collect::<Vec<_>>().join(" "),
            v => v.to_string(),
        };
        println!("  {name:<width$}  {status:<18} {detail}");
    }
}

#[cfg(target_os = "linux")]
fn parse_touch_mapping(s: &str) -> Result<String, String> {
    touch::parse_mapping(s).map(|_| s.to_string())
}

#[cfg(target_os = "linux")]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    config::init(config::Config {
        portal: cli.portal,
        portal_timeout_ms: cli.portal_timeout_ms,
        touch_uinput: cli.touch_uinput,
        ocr_dir: cli.ocr_dir,
        indicator: !cli.no_indicator,
    });
    let exclude: Vec<String> = cli.exclude_tools.into_iter().filter(|s| !s.is_empty()).collect();
    let include: Option<Vec<String>> = cli.tools.map(|v| v.into_iter().filter(|s| !s.is_empty()).collect());
    let session = Arc::new(Mutex::new(Session::new()));
    let server = server::Server::new(session, include, exclude).map_err(|e| anyhow::anyhow!(e))?;
    match cli.command {
        Some(Command::Tools { names }) => {
            let tools = server.tool_list();
            let width = tools.iter().map(|(n, _)| n.len()).max().unwrap_or(0);
            for (name, desc) in tools {
                if names { println!("{name}"); } else {
                    // First sentence keeps the listing to one line per tool.
                    let short = desc.split(". ").next().unwrap_or("").trim_end_matches('.');
                    println!("{name:<width$}  {short}");
                }
            }
            return Ok(());
        }
        Some(Command::Doctor { json }) => {
            let report = server.doctor_report().await.map_err(|e| anyhow::anyhow!(e))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print_doctor(&report);
            }
            std::process::exit(i32::from(doctor_fatal(&report)));
        }
        None => {}
    }
    eprintln!("wayland-mcp {} starting (stdio)", env!("CARGO_PKG_VERSION"));
    let cleanup = server.clone();
    let (stdin, stdout) = stdio();
    let transport = AsyncRwTransport::new_server(stdin, stdout).with_max_line_length(MAX_REQUEST_LINE_BYTES);
    let service = server.serve(transport).await?;
    // SIGTERM/SIGINT → cancel the service so waiting() returns and the cleanup
    // below runs (otherwise the process dies instantly and leaks sandboxes).
    let signalled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        use tokio::signal::unix::{signal, SignalKind};
        let token = service.cancellation_token();
        let flag = signalled.clone();
        let mut term = signal(SignalKind::terminate())?;
        let mut int = signal(SignalKind::interrupt())?;
        tokio::spawn(async move {
            tokio::select! { _ = term.recv() => {}, _ = int.recv() => {} }
            eprintln!("wayland-mcp: termination signal received, shutting down");
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
            token.cancel();
            // A second signal during (possibly blocked) cleanup must not be
            // swallowed: tokio's handlers replace the default action for the
            // rest of the process, so exit explicitly.
            tokio::select! { _ = term.recv() => {}, _ = int.recv() => {} }
            eprintln!("wayland-mcp: second termination signal, exiting immediately");
            apps::kill_live_sandboxes_now();
            std::process::exit(1);
        });
    }
    let _ = service.waiting().await;
    apps::begin_sandbox_shutdown();
    cleanup.shutdown_cleanup();
    if signalled.load(std::sync::atomic::Ordering::SeqCst) {
        // The blocking stdin reader would otherwise keep the runtime from exiting.
        std::process::exit(0);
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("wayland-mcp requires Linux with a Wayland session; this platform is not supported");
    std::process::exit(1);
}

#[cfg(all(test, target_os = "linux"))]
mod cli_tests {
    use super::Cli;
    use clap::Parser;

    /// On/off options read `1`/`0` from their variables, and a flag beats its variable.
    #[test]
    fn bool_options_from_env_and_flags() {
        std::env::remove_var("WAYLAND_MCP_NO_INDICATOR");
        std::env::set_var("WAYLAND_MCP_PORTAL", "0");
        assert!(!Cli::try_parse_from(["wayland-mcp"]).unwrap().portal);
        assert!(Cli::try_parse_from(["wayland-mcp", "--portal"]).unwrap().portal);
        std::env::set_var("WAYLAND_MCP_PORTAL", "1");
        assert!(Cli::try_parse_from(["wayland-mcp"]).unwrap().portal);
        std::env::set_var("WAYLAND_MCP_PORTAL", "bogus");
        assert!(Cli::try_parse_from(["wayland-mcp"]).is_err());
        std::env::remove_var("WAYLAND_MCP_PORTAL");
        assert!(!Cli::try_parse_from(["wayland-mcp"]).unwrap().portal);
        assert!(Cli::try_parse_from(["wayland-mcp", "--no-indicator"]).unwrap().no_indicator);
    }
}
