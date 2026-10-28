//! Persistent per-target virtual pointer/keyboard (wlr virtual-pointer + virtual-keyboard-unstable-v1).
//! Held input is owned by this device object; release_all/shutdown releases only what we pressed.
use crate::wayland::{connect, TargetSpec};
use std::{collections::{HashMap, HashSet}, os::fd::AsFd, time::{Duration, Instant}};
use wayland_client::{globals::registry_queue_init, protocol::{wl_registry, wl_seat}, Connection, Dispatch, QueueHandle};
use wayland_protocols_wlr::virtual_pointer::v1::client::{zwlr_virtual_pointer_manager_v1, zwlr_virtual_pointer_v1};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{zwp_virtual_keyboard_manager_v1, zwp_virtual_keyboard_v1};

#[derive(Default)]
struct IState;
impl Dispatch<wl_registry::WlRegistry, wayland_client::globals::GlobalListContents> for IState {
    fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event, _: &wayland_client::globals::GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}
wayland_client::delegate_noop!(IState: ignore wl_seat::WlSeat);
wayland_client::delegate_noop!(IState: ignore zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1);
wayland_client::delegate_noop!(IState: ignore zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1);
wayland_client::delegate_noop!(IState: ignore zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1);
wayland_client::delegate_noop!(IState: ignore zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1);

pub const BTN_LEFT: u32 = 0x110; pub const BTN_RIGHT: u32 = 0x111; pub const BTN_MIDDLE: u32 = 0x112;
pub const BTN_BACK: u32 = 0x113; pub const BTN_FORWARD: u32 = 0x114;
pub fn button_code(name: &str) -> Result<u32, String> {
    Ok(match name.to_ascii_lowercase().as_str() { "left" => BTN_LEFT, "right" => BTN_RIGHT, "middle" => BTN_MIDDLE, "back" | "side" => BTN_BACK, "forward" | "extra" => BTN_FORWARD, o => return Err(format!("unknown button '{o}'")) })
}

/// (evdev keycode, needs_shift) for printable US-layout characters.
pub fn char_key(c: char) -> Option<(u32, bool)> {
    let shift_table: &[(char, u32)] = &[('!',2),('@',3),('#',4),('$',5),('%',6),('^',7),('&',8),('*',9),('(',10),(')',11),('_',12),('+',13),('{',26),('}',27),('|',43),(':',39),('"',40),('<',51),('>',52),('?',53),('~',41)];
    if let Some((_, code)) = shift_table.iter().find(|(ch, _)| *ch == c) { return Some((*code, true)); }
    let plain: &[(char, u32)] = &[('1',2),('2',3),('3',4),('4',5),('5',6),('6',7),('7',8),('8',9),('9',10),('0',11),('-',12),('=',13),('[',26),(']',27),('\\',43),(';',39),('\'',40),(',',51),('.',52),('/',53),('`',41),(' ',57),('\n',28),('\t',15)];
    if let Some((_, code)) = plain.iter().find(|(ch, _)| *ch == c) { return Some((*code, false)); }
    if c.is_ascii_lowercase() {
        let letters: &[u32] = &[30,48,46,32,18,33,34,35,23,36,37,38,50,49,24,25,16,19,31,20,22,47,17,45,21,44];
        return Some((letters[(c as u32 - 'a' as u32) as usize], false));
    }
    if c.is_ascii_uppercase() {
        let letters: &[u32] = &[30,48,46,32,18,33,34,35,23,36,37,38,50,49,24,25,16,19,31,20,22,47,17,45,21,44];
        return Some((letters[(c as u32 - 'A' as u32) as usize], true));
    }
    None
}
pub fn named_key(name: &str) -> Result<(u32, bool), String> {
    let n = name.to_ascii_lowercase();
    let simple: &[(&str, u32)] = &[
        ("escape",1),("esc",1),("backspace",14),("tab",15),("return",28),("enter",28),("space",57),
        ("leftshift",42),("shift",42),("rightshift",54),("leftctrl",29),("ctrl",29),("control",29),("rightctrl",97),
        ("leftalt",56),("alt",56),("rightalt",100),("leftmeta",125),("meta",125),("super",125),("win",125),("rightmeta",126),
        ("capslock",58),("numlock",69),("scrolllock",70),("insert",110),("delete",111),("home",102),("end",107),
        ("pageup",104),("pagedown",109),("page_up",104),("page_down",109),("up",103),("down",108),("left",105),("right",106),
        ("print",99),("pause",119),("menu",139),
        ("f1",59),("f2",60),("f3",61),("f4",62),("f5",63),("f6",64),("f7",65),("f8",66),("f9",67),("f10",68),("f11",87),("f12",88),
        ("minus",12),("equal",13),("comma",51),("dot",52),("slash",53),("backslash",43),("semicolon",39),("apostrophe",40),("grave",41),
        ("leftbrace",26),("rightbrace",27),("kp0",82),("kp1",79),("kp2",80),("kp3",81),("kp4",75),("kp5",76),("kp6",77),("kp7",71),("kp8",72),("kp9",73),
    ];
    if let Some((_, code)) = simple.iter().find(|(k, _)| *k == n) { return Ok((*code, false)); }
    if n.chars().count() == 1 { if let Some(v) = char_key(n.chars().next().unwrap()) { return Ok(v); } }
    Err(format!("unknown key name '{name}'; validate chords before dispatch"))
}
pub fn modifier_mask(name: &str) -> Option<u32> {
    // xkb modifier masks in the default evdev/us keymap: Shift=1, Lock=2, Control=4, Mod1(Alt)=8, Mod2=16, Mod3=32, Mod4(Super)=64, Mod5=128
    match name.to_ascii_lowercase().as_str() { "shift" | "leftshift" | "rightshift" => Some(1), "ctrl" | "control" | "leftctrl" | "rightctrl" => Some(4), "alt" | "leftalt" | "rightalt" => Some(8), "super" | "meta" | "win" | "leftmeta" | "rightmeta" => Some(64), "capslock" => Some(2), _ => None }
}

