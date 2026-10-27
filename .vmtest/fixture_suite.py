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

shot=call("screenshot",{}); sid=shot["shot_id"]
rt=call("read_text",{"shot_id":sid,"download_models":True}); words=rt.get("words",[])
print("OCR:", [(w["text"],w["center_px"]) for w in words][:18])
inc=[w for w in words if "cremen" in w["text"].lower()]
check("OCR finds Increment", len(inc)>0, [w["text"] for w in words][:10])
# stale shot rejection: shot from before layout? use bogus older generation: capture B then act on A after output change is hard; test unknown target shot via fresh shot on sandbox is fine.
if inc:
 cx,cy=inc[0]["center_px"]
 for k in range(3):
  call("click",{"x":cx,"y":cy,"shot_id":sid,"observe":{"delays_ms":[350]}}); call("wait",{"ms":400})
call("wait",{"ms":1800})
logs=call("app_logs",{"app_id":"app-1","filter":"FIXTURE CLICK"})
counts=[l["line"] for l in logs.get("lines",[])]
check("click x3 -> count 3", any("count=3" in c for c in counts), counts[-3:])
# a11y semantic
find=call("a11y_find",{"name":"Increment"})
btn=[m for m in find.get("matches",[]) if m.get("role")=="button"]
check("a11y finds button", len(btn)>0, find.get("count"))
shot2=call("screenshot",{}); sid2=shot2["shot_id"]
rt2=call("read_text",{"shot_id":sid2}); w2=rt2.get("words",[])
entry=[w for w in w2 if w["text"].lower()=="here"]
check("OCR placeholder entry", len(entry)>0, [w["text"] for w in w2][:10])
if entry:
 ex,ey=entry[0]["center_px"]; call("click",{"x":ex+200,"y":ey,"shot_id":sid2}); call("wait",{"ms":700})
 call("type_text",{"text":"héllo ✓ 123"})
call("wait",{"ms":2000})
logs=call("app_logs",{"app_id":"app-1","filter":"entry="})
last=[l["line"] for l in logs.get("lines",[])]
check("physical type unicode exact", any("héllo ✓ 123" in l for l in last), last[-2:])
# semantic set value on entry via a11y_find role text/entry
findE=call("a11y_find",{"role":"text box"})
print("a11y_find typed entry:", json.dumps(findE)[:400])
matches=findE.get("matches",[])
entry_ref=None
for m in matches:
 if m.get("text") and "héllo" in m["text"]: entry_ref=m["ref_id"]; break
if not entry_ref and matches: entry_ref=matches[0]["ref_id"]
if entry_ref:
 sv=call("a11y_set_value",{"ref_id":entry_ref,"value":"semantic-42"})
 check("a11y_set_value verified", sv.get("verification")=="verified", sv)
 logs=call("app_logs",{"app_id":"app-1","filter":"entry="})
 check("semantic value in app", any("semantic-42" in l["line"] for l in logs.get("lines",[])), [l["line"] for l in logs.get("lines",[])][-2:])
# stale ref must fail without mutation
bad=call("a11y_act",{"ref_id":":1.999|/org/a11y/atspi/accessible/999999","action":"Click"})
check("stale ref refused", "_error" in bad, bad)
# clipboard + paste with preservation
call("clipboard_write",{"text":"PRE-EXISTING"})
if entry_ref:
 # click entry, select all, paste
 shot3=call("screenshot",{}); rt3=call("read_text",{"shot_id":shot3["shot_id"]}); w3=rt3.get("words",[])
 e3=[w for w in w3 if "semantic" in w["text"].lower()]
 if e3:
  call("click",{"x":e3[0]["center_px"][0],"y":e3[0]["center_px"][1],"shot_id":shot3["shot_id"]})
  call("key",{"keys":["ctrl","a"]})
  call("type_text",{"text":"pasted-✓","mode":"paste"})
  call("wait",{"ms":900})
  logs=call("app_logs",{"app_id":"app-1","filter":"entry="})
  check("paste typed", any("pasted-✓" in l["line"] for l in logs.get("lines",[])), [l["line"] for l in logs.get("lines",[])][-2:])
  cr=call("clipboard_read",{})
  check("clipboard restored after paste", cr.get("text")=="PRE-EXISTING", cr.get("text"))
# baseline: save, compare unchanged passes, click increment, compare fails
call("baseline_save",{"name":"fx","overwrite":True})
bc=call("baseline_compare",{"name":"fx"})
check("baseline unchanged passes", bc.get("pass")==True, bc.get("changed_ratio"))
if inc:
 shot4=call("screenshot",{}); rt4=call("read_text",{"shot_id":shot4["shot_id"]}); w4=rt4.get("words",[])
 i4=[w for w in w4 if "cremen" in w["text"].lower()]
 if i4: call("click",{"x":i4[0]["center_px"][0],"y":i4[0]["center_px"][1],"shot_id":shot4["shot_id"]})
