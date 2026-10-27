import os
#!/usr/bin/env python3
"""Long-tail battery part 2: clipboard refusal, trace exec, dbus variants, app/target errors, sandbox isolation."""
import json, subprocess, sys

BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "target", "release", "wayland-mcp")
FIX = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "target", "release", "gtk-fixture")
p = subprocess.Popen([BIN], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
def s(o): p.stdin.write(json.dumps(o) + "\n"); p.stdin.flush()
def r():
    l = p.stdout.readline()
    if not l: raise RuntimeError("EOF " + p.stderr.read()[:3000])
    return json.loads(l)
s({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "battery2", "version": "0"}}}); r()
s({"jsonrpc": "2.0", "method": "notifications/initialized"})
i = [10]; PASS = []; FAIL = []
def call(n, a={}):
    i[0] += 1; s({"jsonrpc": "2.0", "id": i[0], "method": "tools/call", "params": {"name": n, "arguments": a}}); res = r()
    if "error" in res: return {"_error": res["error"].get("message", "")}
    out = res["result"]; texts = [c.get("text", "") for c in out.get("content", []) if c.get("type") == "text"]
    imgs = [c for c in out.get("content", []) if c.get("type") == "image"]
    try: d = json.loads(texts[0]) if texts else {}
    except Exception: d = {"_raw": texts}
    d["_images"] = len(imgs)
    return d
def check(name, cond, detail=""):
    (PASS if cond else FAIL).append(name); print(("PASS " if cond else "FAIL ") + name, str(detail)[:200], flush=True)
def logs_until(app, filt, needle, tries=14, more_than=None):
    lines = []
    for _ in range(tries):
        call("wait", {"ms": 700})
        lines = [l["line"] for l in call("app_logs", {"app_id": app, "filter": filt}).get("lines", [])]
        done = len(lines) > more_than if more_than is not None else any(needle in l for l in lines)
        if done: break
    return lines

T1 = call("sandbox_start", {"width": 1280, "height": 720}).get("target", "sandbox-1"); call("target_select", {"target": T1}); call("a11y_enable", {})
app = call("app_launch", {"argv": [FIX], "wait_ms": 2200}).get("app_id")
check("fixture up", bool(app))

# --- clipboard preservation refusal (17 MiB > 16 MiB limit) ---
big = "B" * (17 * 1024 * 1024)
call("clipboard_write", {"text": big})
refuse = call("type_text", {"text": "x", "mode": "paste"})
check("unpreservable clipboard paste refused", "_error" in refuse and "preservation" in refuse["_error"], refuse)
cr = call("clipboard_read", {})
check("clipboard intact after refusal", cr.get("size") == len(big), cr.get("size"))
call("clipboard_write", {"text": "small"})
check("empty clipboard is normal", call("clipboard_write", {"text": ""}) is not None and call("clipboard_read", {}).get("empty") == True)

