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
./target/release/wayland-mcp --exclude-tools record_start,dbus_call
```

Unknown names in `--tools`/`--exclude-tools` fail startup. Filtering removes
both discovery entries and dispatch routes.

## Targets

- `host`: the desktop the server was started on (`WAYLAND_DISPLAY`).
- `sandbox-N`: created by `sandbox_start` — a nested **niri** compositor with a
  private Wayland socket, private D-Bus session, accessibility bus and
  clipboard. It isolates desktop interaction only; it is **not** a
  filesystem/network sandbox. Requires the `niri` executable; no other
  compositor is substituted. `target_select` switches explicitly; there is no
  automatic fallback from an unavailable sandbox to the host.

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
| dev | `sandbox_start`, `sandbox_stop`, `target_select`, `app_launch`, `app_list`, `app_logs`, `app_stop`, `app_restart`, `wayland_globals`, `dbus_call` |
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
- The activity indicator (layer-shell overlay) shows the action name and
  pointer ripple; it never has keyboard focus and never displays typed text.
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
  restart/stop (unknown processes refused), `dbus_call` (typed arguments,
  `a{sv}` replies, arity/type errors emit no call, system bus works). Tool
  filtering removes discovery and dispatch routes.
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