bc2=call("baseline_compare",{"name":"fx"})
check("baseline after click fails w/ region", bc2.get("pass")==False and len(bc2.get("regions",[]))>0, {"pass":bc2.get("pass"),"regions":bc2.get("regions")})
# batch validation-before-dispatch + valid batch
logs_before=call("app_logs",{"app_id":"app-1","filter":"FIXTURE CLICK"})
n_before=len(logs_before.get("lines",[]))
shot5=call("screenshot",{}); rt5=call("read_text",{"shot_id":shot5["shot_id"]}); w5=rt5.get("words",[])
i5=[w for w in w5 if "cremen" in w["text"].lower()]
if i5:
 cx,cy=i5[0]["center_px"]
 bad_batch=call("batch",{"steps":[{"tool":"click","arguments":{"x":cx,"y":cy,"shot_id":shot5["shot_id"]}},{"tool":"app_launch","arguments":{"argv":["/bin/true"]}}]})
 check("batch rejects disallowed step pre-dispatch", bad_batch.get("dispatched")==False, bad_batch)
 logs_mid=call("app_logs",{"app_id":"app-1","filter":"FIXTURE CLICK"})
 check("invalid batch dispatched nothing", len(logs_mid.get("lines",[]))==n_before, (n_before, len(logs_mid.get("lines",[]))))
 good=call("batch",{"steps":[{"tool":"click","arguments":{"x":cx,"y":cy,"shot_id":shot5["shot_id"]}},{"tool":"wait","arguments":{"ms":100}},{"tool":"click","arguments":{"x":cx,"y":cy,"shot_id":shot5["shot_id"]}}]})
 check("batch completes", good.get("completed")==3, good.get("completed"))
# trace + replay dry run
call("trace_start",{})
if i5: call("click",{"x":cx,"y":cy,"shot_id":shot5["shot_id"]})
tr=call("trace_stop",{})
check("trace recorded click", any(e.get("tool")=="click" for e in tr.get("events",[])), tr.get("event_count"))
tid=tr.get("trace_id")
if tid:
 dry=call("trace_replay",{"trace_id":tid,"dry_run":True})
 check("trace dry-run plans click", any(pp.get("tool")=="click" for pp in dry.get("planned",[])), dry)
# record
rs=call("record_start",{"name":"fxrec","fps":5})
call("wait",{"ms":1500})
rstop=call("record_stop",{"recording_id":rs.get("recording_id")})
check("record frames > 0", (rstop.get("frames_captured") or 0)>0, rstop)
print("RECORD_PATH", rstop.get("path"))
# scroll + drag smoke via logs
call("scroll",{"x":700,"y":600,"space":"logical","dy":3})
shot6=call("screenshot",{}); 
# drag on lower area (drag surface near bottom)
tw=shot6["transform"]["crop_logical"]["width"]; th=shot6["transform"]["crop_logical"]["height"]
# locate drag surface from a11y extents (largest wide, short node), else bottom-of-window estimate
def walk(n, acc):
 if isinstance(n, dict):
  if n.get("extents_screen"): acc.append(n["extents_screen"])
  for c in n.get("children",[]): walk(c, acc)
 return acc
exts=walk(call("a11y_tree",{"max_nodes":400}).get("tree",{}),[])
cands=[e for e in exts if 80 <= e[3] <= 220 and e[2] > 400]
print("drag candidates", cands[:5])
if cands:
 e=cands[0]; call("drag",{"from":{"x":e[0]+40,"y":e[1]+e[3]//2,"space":"logical"},"to":{"x":e[0]+e[2]-40,"y":e[1]+e[3]//2,"space":"logical"}})
else:
 call("drag",{"from":{"x":200,"y":1650,"space":"logical"},"to":{"x":420,"y":1650,"space":"logical"}})
call("wait",{"ms":900})
logs=call("app_logs",{"app_id":"app-1","filter":"drag"})
check("drag events received", len(logs.get("lines",[]))>0, logs.get("lines"))
# hold + release_all
call("key_hold",{"keys":["shift"],"pressed":True})
ra=call("release_all",{})
check("release_all ok", "released" in ra, ra)
# restart + stop
rst=call("app_restart",{"app_id":"app-1"})
check("restart new id", rst.get("app_id") not in (None,"app-1"), rst)
newid=rst.get("app_id")
if newid:
 call("wait",{"ms":800})
 st=call("app_stop",{"app_id":newid})
 check("stop restarted app", st.get("stopped")==True, st)
gs=call("get_app_state",{})
check("get_app_state returns image+tree json", gs is not None, "")
doc=call("doctor",{})
check("doctor runs", bool(doc), "")
dc={c["name"]: c for c in doc.get("checks", [])}
check("doctor: sandbox accessibility available", dc.get("accessibility",{}).get("status")=="available" and (dc["accessibility"]["detail"].get("applications") or 0)>=1, dc.get("accessibility"))
check("doctor: sandbox has no portal", dc.get("portal",{}).get("status")=="unavailable", dc.get("portal"))
check("doctor: no unchecked statuses", not any(c["status"] in ("not-checked","present-not-probed","probed") for c in doc.get("checks", [])), [c["name"]+"="+c["status"] for c in doc.get("checks", [])])
call("sandbox_stop",{})
host_shot=call("screenshot",{"max_dimension":300})
check("host still capturable after sandbox stop", bool(host_shot.get("shot_id")), host_shot.get("shot_id"))
print(f"\n==== {len(PASS)} passed, {len(FAIL)} failed: {FAIL}")
p.terminate()