/// Build the virtual-keyboard keymap. `extra_chars` are Unicode characters not in
/// the US layout; they are assigned onto existing spare keys (F13-F24 key names,
/// whose keycodes already exist in the evdev keycodes table — inventing new key
/// names is ignored by xkbcomp when the code already has a name). Returns the
/// keymap text and (char -> evdev keycode) assignments, verified by introspection.
fn keymap_string(extra_chars: &[char]) -> Result<(String, Vec<(char, u32)>), String> {
    let mut ctx = xkbcommon::xkb::Context::new(xkbcommon::xkb::CONTEXT_NO_FLAGS);
    let km = xkbcommon::xkb::Keymap::new_from_names(&mut ctx, "evdev", "", "us", "", None, xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS)
        .ok_or("failed to compile evdev/us keymap via libxkbcommon")?;
    extend_keymap(km.get_as_string(xkbcommon::xkb::KEYMAP_FORMAT_TEXT_V1), extra_chars)
}

/// Assign `extra_chars` to the F13-F24 spare keys of keymap text `s`.
fn extend_keymap(mut s: String, extra_chars: &[char]) -> Result<(String, Vec<(char, u32)>), String> {
    let mut assigned = Vec::new();
    if !extra_chars.is_empty() {
        // Parse existing key name -> xkb keycode from the keycodes section.
        let mut codes: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
        for line in s.lines() {
            let line = line.trim();
            if line.starts_with('<') {
                if let (Some(end), Some(eq)) = (line.find('>'), line.find('=')) {
                    if let Ok(code) = line[eq + 1..].trim().trim_end_matches(';').trim().parse::<u32>() {
                        codes.insert(line[1..end].to_string(), code);
                    }
                }
            }
        }
        let candidates: Vec<String> = (13..=24).map(|n| format!("FK{n}")).filter(|n| codes.contains_key(n)).collect();
        for (i, ch) in extra_chars.iter().enumerate() {
            let name = candidates.get(i).ok_or("unicode spare keys exhausted (F13-F24 all assigned)")?;
            let code = codes[name];
            let needle = format!("key <{name}>");
            let Some(pos) = s.find(&needle) else {
                // Some xkeyboard-config versions (e.g. Ubuntu's) map no symbols to
                // this spare key: add a fresh one-level entry to xkb_symbols.
                let sym = s.find("\nxkb_symbols").ok_or("keymap has no xkb_symbols section")?;
                let end = s[sym..].find("\n};").map(|e| sym + e + 1).ok_or("xkb_symbols section malformed")?;
                s.insert_str(end, &format!("\tkey <{name}> {{ [ U{:04X} ] }};\n", *ch as u32));
                assigned.push((*ch, code - 8));
                continue;
            };
            // FK23/FK24 carry a two-level `type=` in the stock keymap; a lone
            // keysym under that type does not compile, so force ONE_LEVEL.
            let block_end = s[pos..].find("};").map(|e| pos + e).ok_or("spare key block malformed")?;
            if let Some(t) = s[pos..block_end].find("type=").map(|t| pos + t) {
                let q1 = s[t..block_end].find('"').map(|q| t + q).ok_or("spare key type malformed")?;
                let q2 = s[q1 + 1..block_end].find('"').map(|q| q1 + 1 + q).ok_or("spare key type malformed")?;
                s.replace_range(q1 + 1..q2, "ONE_LEVEL");
            }
            // `symbols[1]= [ ... ]` form: skip the group index, take the level list.
            let list_from = s[pos..block_end].find("symbols[").and_then(|sy| s[pos + sy..block_end].find('=').map(|e| pos + sy + e)).unwrap_or(pos);
            let open = s[list_from..].find('[').map(|o| list_from + o).ok_or("spare key symbols malformed")?;
            let close = s[open..].find(']').map(|c| open + c).ok_or("spare key symbols malformed")?;
            s.replace_range(open + 1..close, &format!(" U{:04X} ", *ch as u32));
            assigned.push((*ch, code - 8));
        }
        // Validate compile AND that each assignment actually took effect.
        let mut ctx2 = xkbcommon::xkb::Context::new(xkbcommon::xkb::CONTEXT_NO_FLAGS);
        let km2 = xkbcommon::xkb::Keymap::new_from_string(&mut ctx2, s.clone(), xkbcommon::xkb::KEYMAP_FORMAT_TEXT_V1, xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS)
            .ok_or("extended unicode keymap failed to compile")?;
        for (ch, evdev) in &assigned {
            let syms = km2.key_get_syms_by_level(xkbcommon::xkb::Keycode::from(evdev + 8), 0, 0);
            let want = *ch as u32;
            let ok = syms.iter().any(|s| { let r = s.raw(); r == want || r == 0x1000000 + want });
            if !ok { return Err(format!("unicode key assignment for {ch:?} did not take effect in the compiled keymap")); }
        }
        if std::env::var("WAYLAND_MCP_DEBUG_KEYMAP").is_ok() { let _ = std::fs::write("/tmp/extended-keymap.txt", &s); }
    }
    Ok((s, assigned))
}

