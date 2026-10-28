# wayland-mcp reference

A Rust (`rmcp`, stdio) computer-use MCP server for Linux Wayland: screenshots for
agent visual context, real virtual-pointer/virtual-keyboard input, accessibility,
clipboard, change detection, private nested-compositor test targets, application
debugging, traces and recordings.

Capability-based, not compositor-name-based: backends are bound from what the
selected target actually advertises (wlr screencopy, virtual pointer/keyboard,
data-control clipboard, layer-shell indicator, AT-SPI). Unsupported capabilities
are reported explicitly — never silently faked, never bypassed after a portal
denial, and never fallen back from a private target to the host desktop.

## Run

```bash
cargo build --release
./target/release/wayland-mcp                 # MCP over stdio; logs on stderr
./target/release/wayland-mcp --tools doctor,screenshot,desktop_info
./target/release/wayland-mcp --exclude-tools record_start,app_launch
```

Unknown names in `--tools`/`--exclude-tools` fail startup. Filtering removes
both discovery entries and dispatch routes.

### Shell commands

```bash
wayland-mcp prime                         # agent guide + generated filtered tool index
wayland-mcp doctor                        # read-only capability table
wayland-mcp tools [--names]               # filtered index
wayland-mcp tools screenshot [--json]     # full description + input schema
wayland-mcp call screenshot '{"max_dimension":1200}' -o shot.png
```

Schemas come from the server's router. Unknown or filtered-out schema names
produce a JSON error on stderr and exit 1; close-match suggestions use only
visible tools. `--json` requires a tool name and cannot combine with `--names`.
`call` accepts one JSON object (default `{}`), uses real MCP dispatch and saves
only the first returned image with `-o`. Text/JSON goes to stdout; errors and
image notices go to stderr. No image with `-o` or an output write failure exits 1.
Malformed/non-object call JSON and CLI usage errors exit 2. Dispatch/tool errors
exit 1; `doctor` also exits 1 for an unreachable Wayland socket/registry. Inspect
normal results for `satisfied:false`, batch validation errors and failed steps;
these do not by themselves change exit 0.

### CLI parity

| MCP capability | CLI coverage / boundary |
|---|---|
| Tool discovery and input schemas | Covered: `tools`, `tools --names`, `tools NAME [--json]`; `prime` appends the generated filtered index. |
| Tool calls and filters | Covered: every registered tool dispatches through `call`, with the same `--tools`/`--exclude-tools` policy, including nested batch/replay checks. |
| Images | Covered: `-o FILE` saves the first image, with notices for additional images; it does not export every image in a multi-image result. |
| Resources and resource subscriptions | Not implemented by the server; no advertised resources or resource routes to expose. |
| Prompts and completion | Not implemented by the server; no prompt/completion handlers to expose. |
| Progress/logging/list-change notifications | Not emitted by the server; there is no progress stream to forward. CLI diagnostics already use stderr. |
| Sandboxes, selected targets, owned app ids/logs | Connection-scoped: each call starts on host; cleanup stops owned apps/sandboxes. Use persistent MCP stdio for their lifecycle. |
| Retained shots, layout tracking and shot-based OCR/diffs/input | Connection-scoped: prior-call shot ids are invalid. Use transform-based logical coordinates, same-call OCR marks or a persistent MCP connection. |
| Held input and clipboard ownership | Released/closed at call cleanup. Complete gestures/chords, paste mode where supported, or one batch can keep related actions together. Clipboard-manager persistence is not guaranteed. |
| Portal approval, backend devices and indicator | Connection-scoped: closed at cleanup; a new portal call can ask again. One batch can share consent across its allowed steps. |
| Active traces and recording handles | Connection-scoped: start/stop cannot span shell calls; recordings stop at cleanup. Use persistent MCP stdio. Disk artifacts remain readable; trace replay still validates target/layout/arguments. |
| Batch result references and image export | Not implemented by the server: batch prevalidates static arguments, permits only its action whitelist, and summarizes images as text. No new CLI batch semantics are invented. |

