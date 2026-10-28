# wayland-mcp: shell agent guide

See and operate a Linux Wayland desktop: screenshots, OCR, pointer/keyboard/touch
input, AT-SPI accessibility, clipboard, visual checks and application diagnostics.
This is desktop automation, not in-process GTK widget inspection. Use gtk-mcp for
instrumented GTK4/libadwaita widget trees, CSS and layout internals. AT-SPI actions
are semantic activation and do not prove that physical pointer/keyboard input works.

## Start here

Run inside the user's Wayland session, with `WAYLAND_DISPLAY`, `XDG_RUNTIME_DIR`
and the session D-Bus environment available. Run `wayland-mcp doctor` first: it
only reads, never prompts, downloads models or changes settings. Unsupported
capabilities are explicit; a successful doctor exit does not mean every feature
works. Native capture/input depend on the compositor's advertised protocols.
GNOME/KDE host control normally requires explicit `--portal` and user consent.

The binary needs PipeWire and xkbcommon. Optional features need `niri`,
`dbus-daemon` and `at-spi2-registryd` (sandboxes), `ffmpeg` (video), and OCR model
files (`--ocr-dir` or `$XDG_CACHE_HOME/ocrs`, normally `~/.cache/ocrs`). Missing OCR
models download only on an OCR request with `download_models:true`; doctor does
not download them. CPU OCR is slow in debug builds; prefer a release binary.

## Commands and argument discovery

```sh
wayland-mcp prime
wayland-mcp doctor
wayland-mcp tools
wayland-mcp tools --names
wayland-mcp tools screenshot
wayland-mcp tools click --json
wayland-mcp call desktop_info
wayland-mcp call screenshot '{"max_dimension":1200}' -o before.png
wayland-mcp --exclude-tools record_start tools --names
```

`tools NAME` prints the complete description and input JSON schema from the
server's dispatch router; `--json` prints `name`, `description`, `input_schema`.
Unknown or filtered-out names produce a JSON error on stderr with close-match
suggestions when available. Discover arguments instead of guessing them.
`--names` is for the index only; `--json` requires a name.

`call TOOL [JSON]` uses real MCP dispatch against a fresh in-process server.
Arguments default to `{}` and must be a JSON object. Text/JSON results go to
stdout; errors and image notices go to stderr. `-o FILE` saves the first returned
image (PNG by default for screenshots; use a matching extension for JPEG).
Additional images are skipped with a notice; `-o` fails if no image is returned.
Without `-o`, image bytes are not printed: save and view the file.

All subcommands accept the server's global flags: `--tools A,B`,
`--exclude-tools A,B`, `--portal`, `--portal-timeout-ms MS`, `--touch-uinput MAPPING`,
`--ocr-dir DIR`, `--no-indicator`. Filters affect listing, schemas and dispatch,
including batch/replay; unknown filter names fail startup. `--help` describes
flags and environment variables. Without a subcommand, the binary runs a
persistent MCP server over stdin/stdout, with diagnostics on stderr.

| Exit | Meaning |
|---|---|
| 0 | Command/call completed without an MCP tool error; inspect result-level status too. |
| 1 | Unknown tool/schema lookup, tool/dispatch failure, image write/no-image failure, startup/runtime error; doctor also uses 1 for unreachable Wayland socket/registry. |
| 2 | CLI usage error or call arguments that are invalid JSON/not an object. |

Timeouts from `wait_screen`/`wait_for_text` are normal results with
`satisfied:false`. A batch can return validation errors or failed steps with
exit 0: inspect `dispatched`, `completed` and each step's `status`. An input
success means dispatch, not that the app accepted it. Never blindly replay an
action after an error: part of it may already have happened.

## Observe → locate → input → verify

1. Run `doctor`, then `call desktop_info` for output geometry and capabilities.
2. Capture with `call screenshot '{"max_dimension":1200}' -o before.png`, or
   `call get_app_state -o before.png` for a screenshot plus bounded AT-SPI tree.
3. View the image and locate the target. `a11y_find`/`a11y_tree` can supply live
   accessibility refs; `wait_for_text` can capture and OCR within one call.
   `read_text`/`find_text` require a retained `shot_id` and therefore cannot read
   a previous shell call's screenshot. OCR marks (`screenshot` with `marks:true`)
   can label text in the same call if models are already installed; they do not
   identify unlabeled icons.
4. For a separate shell input call, convert image pixels to desktop logical
   coordinates as below, and set `space:"logical"`. Request `observe` on the
   action to receive its verification screenshot before the server exits.
5. View the saved observation and/or take another screenshot; use semantic
   readback or a fresh `wait_for_text`/`wait_screen` as appropriate. Check explicit
   verification fields, not only the process exit code.

For example, **after locating a target at logical (120, 80)**:

```sh
wayland-mcp call click '{"x":120,"y":80,"space":"logical","observe":{"delays_ms":[250],"max_dimension":1200}}' -o after.png
```

Replace those coordinates with the observed target, not a guess. Observations
can fail after a successful action; the result reports both facts and does not
replay input. `baseline_save` followed by `baseline_compare` in later calls is
another cross-call visual check: named baselines persist on disk.

## Coordinates and scaling