/// Settle after the post-typing roundtrip: time for the focused client's
/// event loop to apply the keys the compositor has already delivered.
const TYPE_SETTLE_MS: u64 = 50;

pub struct NativeInput {
    pub target_id: String,
    queue: wayland_client::EventQueue<IState>,
    state: IState,
    pointer: Option<zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1>,
    keyboard: Option<zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1>,
    start: Instant,
    pub held_buttons: HashSet<u32>,
    pub held_keys: HashMap<String, u32>, // name -> evdev
    pub held_modifiers_mask: u32,
    unicode_keys: HashMap<char, u32>, // char -> evdev keycode (spare keys)
    unicode_lru: Vec<char>, // unicode_keys chars, least- to most-recently used
    held_key_codes: HashSet<u32>, // every evdev code currently pressed (incl. anonymous key_event_pub)
    pub pointer_logical: (f64, f64),
    pub route: String,
}
impl NativeInput {
    pub fn new(spec: &TargetSpec) -> Result<Self, String> {
        let conn = connect(spec)?;
        let (globals, queue) = registry_queue_init::<IState>(&conn).map_err(|e| format!("input registry: {e}"))?;
        let qh = queue.handle();
        let seat: Option<wl_seat::WlSeat> = globals.bind(&qh, 1..=1, ()).ok();
        let pointer = globals.bind::<zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1, _, _>(&qh, 1..=2, ()).ok()
            .map(|mgr| mgr.create_virtual_pointer(seat.as_ref(), &qh, ()));
        let keyboard_mgr = globals.bind::<zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1, _, _>(&qh, 1..=1, ()).ok();
        let mut dev = Self { target_id: spec.id.clone(), queue, state: IState, pointer, keyboard: None, start: Instant::now(),
            held_buttons: HashSet::new(), held_keys: HashMap::new(), held_modifiers_mask: 0, unicode_keys: HashMap::new(), unicode_lru: Vec::new(), held_key_codes: HashSet::new(), pointer_logical: (0.0, 0.0), route: String::new() };
        if let (Some(mgr), Some(seat)) = (keyboard_mgr, seat.as_ref()) {
            let kb = mgr.create_virtual_keyboard(seat, &qh, ());
            dev.keyboard = Some(kb);
            dev.upload_keymap(&[])?;
        }
        let mut routes = Vec::new();
        if dev.pointer.is_some() { routes.push("zwlr_virtual_pointer_manager_v1"); }
        if dev.keyboard.is_some() { routes.push("zwp_virtual_keyboard_manager_v1"); }
        if routes.is_empty() { return Err("no native virtual pointer/keyboard globals on this target; portal input requires an explicitly consented session (see portal path), uinput requires explicit verified mapping".into()); }
        dev.route = routes.join("+");
        Ok(dev)
    }
    fn time(&self) -> u32 { self.start.elapsed().as_millis() as u32 }
    fn flush(&mut self) -> Result<(), String> { self.queue.flush().map_err(|e| format!("wayland flush: {e}"))?; self.queue.dispatch_pending(&mut self.state).map_err(|e| format!("wayland dispatch: {e}"))?; Ok(()) }
    /// Block until the compositor has processed every request sent so far
    /// (wl_display.sync), i.e. forwarded all injected input to the focused client.
    fn roundtrip(&mut self) -> Result<(), String> { self.queue.roundtrip(&mut self.state).map(|_| ()).map_err(|e| format!("wayland roundtrip: {e}")) }
    pub fn key_event_pub(&mut self, evdev: u32, pressed: bool) -> Result<(), String> { self.key_event(evdev, pressed, 0) }
    fn upload_keymap(&mut self, extra: &[char]) -> Result<(), String> {
        let (text, _) = keymap_string(extra)?;
        let bytes = text.as_bytes();
        let fd = unsafe { libc::memfd_create(b"wayland-mcp-keymap\0".as_ptr() as *const libc::c_char, libc::MFD_CLOEXEC) };
        if fd < 0 { return Err("memfd_create keymap failed".into()); }
        use std::{io::Write, os::fd::FromRawFd};
        let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
        file.write_all(bytes).map_err(|e| format!("write keymap: {e}"))?;
        if let Some(kb) = &self.keyboard { kb.keymap(1, file.as_fd(), bytes.len() as u32); }
        drop(file);
        self.flush()
    }
    /// Ensure every char is typeable; extends the keymap only when no keys are held.
    pub fn ensure_chars(&mut self, text: &str) -> Result<(), String> {
        // Mark chars already assigned and used by this text as most-recently-used.
        for c in text.chars() {
            if self.unicode_keys.contains_key(&c) {
                self.unicode_lru.retain(|x| *x != c);
                self.unicode_lru.push(c);
            }
        }
        let mut missing: Vec<char> = Vec::new();
        for c in text.chars() {
            if char_key(c).is_none() && !self.unicode_keys.contains_key(&c) && !missing.contains(&c) { missing.push(c); }
        }
        if missing.is_empty() { return Ok(()); }
        if !self.held_keys.is_empty() || !self.held_key_codes.is_empty() { return Err("cannot extend keyboard keymap while keys are held".into()); }
        if self.keyboard.is_none() { return Err("virtual keyboard unavailable for unicode text".into()); }
        // Spare slots are F13-F24 (12). A single text needing more than that is an
        // error; otherwise evict least-recently-used assignments to make room —
        // no keys are held here, so evicted chars can be re-assigned on demand.
        const SPARE: usize = 12;
        // Chars of this text that are already assigned must stay assigned.
        let mut in_text: Vec<char> = Vec::new();
        for c in text.chars() { if self.unicode_keys.contains_key(&c) && !in_text.contains(&c) { in_text.push(c); } }
        let needed = in_text.len() + missing.len();
        if needed > SPARE { return Err(format!("unicode spare keys exhausted: this text alone needs {needed} distinct non-US characters (max {SPARE})")); }
        // Keymap-commit nudge (before upload): if the virtual keyboard has not
        // yet delivered any key event, a keymap re-upload can leave subsequent
        // keys undelivered (observed on smithay). A real but inert modifier
        // press/release (Shift, no text effect) on the current map first
        // establishes delivery; the upload then takes effect for the text.
        let _ = self.key_event_pub(42, true);
        let _ = self.key_event_pub(42, false);
        std::thread::sleep(std::time::Duration::from_millis(120));
        // Retain this text's existing assignments, then the most-recently-used
        // others that still fit; evict the rest.
        let keep = SPARE - needed;
        let mut others: Vec<char> = self.unicode_lru.iter().rev().filter(|c| !in_text.contains(c)).take(keep).copied().collect();
        others.reverse();
        let mut retained: Vec<char> = others;
        retained.extend(in_text.iter().copied());
        let mut chars = retained.clone();
        for c in &missing { chars.push(*c); }
        let (text_map, assigned) = keymap_string(&chars)?;
        let bytes = text_map.as_bytes();
        let fd = unsafe { libc::memfd_create(b"wayland-mcp-keymap\0".as_ptr() as *const libc::c_char, libc::MFD_CLOEXEC) };
        if fd < 0 { return Err("memfd_create keymap failed".into()); }
        use std::{io::Write, os::fd::FromRawFd};
        let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
        file.write_all(bytes).map_err(|e| e.to_string())?;
        self.keyboard.as_ref().unwrap().keymap(1, file.as_fd(), bytes.len() as u32);
        drop(file);
        self.flush()?;
        // Short settle so the compositor can propagate the new map to the
        // focused client before the first character is sent.
        std::thread::sleep(std::time::Duration::from_millis(150));
        self.unicode_keys.clear();
        for (c, ev) in assigned { self.unicode_keys.insert(c, ev); }
        self.unicode_lru = retained;
        for c in &missing { self.unicode_lru.push(*c); }
        Ok(())
    }
    fn require_pointer(&self) -> Result<&zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1, String> { self.pointer.as_ref().ok_or_else(|| "virtual pointer unavailable on this target".to_string()) }
    fn require_keyboard(&self) -> Result<&zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1, String> { self.keyboard.as_ref().ok_or_else(|| "virtual keyboard unavailable on this target".to_string()) }

