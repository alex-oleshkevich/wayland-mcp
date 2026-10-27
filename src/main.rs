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
#[cfg(target_os = "linux")]
#[derive(Parser, Debug)]
#[command(name = "wayland-mcp", version)]
struct Cli {
    /// Comma-separated exact tool names to include (unknown names fail startup).
    #[arg(long, value_delimiter = ',')]
    tools: Option<Vec<String>>,
    /// Comma-separated exact tool names to exclude (unknown names fail startup).
    #[arg(long, value_delimiter = ',', default_value = "")]
    exclude_tools: Vec<String>,
}

#[cfg(target_os = "linux")]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let exclude: Vec<String> = cli.exclude_tools.into_iter().filter(|s| !s.is_empty()).collect();
    let include: Option<Vec<String>> = cli.tools.map(|v| v.into_iter().filter(|s| !s.is_empty()).collect());
    let session = Arc::new(Mutex::new(Session::new()));
    let server = server::Server::new(session, include, exclude).map_err(|e| anyhow::anyhow!(e))?;
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
