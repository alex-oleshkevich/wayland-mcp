import os
#!/usr/bin/env python3
"""Long-tail live battery for wayland-mcp (host sandbox). Usage: battery.py [binary] [fixture]"""
import json, subprocess, sys, time

BIN = sys.argv[1] if len(sys.argv) > 1 else os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "target", "release", "wayland-mcp")
FIX = sys.argv[2] if len(sys.argv) > 2 else os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "target", "release", "gtk-fixture")
p = subprocess.Popen([BIN], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
def s(o): p.stdin.write(json.dumps(o) + "\n"); p.stdin.flush()
def r():
    l = p.stdout.readline()
    if not l: raise RuntimeError("EOF " + p.stderr.read()[:3000])
    return json.loads(l)
s({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "battery", "version": "0"}}}); r()
s({"jsonrpc": "2.0", "method": "notifications/initialized"})
i = [10]; PASS = []; FAIL = []
def call(n, a={}):
    i[0] += 1; s({"jsonrpc": "2.0", "id": i[0], "method": "tools/call", "params": {"name": n, "arguments": a}}); res = r()
    if "error" in res: return {"_error": res["error"].get("message", "")}
    out = res["result"]; texts = [c.get("text", "") for c in out.get("content", []) if c.get("type") == "text"]
    imgs = [c for c in out.get("content", []) if c.get("type") == "image"]
    try: d = json.loads(texts[0]) if texts else {}
    except Exception: d = {"_raw": texts}
    d["_images"] = len(imgs); d["_mime"] = imgs[0].get("mimeType") if imgs else None
    return d
def check(name, cond, detail=""):
    (PASS if cond else FAIL).append(name); print(("PASS " if cond else "FAIL ") + name, str(detail)[:200], flush=True)
def app_lines(app, filt):
    return [l["line"] for l in call("app_logs", {"app_id": app, "filter": filt}).get("lines", [])]
def logs_until(app, filt, needle, tries=20, more_than=None):
    # Readbacks lag seconds: poll until a line containing needle appears, or until
    # the filtered line count exceeds the more_than baseline.
    lines = []
    for _ in range(tries):
        call("wait", {"ms": 700})
        lines = app_lines(app, filt)
        done = len(lines) > more_than if more_than is not None else any(needle in l for l in lines)
        if done: break
    return lines
def settled_logs(app, filt, tries=10):
    # Baseline: poll until the filtered line count is stable across two consecutive reads.
    prev = None; lines = []
    for _ in range(tries):
        call("wait", {"ms": 700})
        lines = app_lines(app, filt)
        if prev is not None and len(lines) == prev: break
        prev = len(lines)
    return lines
def text_until(role, expected, tries=20):
    # Poll a11y text of the first node with role until it is in `expected` (tuple).
    # A missing node reads as "<missing>" so it never satisfies an "empty" expectation.
    text = None
    for _ in range(tries):
        m = call("a11y_find", {"role": role}).get("matches", [])
        text = m[0].get("text") if m else "<missing>"
        if text in expected: break
        call("wait", {"ms": 700})
    return text
T1 = call("sandbox_start", {"width": 1280, "height": 720}).get("target", "sandbox-1"); call("target_select", {"target": T1}); call("a11y_enable", {})
app = call("app_launch", {"argv": [FIX], "wait_ms": 2200}).get("app_id")
check("fixture up", bool(app))

# --- visual variants ---
shot = call("screenshot", {"format": "jpeg", "max_dimension": 900})
check("jpeg screenshot", shot.get("_mime") == "image/jpeg", shot.get("_mime"))
sid = shot["shot_id"]
crop = call("screenshot", {"crop": {"x": 0, "y": 0, "width": 400, "height": 300}})
check("crop screenshot dims", crop.get("transform", {}).get("image_width") == 400, crop.get("transform"))
grid = call("screenshot", {"grid": True, "max_dimension": 700})
check("grid screenshot returns image", grid.get("_images") == 1)
marks = call("screenshot", {"marks": True, "max_dimension": 1000})
mk = marks.get("marks") or []
check("marks list from OCR", isinstance(mk, list) and len(mk) > 0 and mk[0].get("mark") == 1, str(mk)[:150])
cur = call("screenshot", {"include_cursor": True, "max_dimension": 600})
check("cursor screenshot ok", cur.get("_images") == 1 and cur.get("cursor_composited") == True)
check("pixel out of bounds error", "_error" in call("pixel", {"shot_id": sid, "x": 99999, "y": 0}))
full = call("screenshot", {})
check("diff size mismatch explicit", (call("diff", {"shot_id": full["shot_id"], "other_shot_id": crop["shot_id"]}).get("size_mismatch") or "") != "")
ft = call("find_text", {"shot_id": full["shot_id"], "query": "ncre"})
check("find_text literal", len(ft.get("matches", [])) > 0)
ftr = call("find_text", {"shot_id": full["shot_id"], "query": "nc.*re", "regex": True})
check("find_text regex", len(ftr.get("matches", [])) > 0)
check("find_text bad regex error", "_error" in call("find_text", {"shot_id": full["shot_id"], "query": "[", "regex": True}))
wt = call("wait_for_text", {"query": "ncre", "timeout_ms": 20000, "poll_ms": 400})
check("wait_for_text appear", wt.get("satisfied") == True, wt)
wtd = call("wait_for_text", {"query": "ncre", "mode": "disappear", "timeout_ms": 3000, "poll_ms": 400})
check("wait_for_text disappear unsatisfied is normal", wtd.get("satisfied") == False and "_error" not in wtd, wtd)