Screenshot `transform` describes the returned image's dimensions and its desktop
logical rectangle. `crop` is in logical coordinates; `max_dimension` changes
returned image pixels, not desktop geometry. Output origins can be negative;
physical pixels and desktop logical coordinates are not interchangeable.

Within one persistent MCP connection, pass `space:"screenshot"`, that image's
`shot_id`, and its pixel coordinates. Only the last 32 shots are retained.
Cross-target or changed-layout shots are rejected before input. This is a layout
check, not detection that a button moved inside a window: observe again after UI
changes. Never carry a `shot_id` to a new `call` process.

For separate shell calls, convert a returned image pixel `(px, py)` using its
transform. Let `[cx, cy, cw, ch]` be `crop_exact` when non-null, otherwise
`crop_logical`'s x/y/width/height. Let `W/H` be `image_width/image_height`:

```text
logical_x = cx + (px + 0.5) / W * cw
logical_y = cy + (py + 0.5) / H * ch
```

Use those values with explicit `space:"logical"`. Do not divide by an assumed
monitor scale or omit the output/crop origin. Separate logical-coordinate calls
cannot enforce the previous shot's layout-generation guard; recheck geometry
and recapture if the desktop changes.

## Connection lifetime: what shell calls cannot retain

Every `call` starts on `host` and shuts down its own server. State is per server,
not shared by later invocations:

- Sandboxes and selected targets: shutdown stops nested compositors/private buses.
  `call sandbox_start` cannot prepare a sandbox for another call.
- Shot images/ids, layout tracking and counters: `read_text`, `find_text`, `diff`
  and shot-id-based input cannot consume a prior call's capture.
- Held keys/buttons/touch contacts: cleanup releases server-owned input. Use a
  complete `drag`/chord, or a batch for holds plus subsequent actions.
- Clipboard ownership: the selection-serving connection closes. Do not assume
  `call clipboard_write` will keep text available for a later paste. Use
  `type_text mode="paste"` when supported, or batch clipboard_write plus key.
- Launched app ownership, app ids and captured logs: cleanup stops owned apps;
  another call cannot list/log/stop/restart them. Existing external apps remain.
- Portal sessions/consent, input devices and activity indicator: cleanup closes
  them; a later portal call may need fresh user approval.
- Active trace and recording handles: trace_start cannot collect later shell
  calls; recordings are stopped during cleanup, not continued in the background.

Disk artifacts (baselines, trace files, video files) remain under
`$XDG_DATA_HOME/wayland-mcp` (normally `~/.local/share/wayland-mcp`). Baseline
comparison and trace_replay by path/id can read prior artifacts; replay still
requires its recorded target/layout and valid arguments, and redacted text is
not replayable. AT-SPI refs name external bus objects, not a server cache: they
may work in another host call while the app/object lives; stale refs fail.
Desktop effects such as typed text, clicks and settings are not rolled back.

`batch` shares one server for up to 100 prevalidated sequential steps. Allowed
steps: move, click, drag, scroll, mouse_button, key, key_hold, type_text,
release_all, wait, screenshot, touch_tap, touch_swipe, touch_pinch, a11y_act,
a11y_set_value, clipboard_write. No nested batch, sandbox/target/process/D-Bus,
trace or recording operations. It cannot substitute results into later steps;
a screenshot step cannot supply a dynamically discovered shot id to input, and
batch results summarize images as text rather than return image blocks. Use an
action's `observe` or a subsequent screenshot call for visual verification.
For sandbox workflows, retained screenshot OCR/diffs, owned app lifecycle,
traces or multi-call recordings, use one persistent MCP stdio connection, not
separate shell calls. There is no CLI daemon or broker.

## Permissions, safety and privacy

Host input affects the user's real desktop. Observe first, avoid unrelated
windows and secrets, and ask before destructive operations. A sandbox isolates
desktop interaction only, not files or network; it never falls back to host.
In a persistent MCP session: sandbox_start → target_select returned id →
app_launch → observe/input/verify → sandbox_stop (or disconnect for cleanup).

`--portal` explicitly permits a consent dialog on portal-only host backends.
Nothing is captured/injected through the portal before approval. Denial is
terminal for that session; do not bypass it. The timeout defaults to 150000 ms
(range 1000–600000); it cancels unanswered requests and closes the dialog.
One established session covers its granted capture/input/touch capabilities,
not subsequent fresh `call` processes. Sandboxes never use the portal.

Portal keyboard text is restricted to supported US-layout characters; Unicode
can fail preflight. For explicit clipboard-based portal typing, use one batch
with clipboard_write then key `["ctrl","v"]`; this replaces the user's clipboard.
Clipboard-assisted paste/copy preserves restorable formats up to 16 MiB or
refuses before overwrite; portal clipboard targets cannot enumerate formats and
therefore refuse these preservation-based operations. Touch needs a granted
portal capability or an explicit verified `--touch-uinput` mapping, never a
pointer imitation. `a11y_enable` reports pending host setting changes before
applying them; apply only with `confirm_desktop_change:true` when authorized.

Screenshots, logs, traces and recordings can contain secrets. Traces redact text
arguments, but not all context. Keep artifacts private. The activity indicator
is best-effort and can be disabled; its absence is not proof that nothing ran.
The server advertises tools only: no MCP resources, prompts or progress/logging
notifications are implemented. Tool descriptions/schemas and the filtered index
below are the source of truth for discovery.
