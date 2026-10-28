# Testing

Status and claims: see the Validation matrix in [REFERENCE.md](REFERENCE.md).
This file covers how to run the checks and where each area is verified.

## Non-VM checks

```bash
cargo build
cargo test
cargo clippy --all-targets
```

Unit tests live beside their implementations. `tests/cli.rs` exercises the
compiled CLI without a Wayland display: named-tool text/JSON schemas, unknown
names and suggestions, filtered discovery, and incompatible output flags.
The agent guide text is not snapshot-tested.

For a read-only smoke check inside a live Wayland session:

```bash
./target/debug/wayland-mcp prime
./target/debug/wayland-mcp tools screenshot
./target/debug/wayland-mcp tools nope             # JSON error on stderr; exit 1
./target/debug/wayland-mcp call wayland_globals '{"filter":"screencopy"}'
```

## Live batteries (`.vmtest/`)

These drive the release binary over stdio against a live session. They need
a host niri session (for the sandbox), `ffmpeg`/`ffprobe` and the GTK fixture.
Default paths point at `target/release/`.

```bash
cargo build --release
cargo build --release --features fixture --bin gtk-fixture
python3 .vmtest/battery.py [binary] [fixture]
python3 .vmtest/battery2.py
python3 .vmtest/fixture_suite.py
python3 .vmtest/fixture_clipboard.py
```

- `battery.py`: long-tail visual checks. JPEG, crop, grid, marks, cursor
  compositing, out-of-bounds pixel, diff size mismatch.
- `battery2.py`: clipboard refusal and intact restore at 17 MiB, empty
  clipboard, baseline/trace/record/wait error paths,
  app and target errors, sandbox isolation.
- `fixture_suite.py`: GTK fixture end to end. OCR, clicks, Unicode typing,
  AT-SPI set-value, paste with restore, baselines, batch validation.
- `fixture_clipboard.py`: `read_selection via=copy` (copied, nothing
  selected) and paste with no consumer (`paste_consumed:false`); prior
  clipboard intact in every case.

The fixture scripts use release binaries and move the nested sandbox window
to eDP-1, then restore host focus.

All four print `PASS`/`FAIL` per check.

Other files in `.vmtest/` (`vm.qcow2`, `serial.log`, `OVMF_VARS.fd`,
`screen.*`, ISO, keys, `type.py`) are local VM artifacts. Do not commit them.

## GTK fixture

```bash
cargo build --features fixture --bin gtk-fixture        # debug
cargo build --release --features fixture --bin gtk-fixture
```

A GTK4 app with an editable entry, Increment button and count, scrollable
list, drag surface, and stdout event log. Launch it through `app_launch`
inside a sandbox, or directly for manual runs.

## VM guest (GNOME portal)

Used to validate the portal path on mutter. Summary only:

1. Arch guest under QEMU with GNOME and the GTK fixture installed.
2. Reach the guest over SSH with the key in `.vmtest/` (`vmkey`).
3. Start the server in the guest session with `--portal`.
4. Grant consent through the QEMU monitor when the portal dialog appears.
   One consent covers capture, input and touch.
5. Without `--portal`, expect not-enabled guidance and no capture
   or injection. Cancelling the dialog is terminal.

The same procedure was repeated on a KDE Plasma 6.8.1 guest (GDM kept, session
switched via AccountsService). KWin's dialog is a single Allow/Deny prompt.

## Coverage

| Area | Where verified | Result |
|---|---|---|
| Capture orientation | niri host, `grim` + OCR; niri/virtio-gpu VM | Normal, Flipped180 on host; all 8 `wl_output` transform codes (0-7) read upright on virtio-gpu; flipped quarter-turns compose flip then rotate |
| Shot coordinate contract | niri sandbox | stored shot equals returned pixels; stale/cross-target rejected |
| Physical input | niri sandbox, `battery.py` | verified incl. modifiers, drag, scroll, key repeat, holds |
| Unicode typing | niri sandbox, `fixture_suite.py` | `héllo ✓ 123` exact via F13–F24 keymap |
| AT-SPI | niri sandbox | tree/find/act/set-value with readback; errors explicit |
| Clipboard | niri sandbox, `battery2.py`, `fixture_clipboard.py` | cross-process both ways; 16 MiB preserve, 17 MiB refused; paste/copy restore waits for app transfer/ownership |
| Transport | niri sandbox | 17 MiB round trip with 64 MiB lines |
| Changes | niri sandbox | baseline regions; `wait_screen` timeout as normal result |
| Batch / trace / recording | niri sandbox | fail-fast batch; replay executes; ffprobe-valid H.264 |
| App / tool filter | niri sandbox, `battery2.py` | lifecycle, logs; unknown filters fail startup |
| Indicator | niri sandbox | visible in recordings, hidden for captures, click-through |
| Diagnostics | niri host | `doctor` read-only; `a11y_enable` reports pending change |
| GNOME no consent | GNOME VM | not-enabled guidance; nothing captured or injected |
| GNOME portal capture/input | GNOME VM | screenshot OCR-verified; click counts; ASCII type exact |
| GNOME touch | GNOME VM | `touch_tap`/`touch_swipe` drive fixture drag |
| GNOME limits | GNOME VM | non-ASCII type refused in preflight; non-text MIME and primary selection error explicitly; paste/copy-assist refused on portal |
| GNOME clipboard | GNOME VM, real portal | text-only: `clipboard_write`/`clipboard_read` exact `portal-clip-✓`; fixture ctrl+v pastes it; fixture copy `fx-own` read back (SelectionRead); consent dialog has "Allow Clipboard Access" |
| KDE no consent | KDE Plasma 6.8.1 VM | not-enabled guidance for screenshot/click |
| KDE portal capture/keys | KDE Plasma 6.8.1 VM | screenshot + `read_text` find fixture text; ASCII type exact (entry focused via Tab); non-ASCII refused in preflight; Cancel terminal |
| KDE portal touch | KDE Plasma 6.8.1 VM | `touch_tap` increments fixture; `touch_swipe` works; `touch_pinch` scale 1.11 to 3.25 (260/80), reverse 0.31 |
| KDE portal pointer | KDE Plasma 6.8.1 VM | click works on Qt (System Settings) and the GTK4 fixture (3 fresh fixtures, every click registered) |
| KDE clipboard | KDE Plasma 6.8.1 VM | native `ext_data_control` roundtrip passes; paste `pasted-kde-✓` lands (`paste_consumed:true`), copy reads it back, both restore `PRIOR-kde`; nothing-selected copy → `copied:false`; paste into a button → `paste_consumed:false` (failed before the Klipper settle fix); `clipboard_write` + ctrl+v types `héllo ✓` |
| KDE portal orphan timeout | KDE Plasma 6.8.1 VM, real portal | orphaned dialog closed at +15 s; fresh session recovers |
| evdev touch | niri on virtio-multitouch VM | tap/swipe/pinch pass, bogus-mapping refusals pass; indicator empty input-region bug found and fixed |
| Physical host touchscreen | not run | host hardware unvalidated (evdev backend verified in the VM above) |

## OCR models

`ocrs` runs on CPU. Models live in `--ocr-dir` or
`$XDG_CACHE_HOME/ocrs` (default `~/.cache/ocrs`). They download only on an OCR
call with `download_models:true`.

## Build profile

Run OCR-dependent checks with `--release`. Debug-build OCR is very slow and
can time out waits.