# --- input extras ---
rt = call("read_text", {"shot_id": full["shot_id"]})
inc = [w for w in rt.get("words", []) if "ncre" in w["text"].lower()][0]
cx, cy = inc["center_px"]
call("move", {"x": cx, "y": cy, "shot_id": full["shot_id"]})
call("mouse_button", {"button": "left", "pressed": True}); call("wait", {"ms": 150}); call("mouse_button", {"button": "left", "pressed": False})
check("mouse_button across calls clicks", any("count=1" in l for l in logs_until(app, "FIXTURE CLICK", "count=1")))
call("click", {"x": cx, "y": cy, "shot_id": full["shot_id"], "modifiers": ["ctrl"]})
check("click with modifiers dispatches", any("count=2" in l for l in logs_until(app, "FIXTURE CLICK", "count=2")))
call("click", {"x": cx, "y": cy, "shot_id": full["shot_id"], "long_press_ms": 400})
check("long press clicks", any("count=3" in l for l in logs_until(app, "FIXTURE CLICK", "count=3")))
words_before = [w["text"] for w in call("read_text", {"shot_id": call("screenshot", {})["shot_id"]}).get("words", [])]
sc = call("scroll", {"x": 350, "y": 400, "space": "logical", "dy": 5})
check("scroll granularity wheel", sc.get("granularity") == "wheel", sc)
words_after = words_before
for _ in range(10):
    call("wait", {"ms": 700})
    words_after = [w["text"] for w in call("read_text", {"shot_id": call("screenshot", {})["shot_id"]}).get("words", [])]
    if words_after != words_before: break
check("scroll changed view", words_before != words_after, (words_before[:3], words_after[:3]))
sch = call("scroll", {"x": 350, "y": 400, "space": "logical", "dx": 2, "dy": 0, "smooth": True})
check("horizontal smooth scroll dispatches", sch.get("granularity") == "smooth", sch)
def walk(n, acc):
    if isinstance(n, dict):
        if n.get("extents_screen"): acc.append(n["extents_screen"])
        for c in n.get("children", []): walk(c, acc)
    return acc