No daemon/background broker is introduced. The fresh-server boundary is
intentional: it prevents leaking owned processes/input and cannot be removed
without a different connection-lifetime model. Named baselines can be saved and
compared across calls because they are files. AT-SPI refs identify external live
bus objects rather than server-owned handles and may remain usable while those
objects exist. Desktop mutations are not rolled back.

Run `wayland-mcp prime` for coordinate conversion and shell workflow details;
install the agent skill with `npx skills add alex-oleshkevich/wayland-mcp`.

## Targets

- `host`: the desktop the server was started on (`WAYLAND_DISPLAY`).
- `sandbox-N`: created by `sandbox_start` — a nested **niri** compositor with a
  private Wayland socket, private D-Bus session, accessibility bus and
  clipboard. Services the private bus activates (portals, a11y bus, desktop
  daemons) run with the sandbox's `WAYLAND_DISPLAY`, `NIRI_SOCKET` and
  `XDG_RUNTIME_DIR`, never the host's, and are stopped with the sandbox. It
  isolates desktop interaction only; it is **not** a filesystem/network
  sandbox. Requires the `niri` executable; no other compositor is
  substituted. `target_select` switches explicitly; there is no automatic
  fallback from an unavailable sandbox to the host.

## Coordinate contract

`screenshot` returns an MCP image plus a `shot_id`. The stored shot is exactly
the returned image (omit `max_dimension` for native resolution), and its
transform maps returned-image pixels to desktop logical coordinates. Actions
accept `space:"screenshot"` with that `shot_id`, or `space:"logical"`. A shot
from another target or an older layout generation is rejected before input;
it is never rebound to a newer capture.

## Tool groups

| Group | Tools |
|---|---|
| visual | `desktop_info`, `screenshot`, `get_app_state`, `read_text`, `find_text`, `wait_for_text`, `pixel`, `diff` |
| input | `move`, `click`, `drag`, `scroll`, `mouse_button`, `key`, `key_hold`, `type_text`, `release_all`, `wait` |
| touch | `touch_tap`, `touch_swipe`, `touch_pinch` |
| accessibility | `a11y_enable`, `a11y_tree`, `a11y_find`, `a11y_act`, `a11y_set_value` |
| clipboard | `clipboard_read`, `clipboard_write`, `read_selection` |
| changes | `wait_screen`, `frame_probe`, `baseline_save`, `baseline_compare` |
| dev | `sandbox_start`, `sandbox_stop`, `target_select`, `app_launch`, `app_list`, `app_logs`, `app_stop`, `app_restart`, `wayland_globals` |
| automation | `batch`, `trace_start`, `trace_stop`, `trace_replay`, `record_start`, `record_stop` |
| diagnostics | `doctor` (read-only; never prompts, downloads, or changes settings) |

Mutating tools accept `observe:{delays_ms, max_dimension, include_tree}` and
return a post-action observation in the same response. If dispatch succeeds but
observation fails, both facts are reported and nothing is replayed.

## Privacy boundaries

- Artifacts (baselines, traces, recordings) are stored under
  `$XDG_DATA_HOME/wayland-mcp` (fallback `~/.local/share/wayland-mcp`).
- Traces record only actions this server issues while a trace is active;
  typed text is redacted to a character count. Traces/recordings can contain
  secrets and stay local until explicitly read or exported. Screenshot content
  sent as MCP images goes to the calling model provider, as with any MCP image.
- The activity indicator (layer-shell overlay) shows a history of the last N
  actions (`--indicator-history`, default 5), each for `--indicator-ttl-ms`
  (default 3000) before it disappears, with their values (coordinates,
  buttons, named keys, and the text typed with `type_text` as one `Type: ...`
  / `Paste: ...` line); it never has keyboard focus. Typed text is therefore
  visible on screen and in recordings with `include_indicator`; it is not added
  to tool results, events or traces. Bare printable key presses show as `*`.
  When typing secrets use `--no-indicator` or record with `include_indicator:false`.
  Captures hide it and wait for the compositor roundtrip acknowledgement.
  On desktops without layer-shell (e.g. GNOME without an extension, which this
  server will not install) it reports unavailable.