    pub fn move_absolute(&mut self, lx: f64, ly: f64, extent_w: u32, extent_h: u32) -> Result<(), String> {
        let p = self.require_pointer()?.clone();
        p.motion_absolute(self.time(), lx.max(0.0) as u32, ly.max(0.0) as u32, extent_w.max(1), extent_h.max(1));
        p.frame();
        self.pointer_logical = (lx, ly);
        self.flush()
    }
    pub fn button(&mut self, code: u32, pressed: bool) -> Result<(), String> {
        let p = self.require_pointer()?.clone();
        p.button(self.time(), code, if pressed { wayland_client::protocol::wl_pointer::ButtonState::Pressed } else { wayland_client::protocol::wl_pointer::ButtonState::Released });
        p.frame();
        if pressed { self.held_buttons.insert(code); } else { self.held_buttons.remove(&code); }
        self.flush()
    }
    pub fn scroll(&mut self, dx_steps: f64, dy_steps: f64, smooth: bool) -> Result<String, String> {
        let p = self.require_pointer()?.clone();
        p.axis_source(wayland_client::protocol::wl_pointer::AxisSource::Wheel);
        if dy_steps != 0.0 { p.axis(self.time(), wayland_client::protocol::wl_pointer::Axis::VerticalScroll, dy_steps * 15.0); if !smooth { p.axis_discrete(self.time(), wayland_client::protocol::wl_pointer::Axis::VerticalScroll, dy_steps * 15.0, dy_steps as i32); } }
        if dx_steps != 0.0 { p.axis(self.time(), wayland_client::protocol::wl_pointer::Axis::HorizontalScroll, dx_steps * 15.0); if !smooth { p.axis_discrete(self.time(), wayland_client::protocol::wl_pointer::Axis::HorizontalScroll, dx_steps * 15.0, dx_steps as i32); } }
        p.frame(); self.flush()?;
        Ok(if smooth { "smooth".into() } else { "wheel".into() })
    }
    fn send_modifiers(&mut self) -> Result<(), String> {
        let kb = self.require_keyboard()?.clone();
        kb.modifiers(self.held_modifiers_mask, 0, 0, 0);
        Ok(())
    }
    pub fn key_event(&mut self, evdev: u32, pressed: bool, temporary_modifiers: u32) -> Result<(), String> {
        let kb = self.require_keyboard()?.clone();
        if temporary_modifiers != 0 {
            kb.modifiers(self.held_modifiers_mask | temporary_modifiers, 0, 0, 0);
        }
        kb.key(self.time(), evdev, if pressed { 1 } else { 0 });
        if pressed { self.held_key_codes.insert(evdev); } else { self.held_key_codes.remove(&evdev); }
        if temporary_modifiers != 0 { kb.modifiers(self.held_modifiers_mask, 0, 0, 0); }
        self.flush()
    }
    /// Press+release a named key/chord element, validating first (caller validates the whole chord).
    pub fn tap_key(&mut self, name: &str) -> Result<(), String> {
        let (code, shift) = named_key(name)?;
        self.key_event(code, true, if shift { 1 } else { 0 })?;
        std::thread::sleep(Duration::from_millis(12));
        self.key_event(code, false, if shift { 1 } else { 0 })
    }
    pub fn hold_key(&mut self, name: &str) -> Result<(), String> {
        let (code, _shift) = named_key(name)?;
        if let Some(mask) = modifier_mask(name) { self.held_modifiers_mask |= mask; self.send_modifiers()?; }
        self.key_event(code, true, 0)?;
        self.held_keys.insert(name.to_ascii_lowercase(), code);
        Ok(())
    }
    pub fn release_key(&mut self, name: &str) -> Result<(), String> {
        let code = self.held_keys.remove(&name.to_ascii_lowercase()).ok_or_else(|| format!("key '{name}' is not held by this server"))?;
        self.key_event(code, false, 0)?;
        if let Some(mask) = modifier_mask(name) { self.held_modifiers_mask &= !mask; self.send_modifiers()?; self.flush()?; }
        Ok(())
    }
    pub fn type_chars(&mut self, text: &str) -> Result<usize, String> {
        self.ensure_chars(text)?;
        let mut dispatched = 0usize;
        for c in text.chars() {
            if let Some((code, shift)) = char_key(c) {
                self.key_event(code, true, if shift { 1 } else { 0 })?;
                self.key_event(code, false, if shift { 1 } else { 0 })?;
            } else if let Some(code) = self.unicode_keys.get(&c).copied() {
                self.key_event(code, true, 0)?;
                self.key_event(code, false, 0)?;
            } else { return Err(format!("no key mapping for character {c:?} after dispatching {dispatched} chars; NOT replaying")); }
            dispatched += 1;
            std::thread::sleep(Duration::from_millis(4));
        }
        // Return only after the compositor has handled every key (and sent it
        // to the focused client), plus a short settle for the client's own
        // event loop. Wayland offers no ack of client-side processing, so a
        // readback should still wait for the expected state.
        self.roundtrip()?;
        std::thread::sleep(Duration::from_millis(TYPE_SETTLE_MS));
        Ok(dispatched)
    }
    pub fn release_all(&mut self) -> Result<serde_json::Value, String> {
        let mut released = serde_json::json!({"buttons": [], "keys": []});
        let buttons: Vec<u32> = self.held_buttons.iter().copied().collect();
        for b in buttons { let _ = self.button(b, false); released["buttons"].as_array_mut().unwrap().push(b.into()); }
        let keys: Vec<String> = self.held_keys.keys().cloned().collect();
        for k in keys { let _ = self.release_key(&k); released["keys"].as_array_mut().unwrap().push(k.into()); }
        // Any code pressed anonymously (key_event_pub chords) that no named key owns.
        let codes: Vec<u32> = self.held_key_codes.iter().copied().collect();
        for c in codes { let _ = self.key_event(c, false, 0); released["keys"].as_array_mut().unwrap().push(c.into()); }
        if self.held_modifiers_mask != 0 { self.held_modifiers_mask = 0; if self.keyboard.is_some() { let _ = self.send_modifiers(); let _ = self.flush(); } }
        Ok(released)
    }
}