exts = walk(call("a11y_tree", {"max_nodes": 400}).get("tree", {}), [])
area = [e for e in exts if 80 <= e[3] <= 220 and e[2] > 400][0]
call("drag", {"from": {"x": area[0] + 30, "y": area[1] + area[3] // 2, "space": "logical"},
              "to": {"x": area[0] + 200, "y": area[1] + area[3] // 2, "space": "logical"},
              "waypoints": [{"x": area[0] + 120, "y": area[1] + area[3] // 2 + 20, "space": "logical"}], "dwell_ms": 100, "duration_ms": 500})
check("drag with waypoints/dwell", any("drag-end" in l for l in logs_until(app, "drag", "drag-end")))
check("click count 0 error", "_error" in call("click", {"x": 1, "y": 1, "space": "logical", "count": 0}))
check("click count 4 error", "_error" in call("click", {"x": 1, "y": 1, "space": "logical", "count": 4}))
check("key unknown name error", "_error" in call("key", {"keys": ["NotAKey"]}))
check("key_hold release unheld error", "_error" in call("key_hold", {"keys": ["shift"], "pressed": False}))
check("type_text bad mode error", "_error" in call("type_text", {"text": "x", "mode": "smoke-signals"}))
check("wait too long error", "_error" in call("wait", {"ms": 60001}))
check("screenshot unknown output error", "_error" in call("screenshot", {"output": "HDMI-99"}))
check("move unknown target error", "_error" in call("move", {"x": 1, "y": 1, "space": "logical", "target": "sandbox-99"}))

# --- typing/chords via a11y-verified entry ---
tb = call("a11y_find", {"role": "text box"})["matches"][0]
ex, ey, ew, eh = tb["extents_screen"]
call("click", {"x": ex + ew // 2, "y": ey + eh // 2, "space": "logical"}); call("wait", {"ms": 500})
call("key", {"keys": ["a"], "repeat": 3})
check("key repeat types aaa (a11y readback)", text_until("text box", ("aaa",)) == "aaa")
call("key", {"keys": ["ctrl", "a"]}); call("key", {"keys": ["Delete"]})
check("ctrl+a + Delete clears (chord masks)", text_until("text box", ("", None)) in ("", None))
call("type_text", {"text": "keep-me"})
check("typed keep-me (a11y readback)", text_until("text box", ("keep-me",)) == "keep-me")
call("key", {"keys": ["ctrl", "a"]})
rs = {}
for _ in range(5):
    rs = call("read_selection", {"via": "copy"})
    if "keep-me" in (rs.get("text") or ""): break
    call("wait", {"ms": 800})
check("read_selection via copy", "keep-me" in (rs.get("text") or ""), rs)
call("key", {"keys": ["ctrl", "a"]})
rsp = call("read_selection", {"via": "primary"})
check("read_selection via primary", "keep-me" in (rsp.get("text") or "") or "_error" in rsp, rsp)
call("clipboard_write", {"text": "seed"})
check("clipboard bogus mime error", "_error" in call("clipboard_read", {"mime": "application/x-nope"}))

# --- a11y errors ---
lbl = call("a11y_find", {"name": "Count:"})
if lbl.get("matches"):
    sv = call("a11y_set_value", {"ref_id": lbl["matches"][0]["ref_id"], "value": "nope"})
    check("set_value on non-editable errors clearly", "_error" in sv and "editable" in sv["_error"].lower(), sv)
btn = [m for m in call("a11y_find", {"name": "Increment"}).get("matches", []) if m.get("role") == "button"][0]
check("a11y_act bad action error", "_error" in call("a11y_act", {"ref_id": btn["ref_id"], "action": "Explode"}))
check("a11y_find no match count 0", call("a11y_find", {"name": "ZZZ-no-such-widget"}).get("count") == 0)

# --- clipboard preservation refusal ---
big = "B" * (17 * 1024 * 1024)
call("clipboard_write", {"text": big})
refuse = call("type_text", {"text": "x", "mode": "paste"})
check("unpreservable clipboard paste refused", "_error" in refuse and "preservation" in refuse["_error"], refuse)
cr = call("clipboard_read", {})
check("clipboard intact after refusal", cr.get("size") == len(big), cr.get("size"))
call("clipboard_write", {"text": "small"})

# --- changes/errors ---
check("baseline_compare missing error", "_error" in call("baseline_compare", {"name": "never-saved-zz"}))
call("baseline_save", {"name": "bat", "overwrite": True})
check("baseline_save overwrite refusal", "_error" in call("baseline_save", {"name": "bat"}))
check("trace_stop without trace error", "_error" in call("trace_stop", {}))
check("record_stop unknown error", "_error" in call("record_stop", {"recording_id": "rec-zz"}))
check("record_start bad name error", "_error" in call("record_start", {"name": "../evil"}))
ws = call("wait_screen", {"mode": "change", "timeout_ms": 1500, "poll_ms": 300})
check("wait_screen change timeout satisfied:false", ws.get("satisfied") == False and "_error" not in ws, ws)
call("trace_start", {})
call("click", {"x": cx, "y": cy, "space": "logical"})
tr = call("trace_stop", {})
check("trace has click", any(e.get("tool") == "click" for e in tr.get("events", [])), tr.get("event_count"))
before = len(settled_logs(app, "FIXTURE CLICK"))
rep = call("trace_replay", {"trace_id": tr.get("trace_id"), "dry_run": False})
after_lines = logs_until(app, "FIXTURE CLICK", None, more_than=before)
check("trace_replay EXECUTES (count grows)", rep.get("replayed", 0) >= 1 and len(after_lines) > before, (rep, before, len(after_lines)))
check("trace_replay missing file error", "_error" in call("trace_replay", {"path": "/nonexistent/trace.jsonl", "dry_run": True}))

# --- apps/targets errors + pagination ---
check("app_launch bad binary error", "_error" in call("app_launch", {"argv": ["/nonexistent/binary-zz"]}))
echo = call("app_launch", {"argv": ["/bin/echo", "page-test"], "wait_ms": 400})
l1 = call("app_logs", {"app_id": echo.get("app_id"), "max_lines": 1})
l2 = call("app_logs", {"app_id": echo.get("app_id"), "cursor": l1.get("next_cursor")})
check("app_logs cursor pagination", len(l2.get("lines", [])) == 0 and l2.get("next_cursor") == l1.get("next_cursor"), (l1.get("next_cursor"), l2))
check("target_select unknown error", "_error" in call("target_select", {"target": "sandbox-99"}))
check("sandbox_stop host refused", "_error" in call("sandbox_stop", {"target": "host"}))

# --- second sandbox isolation ---
T2 = call("sandbox_start", {}).get("target", "sandbox-2")
c1 = call("clipboard_write", {"text": "on-sandbox-1"})  # still on sandbox-1? active is sandbox-1
call("target_select", {"target": T2})
r2 = call("clipboard_read", {})
check("sandbox-2 clipboard isolated (empty)", r2.get("empty") == True, r2)
call("clipboard_write", {"text": "on-sandbox-2"})
call("target_select", {"target": T1})
r1 = call("clipboard_read", {})
check("sandbox-1 clipboard unaffected by sandbox-2", (r1.get("text") or "") in ("small", "on-sandbox-1"), r1.get("text"))
call("sandbox_stop", {"target": T2})

print(f"\n==== battery: {len(PASS)} passed, {len(FAIL)} failed {FAIL}", flush=True)
call("sandbox_stop", {})
p.terminate()
sys.exit(1 if FAIL else 0)
