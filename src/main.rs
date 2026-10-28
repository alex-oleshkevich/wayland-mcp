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
    /// How many recent actions the on-screen overlay lists (newest on top).
    #[arg(long, global = true, env = "WAYLAND_MCP_INDICATOR_HISTORY", default_value_t = 5, value_name = "N",
          value_parser = clap::value_parser!(u8).range(1..=20))]
    indicator_history: u8,
    /// How long each action stays on the on-screen overlay.
    #[arg(long, global = true, env = "WAYLAND_MCP_INDICATOR_TTL_MS", default_value_t = 3000, value_name = "MS",
          value_parser = clap::value_parser!(u64).range(500..=60_000))]
    indicator_ttl_ms: u64,
}

#[cfg(target_os = "linux")]
#[derive(clap::Subcommand, Debug)]
enum Command {
    /// Check what works on this desktop (read-only; never prompts, downloads
    /// or changes settings). Exits 1 when the Wayland session is unreachable.
    Doctor,
    /// Print a guide for AI agents driving wayland-mcp from the shell.
    Prime,
    /// List tools, or show one tool's description and input JSON schema.
    Tools {
        /// Tool name; omitted to list tools after --tools/--exclude-tools.
        #[arg(conflicts_with = "names")]
        name: Option<String>,
        /// Print only the names, one per line.
        #[arg(long, conflicts_with = "json")]
        names: bool,
        /// Print the named tool's definition as JSON.
        #[arg(long, requires = "name")]
        json: bool,
    },
    /// Run one tool through the real MCP dispatch and print the result.
    ///
    /// Starts a fresh in-process server for this call and shuts it down
    /// afterwards. Sandboxes, selected targets, shot ids, held input, clipboard
    /// ownership, launched apps/logs, portal sessions, active traces and recording
    /// handles do not carry over between calls. Saved artifacts remain on disk.
    /// The result goes to stdout; errors go to stderr. Exits 1
    /// when the call fails (unknown tool, invalid arguments, tool error, or for
    /// `doctor` an unreachable Wayland session) and 2 when JSON is not a JSON
    /// object.
    Call {
        /// Tool name, as listed by `tools`.
        tool: String,
        /// Tool arguments as a JSON object.
        #[arg(default_value = "{}")]
        json: String,
        /// Write the first image of the result to FILE.
        #[arg(short, long, value_name = "FILE")]
        output: Option<std::path::PathBuf>,
    },
}

/// The same filtered index is used by `tools` and appended to `prime`.
#[cfg(target_os = "linux")]
fn tool_index(server: &server::Server, names: bool) -> String {
    use std::fmt::Write as _;
    let tools = server.tool_list();
    let width = tools.iter().map(|(name, _)| name.len()).max().unwrap_or(0);
    let mut out = String::new();
    for (name, desc) in tools {
        if names { let _ = writeln!(out, "{name}"); } else {
            let short = desc.split(". ").next().unwrap_or("").trim_end_matches('.');
            let _ = writeln!(out, "{name:<width$}  {short}");
        }
    }
    out
}

/// Read schemas from the dispatch router, never from a parallel CLI registry.
#[cfg(target_os = "linux")]
fn tool_definition(server: &server::Server, name: &str) -> Result<serde_json::Value, rmcp::ErrorData> {
    let tools = server.tool_router.list_all();
    if let Some(tool) = tools.iter().find(|tool| tool.name == name) {
        return Ok(serde_json::json!({
            "name": tool.name,
            "description": tool.description,
            "input_schema": tool.input_schema,
        }));
    }
    // Tool identifiers are ASCII. Reuse one edit-distance row for all candidates.
    let mut row = vec![0usize; name.len() + 1];
    let mut close = Vec::new();
    for tool in &tools {
        for (i, cell) in row.iter_mut().enumerate() { *cell = i; }
        for (i, a) in tool.name.bytes().enumerate() {
            let mut diagonal = row[0];
            row[0] = i + 1;
            for (j, b) in name.bytes().enumerate() {
                let above = row[j + 1];
                row[j + 1] = (above + 1).min(row[j] + 1).min(diagonal + usize::from(a != b));
                diagonal = above;
            }
        }
        let distance = row[name.len()];
        if distance <= 3 { close.push((distance, tool.name.as_ref())); }
    }
    close.sort_unstable();
    let suggestions: Vec<_> = close.into_iter().take(3).map(|(_, name)| name).collect();
    let hint = if suggestions.is_empty() {
        "run `wayland-mcp tools` to list available tools".to_string()
    } else { format!("did you mean: {}?", suggestions.join(", ")) };
    Err(rmcp::ErrorData::invalid_params(
        format!("unknown tool '{name}'; {hint}"),
        Some(serde_json::json!({"suggestions": suggestions})),
    ))
}