/// Portal-backed input (RemoteDesktop notify calls) for targets without native
/// virtual pointer/keyboard (GNOME/KDE). Coordinates are converted from the
/// server's layout-origin space back to global logical, then to stream-local.
pub struct PortalDevice {
    pub handle: crate::portal_session::PortalHandle,
    spec: TargetSpec,
    origin: (i32, i32),
    held_buttons: HashSet<u32>,
    held_keys: HashMap<String, u32>,
    held_key_codes: HashSet<u32>,
    route: String,
}
impl PortalDevice {
    pub fn new(spec: &TargetSpec, handle: crate::portal_session::PortalHandle) -> Result<Self, String> {
        let disc = crate::wayland::discover(spec)?;
        let origin = (disc.outputs.iter().map(|o| o.logical.x).min().unwrap_or(0), disc.outputs.iter().map(|o| o.logical.y).min().unwrap_or(0));
        let route = format!("portal:RemoteDesktop devices={}", handle.devices);
        Ok(Self { handle, spec: spec.clone(), origin, held_buttons: HashSet::new(), held_keys: HashMap::new(), held_key_codes: HashSet::new(), route })
    }
    /// Layout origin recomputed per call: outputs can move after construction,
    /// and the server passes coordinates relative to a freshly computed origin.
    fn current_origin(&mut self) -> Result<(i32, i32), String> {
        let disc = crate::wayland::discover(&self.spec)?;
        self.origin = (disc.outputs.iter().map(|o| o.logical.x).min().unwrap_or(0), disc.outputs.iter().map(|o| o.logical.y).min().unwrap_or(0));
        Ok(self.origin)
    }
    fn stream_at(&self, gx: f64, gy: f64) -> Result<(u32, f64, f64), String> {
        for s in &self.handle.streams {
            if s.size.0 <= 0 || s.size.1 <= 0 { continue; }
            let (sx, sy) = (s.position.0 as f64, s.position.1 as f64);
            let (sw, sh) = (s.size.0 as f64, s.size.1 as f64);
            if gx >= sx && gy >= sy && gx < sx + sw && gy < sy + sh { return Ok((s.node_id, gx - sx, gy - sy)); }
        }
        Err(format!("point ({gx},{gy}) is not on any shared portal stream; available streams: {:?}", self.handle.streams.iter().map(|s| (s.node_id, s.position, s.size)).collect::<Vec<_>>()))
    }
    pub fn move_absolute(&mut self, lx: f64, ly: f64, _extent_w: u32, _extent_h: u32) -> Result<(), String> {
        let origin = self.current_origin()?;
        let (gx, gy) = (lx + origin.0 as f64, ly + origin.1 as f64);
        let (node, x, y) = self.stream_at(gx, gy)?;
        self.handle.notify(crate::portal_session::Notify::MotionAbsolute { stream: node, x, y })
    }
    pub fn button(&mut self, code: u32, pressed: bool) -> Result<(), String> {
        self.handle.notify(crate::portal_session::Notify::Button { button: code, pressed })?;
        if pressed { self.held_buttons.insert(code); } else { self.held_buttons.remove(&code); }
        Ok(())
    }
    pub fn scroll(&mut self, dx_steps: f64, dy_steps: f64, smooth: bool) -> Result<String, String> {
        for (vertical, steps) in [(true, dy_steps), (false, dx_steps)] {
            if steps == 0.0 { continue; }
            if smooth || steps.fract() != 0.0 {
                self.handle.notify(crate::portal_session::Notify::AxisSmooth { vertical, value: steps * 15.0 })?;
            } else {
                self.handle.notify(crate::portal_session::Notify::AxisDiscrete { vertical, steps: steps as i32 })?;
            }
        }
        Ok(if smooth { "smooth".into() } else { "wheel".into() })
    }
    pub fn key_event(&mut self, evdev: u32, pressed: bool, temporary_modifiers: u32) -> Result<(), String> {
        if temporary_modifiers != 0 {
            self.handle.notify(crate::portal_session::Notify::Keycode { code: 42, pressed: true })?;
            let r = self.handle.notify(crate::portal_session::Notify::Keycode { code: evdev, pressed });
            let _ = self.handle.notify(crate::portal_session::Notify::Keycode { code: 42, pressed: false });
            if r.is_ok() { if pressed { self.held_key_codes.insert(evdev); } else { self.held_key_codes.remove(&evdev); } }
            return r;
        }
        self.handle.notify(crate::portal_session::Notify::Keycode { code: evdev, pressed })?;
        if pressed { self.held_key_codes.insert(evdev); } else { self.held_key_codes.remove(&evdev); }
        Ok(())
    }
    pub fn key_event_pub(&mut self, evdev: u32, pressed: bool) -> Result<(), String> { self.key_event(evdev, pressed, 0) }
    pub fn hold_key(&mut self, name: &str) -> Result<(), String> {
        let (code, _) = named_key(name)?;
        self.handle.notify(crate::portal_session::Notify::Keycode { code, pressed: true })?;
        self.held_keys.insert(name.to_ascii_lowercase(), code);
        self.held_key_codes.insert(code);
        Ok(())
    }
    pub fn release_key(&mut self, name: &str) -> Result<(), String> {
        let code = self.held_keys.remove(&name.to_ascii_lowercase()).ok_or_else(|| format!("key '{name}' is not held by this server"))?;
        let r = self.handle.notify(crate::portal_session::Notify::Keycode { code, pressed: false });
        if r.is_ok() { self.held_key_codes.remove(&code); }
        r
    }
    pub fn type_chars(&mut self, text: &str) -> Result<usize, String> {
        let unsupported: Vec<char> = text.chars().filter(|c| char_key(*c).is_none()).collect();
        if !unsupported.is_empty() {
            return Err(format!("portal keyboard cannot type these characters on the active (US) keymap: {unsupported:?}; no characters were dispatched — use clipboard_write followed by key ctrl+v, or a native-input target"));
        }
        let mut dispatched = 0usize;
        for c in text.chars() {
            if let Some((code, shift)) = char_key(c) {
                self.key_event(code, true, if shift { 1 } else { 0 })?;
                self.key_event(code, false, if shift { 1 } else { 0 })?;
            }
            dispatched += 1;
            std::thread::sleep(std::time::Duration::from_millis(4));
        }
        Ok(dispatched)
    }
    pub fn release_all(&mut self) -> Result<serde_json::Value, String> {
        let mut released = serde_json::json!({"buttons": [], "keys": []});
        let buttons: Vec<u32> = self.held_buttons.iter().copied().collect();
        for b in buttons { let _ = self.button(b, false); released["buttons"].as_array_mut().unwrap().push(b.into()); }
        let keys: Vec<String> = self.held_keys.keys().cloned().collect();
        for k in keys { let _ = self.release_key(&k); released["keys"].as_array_mut().unwrap().push(k.into()); }
        let codes: Vec<u32> = self.held_key_codes.iter().copied().collect();
        for c in codes { let _ = self.key_event(c, false, 0); released["keys"].as_array_mut().unwrap().push(c.into()); }
        Ok(released)
    }
}

