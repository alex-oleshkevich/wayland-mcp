import os
import json, subprocess, sys
BIN=os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "target", "release", "wayland-mcp"); FIX=os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "target", "release", "gtk-fixture")
p=subprocess.Popen([BIN],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True)
def s(o): p.stdin.write(json.dumps(o)+"\n"); p.stdin.flush()
def r():
 l=p.stdout.readline()
 if not l: raise RuntimeError("EOF "+p.stderr.read()[:3000])
 return json.loads(l)
s({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"s","version":"0"}}}); r()
s({"jsonrpc":"2.0","method":"notifications/initialized"})
i=[10]; PASS=[]; FAIL=[]
def call(n,a={}):
 i[0]+=1; s({"jsonrpc":"2.0","id":i[0],"method":"tools/call","params":{"name":n,"arguments":a}}); res=r()
 if "error" in res: return {"_error":res["error"].get("message","")}
 out=res["result"]; texts=[c.get("text","") for c in out.get("content",[]) if c.get("type")=="text"]
 try: return json.loads(texts[0]) if texts else {}
 except: return {"_raw":texts}
def check(name, cond, detail=""):
 (PASS if cond else FAIL).append(name); print(("PASS " if cond else "FAIL ")+name, str(detail)[:220])

import os, time, glob
_socks=sorted(glob.glob(f"/run/user/{os.getuid()}/niri.wayland-1.*.sock"), key=os.path.getmtime)
if not _socks: sys.exit("host niri socket for wayland-1 not found; containment impossible, refusing to run")
HENV=dict(os.environ, NIRI_SOCKET=_socks[-1])
def niri(*a): return subprocess.run(["niri","msg","--json",*a],env=HENV,capture_output=True,text=True).stdout
before={w["id"] for w in json.loads(niri("windows"))}; focused=(json.loads(niri("focused-window")) or {}).get("id")
SB=call("sandbox_start",{"width":1280,"height":720}); call("target_select",{"target":SB["target"]})
time.sleep(1)
for w in json.loads(niri("windows")):
 if w["id"] not in before: subprocess.run(["niri","msg","action","move-window-to-monitor","--id",str(w["id"]),"eDP-1"],env=HENV)
if focused is not None: subprocess.run(["niri","msg","action","focus-window","--id",str(focused)],env=HENV)
call("a11y_enable",{})
launch=call("app_launch",{"argv":[FIX],"wayland_debug":True,"wait_ms":2500})
check("fixture running", launch.get("status")=="running", launch)
logs={}
for _ in range(30):
 logs=call("app_logs",{"app_id":"app-1","filter":"FIXTURE READY"})
 if any("READY" in l["line"] for l in logs.get("lines",[])): break
 time.sleep(0.5)
check("fixture ready log", any("READY" in l["line"] for l in logs.get("lines",[])))
call("key",{"keys":["Escape"]}); time.sleep(0.5)  # dismiss niri hotkey overlay; sandbox target is selected
logs=call("app_logs",{"app_id":"app-1","filter":"zwlr_screencopy"})
check("wayland_debug lines captured", logs.get("buffered_lines",0)>50, logs.get("buffered_lines"))
def words():
 sh=call("screenshot",{}); return sh, call("read_text",{"shot_id":sh["shot_id"]}).get("words",[])
sh,w=words()
ent=[x for x in w if x["text"].lower()=="here"]
inc=[x for x in w if "cremen" in x["text"].lower()]
check("found entry+button", bool(ent) and bool(inc))
ex,ey=ent[0]["center_px"]
call("click",{"x":ex+200,"y":ey,"shot_id":sh["shot_id"]}); call("wait",{"ms":600})
call("type_text",{"text":"copy-me-✓"}); call("wait",{"ms":800})
call("clipboard_write",{"text":"PRIOR"})
call("key",{"keys":["ctrl","a"]}); call("wait",{"ms":300})
rr=call("read_selection",{"via":"copy"}); print("COPY_OK",json.dumps(rr,ensure_ascii=False))
check("copy happy", rr.get("copied") is True and rr.get("text")=="copy-me-✓" and rr.get("clipboard_restored") is True, rr)
cr=call("clipboard_read",{}); check("restored PRIOR after copy", cr.get("text")=="PRIOR", cr)
# focus non-text widget
sh,w=words(); inc=[x for x in w if "cremen" in x["text"].lower()]
bx,by=inc[0]["center_px"]; call("click",{"x":bx,"y":by,"shot_id":sh["shot_id"]}); call("wait",{"ms":500})
rr=call("read_selection",{"via":"copy"}); print("COPY_NONE",json.dumps(rr,ensure_ascii=False))
check("copy error path", rr.get("copied") is False and rr.get("text") is None and rr.get("clipboard_restored") is False, rr)
cr=call("clipboard_read",{}); check("PRIOR untouched after failed copy", cr.get("text")=="PRIOR", cr)
rr=call("type_text",{"text":"nopaste","mode":"paste"}); print("PASTE_NONE",json.dumps(rr,ensure_ascii=False))
check("paste_consumed false", rr.get("paste_consumed") is False and rr.get("clipboard_restored") is True, rr)
cr=call("clipboard_read",{}); check("PRIOR after unconsumed paste", cr.get("text")=="PRIOR", cr)
call("sandbox_stop",{})
print("==== %d passed, %d failed: %s"%(len(PASS),len(FAIL),FAIL))