/// True when the server could not work at all: no Wayland socket or registry.
#[cfg(target_os = "linux")]
fn doctor_fatal(report: &serde_json::Value) -> bool {
    report["checks"].as_array().is_some_and(|cs| cs.iter().any(|c| matches!(
        (c["name"].as_str(), c["status"].as_str()),
        (Some("wayland_socket"), Some("missing")) | (Some("registry"), Some("error"))
    )))
}

/// Run `tool` against a fresh in-process server over a duplex pipe, using a real
/// rmcp client so filters, validation and errors match an external client.
/// Output is buffered and written after cleanup, so a closed stdout pipe can't
/// skip the cleanup. Returns the process exit code.
#[cfg(target_os = "linux")]
async fn run_call(
    server: server::Server,
    tool: String,
    args: serde_json::Map<String, serde_json::Value>,
    output: Option<std::path::PathBuf>,
) -> anyhow::Result<i32> {
    use base64::Engine;
    use rmcp::model::{CallToolRequestParams, ContentBlock};
    use std::fmt::Write as _;

    let cleanup = server.clone();
    // Ctrl-C / SIGTERM mid-call (e.g. during app_launch wait_ms) must still
    // stop anything the call started. Cleanup may wait for an input action
    // that holds its device (a long type_text or drag), so it runs off this
    // task and a second signal exits at once, as in the server path.
    {
        use tokio::signal::unix::{signal, SignalKind};
        let cleanup = cleanup.clone();
        let mut term = signal(SignalKind::terminate())?;
        let mut int = signal(SignalKind::interrupt())?;
        tokio::spawn(async move {
            let code = tokio::select! { _ = term.recv() => 143, _ = int.recv() => 130 };
            apps::begin_sandbox_shutdown();
            let done = tokio::task::spawn_blocking(move || cleanup.shutdown_cleanup());
            tokio::select! {
                _ = done => {}
                _ = term.recv() => { eprintln!("wayland-mcp: second termination signal, exiting immediately"); apps::kill_live_sandboxes_now(); }
                _ = int.recv() => { eprintln!("wayland-mcp: second termination signal, exiting immediately"); apps::kill_live_sandboxes_now(); }
            }
            std::process::exit(code);
        });
    }
    let (client_io, server_io) = tokio::io::duplex(MAX_REQUEST_LINE_BYTES);
    let (sr, sw) = tokio::io::split(server_io);
    let (cr, cw) = tokio::io::split(client_io);
    let server_transport = AsyncRwTransport::new_server(sr, sw).with_max_line_length(MAX_REQUEST_LINE_BYTES);
    let client_transport = AsyncRwTransport::new_client(cr, cw).with_max_line_length(MAX_REQUEST_LINE_BYTES);
    let server_task = tokio::spawn(async move {
        match server.serve(server_transport).await {
            Ok(s) => { let _ = s.waiting().await; }
            Err(e) => eprintln!("wayland-mcp: server failed to start: {e}"),
        }
    });
    let is_doctor = tool == "doctor";
    let mut params = CallToolRequestParams::new(tool);
    params.arguments = Some(args);
    let (mut out, mut err) = (String::new(), String::new());
    let code = match ().serve(client_transport).await {
        Err(e) => { let _ = writeln!(err, "error: {e}"); 1 }
        Ok(client) => {
            let res = client.call_tool(params).await;
            let _ = client.cancel().await;
            match res {
                Err(e) => { let _ = writeln!(err, "error: {e}"); 1 }
                Ok(result) => {
                    let failed = result.is_error == Some(true);
                    let mut code = i32::from(failed);
                    let mut images = 0usize;
                    for block in &result.content {
                        match block {
                            ContentBlock::Text(t) => {
                                let text = match serde_json::from_str::<serde_json::Value>(&t.text) {
                                    Ok(v) if v.is_object() || v.is_array() => {
                                        if is_doctor && !failed && doctor_fatal(&v) { code = 1; }
                                        serde_json::to_string_pretty(&v)?
                                    }
                                    _ => t.text.clone(),
                                };
                                // A tool error's text is the error message, not a result.
                                let _ = if failed { writeln!(err, "error: {text}") } else { writeln!(out, "{text}") };
                            }
                            ContentBlock::Image(img) => {
                                images += 1;
                                let bytes = base64::engine::general_purpose::STANDARD.decode(&img.data);
                                match (&output, bytes) {
                                    (Some(path), Ok(b)) if images == 1 => match std::fs::write(path, &b) {
                                        Ok(()) => { let _ = writeln!(err, "wrote {} ({}, {} bytes)", path.display(), img.mime_type, b.len()); }
                                        Err(e) => { let _ = writeln!(err, "error: cannot write {}: {e}", path.display()); code = 1; }
                                    },
                                    (Some(_), Ok(_)) => {}
                                    (Some(_), Err(e)) => { let _ = writeln!(err, "error: invalid image data: {e}"); code = 1; }
                                    (None, Ok(b)) => { let _ = writeln!(err, "image ({}, {} bytes); use -o FILE to save it", img.mime_type, b.len()); }
                                    (None, Err(e)) => { let _ = writeln!(err, "error: invalid image data: {e}"); code = 1; }
                                }
                            }
                            other => { let _ = writeln!(out, "{}", serde_json::to_string_pretty(other)?); }
                        }
                    }
                    if output.is_some() && images > 1 {
                        let _ = writeln!(err, "skipped {} more image(s); only the first is written", images - 1);
                    }
                    if output.is_some() && images == 0 && !failed {
                        let _ = writeln!(err, "error: result has no image to write");
                        code = 1;
                    }
                    code
                }
            }
        }
    };
    let _ = server_task.await;
    apps::begin_sandbox_shutdown();
    cleanup.shutdown_cleanup();
    // Write errors (e.g. stdout piped into `head`) must not panic: cleanup is done.
    use std::io::Write as _;
    let _ = std::io::stdout().lock().write_all(out.as_bytes());
    let _ = std::io::stderr().lock().write_all(err.as_bytes());
    Ok(code)
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
    // Reject bad call arguments before anything starts.
    let call_args = match &cli.command {
        Some(Command::Call { json, .. }) => match serde_json::from_str::<serde_json::Value>(json) {
            Ok(serde_json::Value::Object(m)) => Some(m),
            Ok(_) => { eprintln!("error: call arguments must be a JSON object"); std::process::exit(2); }
            Err(e) => { eprintln!("error: call arguments are not valid JSON: {e}"); std::process::exit(2); }
        },
        _ => None,
    };
    config::init(config::Config {
        portal: cli.portal,
        portal_timeout_ms: cli.portal_timeout_ms,
        touch_uinput: cli.touch_uinput,
        ocr_dir: cli.ocr_dir,
        indicator: !cli.no_indicator,
        indicator_history: cli.indicator_history.into(),
        indicator_ttl_ms: cli.indicator_ttl_ms,
    });
    let exclude: Vec<String> = cli.exclude_tools.into_iter().filter(|s| !s.is_empty()).collect();
    let include: Option<Vec<String>> = cli.tools.map(|v| v.into_iter().filter(|s| !s.is_empty()).collect());
    let session = Arc::new(Mutex::new(Session::new()));
    let server = server::Server::new(session, include, exclude).map_err(|e| anyhow::anyhow!(e))?;
    match cli.command {
        Some(Command::Prime) => {
            print!("{}\n\n## Tool index\n\n```text\n{}```\n", include_str!("prime.md").trim_end(), tool_index(&server, false));
            return Ok(());
        }
        Some(Command::Tools { name, names, json }) => {
            if let Some(name) = name {
                match tool_definition(&server, &name) {
                    Ok(definition) if json => println!("{}", serde_json::to_string_pretty(&definition)?),
                    Ok(definition) => println!("{}\n{}\n\nARGS_JSON schema:\n{}", name,
                        definition["description"].as_str().unwrap_or(""),
                        serde_json::to_string_pretty(&definition["input_schema"])?),
                    Err(error) => {
                        eprintln!("{}", serde_json::to_string(&error)?);
                        std::process::exit(1);
                    }
                }
            } else { print!("{}", tool_index(&server, names)); }
            return Ok(());
        }
        Some(Command::Doctor) => {
            let report = server.doctor_report().await.map_err(|e| anyhow::anyhow!(e))?;
            print_doctor(&report);
            std::process::exit(i32::from(doctor_fatal(&report)));
        }
        Some(Command::Call { tool, output, .. }) => {
            let code = run_call(server, tool, call_args.unwrap_or_default(), output).await?;
            std::process::exit(code);
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