# --- changes / trace / record errors ---
check("baseline_compare missing error", "_error" in call("baseline_compare", {"name": "never-saved-zz"}))
call("baseline_save", {"name": "bat", "overwrite": True})
check("baseline_save overwrite refusal", "_error" in call("baseline_save", {"name": "bat"}))
check("trace_stop without trace error", "_error" in call("trace_stop", {}))
check("record_stop unknown error", "_error" in call("record_stop", {"recording_id": "rec-zz"}))
check("record_start bad name error", "_error" in call("record_start", {"name": "../evil"}))
ws = call("wait_screen", {"mode": "change", "timeout_ms": 1500, "poll_ms": 300})
check("wait_screen change timeout satisfied:false", ws.get("satisfied") == False and "_error" not in ws, ws)
btn = [m for m in call("a11y_find", {"name": "Increment"}).get("matches", []) if m.get("role") == "button"][0]
bx, by, bw, bh = btn["extents_screen"]
call("trace_start", {})
call("click", {"x": bx + bw // 2, "y": by + bh // 2, "space": "logical"})
tr = call("trace_stop", {})
check("trace has click", any(e.get("tool") == "click" for e in tr.get("events", [])), tr.get("event_count"))
n_before = len(logs_until(app, "FIXTURE CLICK", "count=", tries=3))
rep = call("trace_replay", {"trace_id": tr.get("trace_id"), "dry_run": False})
n_after = len(logs_until(app, "FIXTURE CLICK", "count=", tries=20, more_than=n_before))
check("trace_replay EXECUTES (count grows)", rep.get("replayed", 0) >= 1 and n_after > n_before, (rep, n_before, n_after))
check("trace_replay missing file error", "_error" in call("trace_replay", {"path": "/nonexistent/trace.jsonl", "dry_run": True}))
other = call("trace_replay", {"trace_id": tr.get("trace_id"), "dry_run": True})
check("trace_replay dry-run does not execute", other.get("dry_run") == True and len(logs_until(app, "FIXTURE CLICK", "count=", tries=3)) == n_after)

# --- frame_probe with change (click during probe handled separately: static pattern here) ---
fp = call("frame_probe", {"duration_ms": 6000, "interval_ms": 250})
check("frame_probe samples", len(fp.get("samples", [])) >= 3 and fp.get("pattern") == "still", fp.get("pattern"))

# --- dbus variants ---
check("dbus arity mismatch error", "_error" in call("dbus_call", {"destination": "org.freedesktop.DBus", "path": "/org/freedesktop/DBus", "interface": "org.freedesktop.DBus", "method": "GetNameOwner", "signature": "ss", "args": ["only-one"]}))
sysr = call("dbus_call", {"bus": "system", "destination": "org.freedesktop.DBus", "path": "/org/freedesktop/DBus", "interface": "org.freedesktop.DBus", "method": "ListNames"})
check("dbus system bus ListNames", "_error" not in sysr and "org.freedesktop.DBus" in json.dumps(sysr), sysr)
props = call("dbus_call", {"destination": "org.freedesktop.DBus", "path": "/org/freedesktop/DBus", "interface": "org.freedesktop.DBus.Properties", "method": "GetAll", "signature": "s", "args": ["org.freedesktop.DBus"]})
check("dbus a{sv} reply decoded", "_error" not in props and isinstance(props.get("reply"), list), props)
vset = call("dbus_call", {"destination": "org.a11y.Bus", "path": "/org/a11y/bus", "interface": "org.freedesktop.DBus.Properties", "method": "Set", "signature": "ssv", "args": ["org.a11y.Status", "IsEnabled", True]})
check("dbus variant arg (ssv Set)", "_error" not in vset, vset)
gid = call("dbus_call", {"destination": "org.freedesktop.DBus", "path": "/org/freedesktop/DBus", "interface": "org.freedesktop.DBus", "method": "GetId"})
check("dbus no-arg call", "_error" not in gid and bool(gid.get("reply")), gid)
bad = call("dbus_call", {"destination": "org.freedesktop.DBus", "path": "/org/freedesktop/DBus", "interface": "org.freedesktop.DBus", "method": "GetNameOwner", "signature": "s", "args": [123]})
check("dbus arg type mismatch error (no call)", "_error" in bad, bad)

# --- apps/targets errors + pagination ---
check("app_launch bad binary error", "_error" in call("app_launch", {"argv": ["/nonexistent/binary-zz"]}))
echo = call("app_launch", {"argv": ["/bin/echo", "page-test"], "wait_ms": 400})
l1 = call("app_logs", {"app_id": echo.get("app_id"), "max_lines": 1})
l2 = call("app_logs", {"app_id": echo.get("app_id"), "cursor": l1.get("next_cursor")})
check("app_logs cursor pagination", len(l2.get("lines", [])) == 0 and l2.get("next_cursor") == l1.get("next_cursor"), (l1.get("next_cursor"), l2))
st = call("app_stop", {"app_id": echo.get("app_id")})
check("app_stop already-exited ok (M3: already_exited, no signal)", "_error" not in st and st.get("already_exited") == True and st.get("escalated") == False, st)
check("app_restart unknown error", "_error" in call("app_restart", {"app_id": "app-zz"}))
check("target_select unknown error", "_error" in call("target_select", {"target": "sandbox-99"}))
check("sandbox_stop host refused", "_error" in call("sandbox_stop", {"target": "host"}))

# --- second sandbox isolation ---
T2 = call("sandbox_start", {}).get("target", "sandbox-2")
call("target_select", {"target": T1})
call("clipboard_write", {"text": "on-sandbox-1"})
call("target_select", {"target": T2})
r2 = call("clipboard_read", {})
check("sandbox-2 clipboard isolated (empty)", r2.get("empty") == True, r2)
call("clipboard_write", {"text": "on-sandbox-2"})
call("target_select", {"target": T1})
r1 = call("clipboard_read", {})
check("sandbox-1 clipboard unaffected by sandbox-2", (r1.get("text") or "") == "on-sandbox-1", r1.get("text"))
call("sandbox_stop", {"target": T2})
check("sandbox-2 gone after stop", "_error" in call("target_select", {"target": T2}))

# --- get_app_state combined + doctor on sandbox ---
gs = call("get_app_state", {})
check("get_app_state has image + a11y json", gs.get("_images") == 1, gs.keys() if isinstance(gs, dict) else "")
doc = call("doctor", {})
check("doctor checks present", len(doc.get("checks", [])) >= 8, len(doc.get("checks", [])))

print(f"\n==== battery2: {len(PASS)} passed, {len(FAIL)} failed {FAIL}", flush=True)
call("sandbox_stop", {})
p.terminate()
sys.exit(1 if FAIL else 0)