- Clipboard-assisted operations preserve every advertised MIME payload within
  a 16 MiB limit or refuse before overwriting. Targets whose clipboard is the
  portal (GNOME) cannot enumerate MIME types, so they refuse these operations
  (see GNOME limits); KDE uses native ext data-control for the clipboard and
  only sends the ctrl+v/ctrl+c chord through the portal, so they work there.
  Paste waits (≤2 s) for a clipboard transfer before restoring, reporting
  `paste_consumed:false` if none came; copy waits (≤2 s) for a
  selection-owner change and returns `copied:false`, clipboard untouched, if
  nothing was copied. `paste_consumed` is a heuristic: data-control cannot
  identify the reader, so reads in the first 250 ms (eager clipboard managers
  such as Klipper) are excluded, and a manager that reads later can still
  yield a false `true`. Verified on KDE with Klipper running.
- OCR (`ocrs`, CPU) models live in `--ocr-dir` or
  `$XDG_CACHE_HOME/ocrs`; they are downloaded only by an OCR call with
  `download_models:true`. The bundled alphabet covers Latin scripts.

## Validation matrix

Status key: **verified** means exercised live with the evidence noted.
**PENDING** means not yet validated. Anything not listed is not claimed.
Commands and per-area coverage: [TESTING.md](TESTING.md).

| Environment | Status |
|---|---|
| niri host + nested niri sandbox, GTK fixture | verified (below) |
| GNOME VM guest (mutter), no consent | verified: not-enabled guidance, nothing captured or injected |
| GNOME VM guest, portal consent (`--portal`) | verified: capture, click, ASCII type_text, touch, portal clipboard (text read/write, external paste, external copy read) |
| KDE Plasma 6.8.1 VM guest (KWin), no consent | verified: not-enabled guidance, nothing captured or injected |
| KDE Plasma 6.8.1 VM guest, portal consent (`--portal`) | verified: capture, keyboard/ASCII type_text, `touch_tap`, `touch_swipe`, `touch_pinch`, pointer click (Qt app and GTK4 fixture), native ext-data-control clipboard, Cancel terminal, orphan-timeout cleanup |
| Physical host touchscreen | host hardware unvalidated; the evdev touch backend is verified in a VM (niri, virtio-multitouch): tap/swipe/pinch pass, bogus-mapping refusals pass |
| Quarter-turn output transforms | verified: all 8 `wl_output` transform codes (0-7) read upright on niri/virtio-gpu; flipped quarter-turns compose flip then rotate |

niri host + sandbox:

- Capture: native wlr-screencopy. Orientation verified pixel-wise against
  `grim` and OCR (Normal, Flipped180). Stored shot equals returned pixels.
  Stale-generation and cross-target shot ids are rejected before input.
- Input: click count, modifiers, long-press, `mouse_button` across calls,
  drag with waypoints and dwell, scroll (wheel and smooth), key repeat, holds,
  `release_all`, post-action observe. Ctrl chords require an explicit
  modifiers mask. Unicode `héllo ✓ 123` is typed exactly through the spare
  F13–F24 keymap extension.
- AT-SPI: tree, find, act, set-value with readback. Stale refs and
  non-editable set-value return explicit errors. The sandbox runs its own
  `at-spi2-registryd`.
- Clipboard: wlr data-control, cross-process in both directions against
  `wl-copy`/`wl-paste`. Paste preserves all MIME payloads up to 16 MiB, or
  refuses before overwriting (checked at 17 MiB). An empty clipboard is a
  normal state.
- Transport: large requests are served with 64 MiB lines (17 MiB round trip).
- Changes: baseline pass/fail with regions; size mismatch is reported, never
  rescaled. `wait_screen`/`wait_for_text` return `satisfied:false` on timeout
  as a normal result. `frame_probe` reports sampling pattern, not FPS.
- Automation: batch validates all steps before dispatch and stops on the first
  error by default. Trace replay executes and refuses when the recorded target
  is gone. Recordings are ffprobe-valid H.264; encoder errors are reported.
- Dev: app launch/list/logs (cursor pagination, filter, `WAYLAND_DEBUG=client`),
  restart/stop (unknown processes refused). Tool filtering removes discovery
  and dispatch routes.
