<p align="center">
  <img src="assets/icon.svg" width="96" height="96" alt="wayland-mcp icon">
</p>

# wayland-mcp

[![QA](https://github.com/alex-oleshkevich/wayland-mcp/actions/workflows/qa.yml/badge.svg)](https://github.com/alex-oleshkevich/wayland-mcp/actions/workflows/qa.yml)
[![Release](https://github.com/alex-oleshkevich/wayland-mcp/actions/workflows/release.yml/badge.svg)](https://github.com/alex-oleshkevich/wayland-mcp/actions/workflows/release.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

wayland-mcp lets an AI agent see and operate a Linux Wayland desktop. It is an
MCP server that speaks over stdio: your agent asks for a screenshot, reads the
text on screen, clicks, types, and checks what changed. It can also start a
private nested desktop so the agent works in a sandbox instead of on your own
screen.

It works with what the compositor offers. On niri and other wlroots-style
compositors it uses the native capture and input protocols. On GNOME and KDE,
which don't expose those, it goes through the screen-sharing portal after you
approve a consent dialog. When something isn't supported, the tool returns an
error that says so. It never fakes a result or quietly switches from the
sandbox to your real desktop.

## What it can do

- Take screenshots, read text with on-device OCR, find text, sample pixels and
  compare images.
- Move the pointer, click, drag, scroll, press keys and type Unicode text.
- Send touch taps, swipes and pinches.
- Read the accessibility tree of apps, find elements, press buttons and set
  field values.
- Read and write the clipboard. Paste-style typing puts your previous clipboard
  back afterwards.
- Wait for the screen or a piece of text to change, and save baselines to
  compare against later.
- Run a sandbox: a nested niri session with its own Wayland socket, D-Bus
  session, accessibility bus and clipboard. Launch apps inside it and read
  their logs.
- Batch several actions, record traces and replay them, and record the screen
  to video.
- Check what the current desktop supports with `doctor`, which only reads and
  never changes anything.

There are 50 tools in all. [REFERENCE.md](REFERENCE.md) lists them by group.

The sandbox only keeps the agent off your desktop. It doesn't restrict files or
network access.

## Installation

wayland-mcp runs on Linux with a Wayland session. The macOS builds on the
releases page exist only so the project builds everywhere; they print an
"unsupported platform" error and exit.

The quickest way to install is the install script. It downloads the latest
release for your CPU (amd64 or arm64), checks its SHA-256 checksum and puts
`wayland-mcp` in `~/.local/bin`:

```bash
curl -fsSL https://raw.githubusercontent.com/alex-oleshkevich/wayland-mcp/main/install.sh | sh
```

To pick a release or a different directory, pass options after `sh -s --`:

```bash
curl -fsSL https://raw.githubusercontent.com/alex-oleshkevich/wayland-mcp/main/install.sh | sh -s -- --version v0.2.0 --dir /usr/local/bin
```

You can also download an archive yourself from the
[releases page](https://github.com/alex-oleshkevich/wayland-mcp/releases).
The binary needs PipeWire and xkbcommon installed, which every Wayland desktop
has.

To build from source you need a Rust toolchain plus the PipeWire and
xkbcommon development libraries (the PipeWire bindings also need clang to
build). Then:

```bash
cargo build --release
```

The binary ends up at `target/release/wayland-mcp`.

Some features call other programs at runtime:

| Feature | Needs |
|---|---|
| Sandbox | `niri`, `dbus-daemon`, `at-spi2-registryd` |
| Screen recording | `ffmpeg` |
| OCR | model files, downloaded with `curl` on the first OCR call that passes `download_models:true` |

## Integration

Point your MCP client at the binary. It must run inside your Wayland session
so it can see `WAYLAND_DISPLAY`.

For Claude Code:

```bash
claude mcp add wayland-mcp -- /path/to/wayland-mcp/target/release/wayland-mcp
```

For clients that use an `mcpServers` JSON file:

```json
{
  "mcpServers": {
    "wayland-mcp": {
      "command": "/path/to/wayland-mcp/target/release/wayland-mcp",
      "args": ["--portal"]
    }
  }
}
```

Leave out `--portal` unless you're on GNOME or KDE and want the agent to
control your real desktop.

Options:

| Flag | Effect |
|---|---|
| `--portal` | Allows the screen-sharing portal (needed for GNOME and KDE). Off by default, and never used for sandboxes. |
| `--portal-timeout-ms <MS>` | How long to wait for you to answer the consent dialog, 1000 to 600000. Default 150000. If time runs out, the request is cancelled and the dialog closes. |
| `--touch-uinput <MAPPING>` | Maps a touchscreen device for touch input, as `/dev/input/eventN:WIDTHxHEIGHT@OUTPUT`. |
| `--ocr-dir <DIR>` | Where the OCR models live. Defaults to `$XDG_CACHE_HOME/ocrs`. |
| `--no-indicator` | Turns off the on-screen overlay that shows what the agent is doing. |
| `--tools <A,B>` | Exposes only these tools. |
| `--exclude-tools <A,B>` | Hides these tools. |

A misspelled tool name or an invalid value stops the server at startup.

```bash
wayland-mcp --tools doctor,screenshot,desktop_info
wayland-mcp --exclude-tools record_start,dbus_call
```

Each option except the tool filters can also come from an environment
variable: `WAYLAND_MCP_PORTAL`, `WAYLAND_MCP_PORTAL_TIMEOUT_MS`,
`WAYLAND_MCP_TOUCH_UINPUT`, `WAYLAND_MCP_OCR_DIR` and
`WAYLAND_MCP_NO_INDICATOR`. The two on/off ones take `1`/`0` (or
`true`/`false`, `yes`/`no`). A flag on the command line wins over its
variable. `wayland-mcp --help` lists everything.

Two subcommands help with setup. Both take the same options as the server.

```bash
wayland-mcp doctor          # what works on this desktop; --json for the raw report
wayland-mcp tools           # tools a client would see; --names for names only
wayland-mcp --exclude-tools record_start tools --names
```

`doctor` only reads: it never shows a consent dialog, downloads anything or
changes settings. It exits 1 when it can't reach the Wayland session at all.

## Usage

A typical session starts with `doctor` to see what works on this desktop. Then
the agent takes a `screenshot`, which returns an image and a `shot_id`. Clicks
and drags can use pixel positions from that image if they pass the same
`shot_id`. If the layout has changed since the screenshot, the server refuses
the action instead of guessing where to click.

To keep the agent off your screen, have it call `sandbox_start`, then
`target_select` with the returned id, and launch the app under test with
`app_launch`. Everything after that happens inside the nested session.
`sandbox_stop`, or shutting down the server, cleans it up.

On GNOME or KDE, the first screenshot or input action opens the system's
consent dialog. One approval covers capture, input and touch for that session.
If you deny it, the action fails and the server doesn't try another way in.

A few limits to know:

- On GNOME and KDE the portal keyboard only types characters on the US layout.
  For anything else, write the text with `clipboard_write` and press ctrl+v
  with `key`.
- On GNOME the clipboard is text-only, and paste-style typing is refused,
  because the server can't check what it would overwrite.
- Physical touchscreens haven't been tested on real hardware yet. The touch
  code has only been tested in a VM.

Data the server saves (baselines, traces, recordings) stays under
`$XDG_DATA_HOME/wayland-mcp`, which is usually `~/.local/share/wayland-mcp`.
Traces replace typed text with a character count.
Screenshots go to whichever model your client uses, like any other MCP image.

## More

- [REFERENCE.md](REFERENCE.md): every tool group, the coordinate rules,
  privacy details and per-desktop notes.
- [TESTING.md](TESTING.md): the test suites, the VM setup and what has been
  checked on each desktop.