/// The single input device abstraction used by all tools: native virtual
/// pointer/keyboard where the compositor offers it, otherwise the consented
/// portal session. There is no third silent fallback.
pub enum InputDevice {
    Native(Box<NativeInput>),
    Portal(PortalDevice),
}
impl InputDevice {
    /// `portal` yields the target's shared consented session (one per target,
    /// shared with capture) and is only invoked when native input is unavailable.
    pub fn new(spec: &TargetSpec, portal: impl FnOnce() -> Result<crate::portal_session::PortalHandle, String>) -> Result<Self, String> {
        match NativeInput::new(spec) {
            Ok(n) => Ok(Self::Native(Box::new(n))),
            Err(native_err) => {
                if spec.kind == "sandbox" {
                    return Err(format!("{native_err} (private targets never use the host portal session)"));
                }
                match portal().and_then(|h| PortalDevice::new(spec, h)) {
                    Ok(p) => Ok(Self::Portal(p)),
                    Err(portal_err) => Err(format!("native input unavailable: {native_err}; portal input unavailable: {portal_err}")),
                }
            }
        }
    }
    /// Some(reason) when this device is backed by a portal session that has ended.
    pub fn portal_dead_reason(&self) -> Option<String> {
        match self { Self::Portal(p) => p.handle.dead_reason(), Self::Native(_) => None }
    }
    pub fn route(&self) -> String {
        match self { Self::Native(n) => n.route.clone(), Self::Portal(p) => p.route.clone() }
    }
    pub fn move_absolute(&mut self, lx: f64, ly: f64, w: u32, h: u32) -> Result<(), String> {
        match self { Self::Native(n) => n.move_absolute(lx, ly, w, h), Self::Portal(p) => p.move_absolute(lx, ly, w, h) }
    }
    pub fn button(&mut self, code: u32, pressed: bool) -> Result<(), String> {
        match self { Self::Native(n) => n.button(code, pressed), Self::Portal(p) => p.button(code, pressed) }
    }
    pub fn scroll(&mut self, dx: f64, dy: f64, smooth: bool) -> Result<String, String> {
        match self { Self::Native(n) => n.scroll(dx, dy, smooth), Self::Portal(p) => p.scroll(dx, dy, smooth) }
    }
    pub fn key_event(&mut self, evdev: u32, pressed: bool, temporary_modifiers: u32) -> Result<(), String> {
        match self { Self::Native(n) => n.key_event(evdev, pressed, temporary_modifiers), Self::Portal(p) => p.key_event(evdev, pressed, temporary_modifiers) }
    }
    pub fn key_event_pub(&mut self, evdev: u32, pressed: bool) -> Result<(), String> {
        match self { Self::Native(n) => n.key_event_pub(evdev, pressed), Self::Portal(p) => p.key_event_pub(evdev, pressed) }
    }
    pub fn hold_key(&mut self, name: &str) -> Result<(), String> {
        match self { Self::Native(n) => n.hold_key(name), Self::Portal(p) => p.hold_key(name) }
    }
    pub fn release_key(&mut self, name: &str) -> Result<(), String> {
        match self { Self::Native(n) => n.release_key(name), Self::Portal(p) => p.release_key(name) }
    }
    pub fn type_chars(&mut self, text: &str) -> Result<usize, String> {
        match self { Self::Native(n) => n.type_chars(text), Self::Portal(p) => p.type_chars(text) }
    }
    pub fn release_all(&mut self) -> Result<serde_json::Value, String> {
        match self { Self::Native(n) => n.release_all(), Self::Portal(p) => p.release_all() }
    }
}
#[cfg(test)]
mod spare_key_tests {
    #[test] fn all_twelve_spare_keys_compile() {
        let chars: Vec<char> = "ñ¿¡áíóúçàèéê".chars().collect();
        let (_, assigned) = super::keymap_string(&chars).expect("12 spare keys must compile");
        assert_eq!(assigned.len(), 12);
    }
    /// Ubuntu's xkeyboard-config has no symbols entry for some spare keys; the
    /// assignment must add one rather than fail.
    #[test] fn spare_keys_without_symbols_entries_are_added() {
        let mut ctx = xkbcommon::xkb::Context::new(xkbcommon::xkb::CONTEXT_NO_FLAGS);
        let km = xkbcommon::xkb::Keymap::new_from_names(&mut ctx, "evdev", "", "us", "", None, xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS).unwrap();
        let full = km.get_as_string(xkbcommon::xkb::KEYMAP_FORMAT_TEXT_V1);
        let stripped: String = full.lines().filter(|l| !l.contains("key <FK19>") && !l.contains("key <FK20>")).map(|l| format!("{l}\n")).collect();
        assert!(!stripped.contains("key <FK19>"));
        let chars: Vec<char> = "ñ¿¡áíóúçàèéê".chars().collect();
        let (_, assigned) = super::extend_keymap(stripped, &chars).expect("missing spare-key entries must be added");
        assert_eq!(assigned.len(), 12);
    }
}