- Indicator: layer-shell overlay appears in recordings and is hidden for
  captures via acknowledged hide. Input region is empty, so it is click-through.
- Diagnostics: `doctor` is read-only. `a11y_enable` reports the pending desktop
  change before applying it.

GNOME limits (explicit errors, not faked):

- Native screencopy, virtual pointer, virtual keyboard and layer-shell are
  unavailable. Without `--portal`, screenshot and click return
  not-enabled guidance and nothing is captured or injected.
- Clicking Cancel in the consent dialog is terminal. There is no fallback.
- One consent covers capture, input and touch.
- Non-ASCII `type_text` returns a preflight error naming the characters and
  dispatches nothing. Use `clipboard_write` then a `key` ctrl+v (verified), since
  `type_text mode=paste` refuses on the portal (below). Mutter only honors keymap
  keysyms.
- Clipboard (portal, ashpd Clipboard on the same RemoteDesktop session; the
  consent dialog gains "Allow Clipboard Access"): text only. Verified live on
  mutter: `clipboard_write` then `clipboard_read` returns exact
  `portal-clip-✓` as text/plain; a ctrl+v in the GTK fixture pastes it (another
  app reads our selection); after the fixture copied text, `clipboard_read`
  returned it (real SelectionRead). Non-text MIME read/write and primary
  selection return explicit errors. Mutter serves reads of our own selection
  from the owned text.
- Clipboard-assisted paste/copy (`type_text mode=paste`, `read_selection
  via=copy`) refuse on portal targets: "portal clipboard cannot enumerate MIME
  formats, so preservation of the existing clipboard cannot be verified;
  refusing clipboard-assisted paste/copy rather than risk destroying non-text
  clipboard data (use type_text mode='keys', or an explicit clipboard_write)".
- Backend order: wlr data-control, ext data-control, portal (portal only with
  `--portal`, never in the sandbox).
- Indicator reports unavailable (no layer-shell).

`touch_tap` and `touch_swipe` drive the fixture's drag gesture through the
GNOME portal (touchscreen granted by RemoteDesktop alone).

KDE Plasma / KWin 6.8.1 facts (explicit errors, not faked):

- KWin advertises no `zwlr_screencopy`, virtual-pointer or virtual-keyboard
  globals; layer-shell is available. Without `--portal`, screenshot
  and click return not-enabled guidance.
- The portal dialog is a single "Remote Control Requested" Allow/Deny prompt
  with no output picker. Deny is terminal; there is no fallback.
- With consent: capture, `key`/ASCII `type_text` (non-ASCII refused in
  preflight), `touch_tap` and a pointer click on a Qt app (System Settings)
  verified. Touch is granted by RemoteDesktop alone.
- KWin advertises only `ext_data_control_manager_v1`; the clipboard tools use it
  natively (roundtrip verified on KDE).
- Pointer click and `touch_swipe` on the GTK fixture work (3 fresh fixtures,
  every click registered); the earlier failure was attributed to a stale guest
  binary/state and was not reproduced. `touch_pinch` is verified quantitatively
  (scale 1.11 to 3.25 for a 260/80 spread, reverse 0.31).
- An orphaned consent dialog is closed by the establishment timeout (dialog gone
  at +15 s on the real KDE portal; a fresh session recovers).

Fixed during testing (no open known issues):

- Evdev touch testing: the indicator surface used a NULL input region
  (compositor default, which swallowed touch); it now sets an explicit empty
  `wl_region`.

Known limitation: a portal screen stream does not follow an output mode or
scale change. After changing resolution/scale on KDE, `screenshot` for that
output returns an explicit "no matching screen stream" error until the portal
session is restarted. It never returns the old geometry's pixels.

Not validated: physical host touchscreen hardware (the evdev backend is
VM-verified only).

## Fixture

```bash
cargo build --features fixture --bin gtk-fixture
```

A GTK4 app (editable entry, Increment button/count, scrollable list, drag
surface, stdout event log) used by the verification flow above.

Build and run instructions, test batteries and the VM recipe: [TESTING.md](TESTING.md).
