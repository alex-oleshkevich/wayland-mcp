//! Synthetic multitouch via a configured evdev touchscreen (write-injection on the
//! operator-verified /dev/input/eventN mapping). This is a touchscreen path, not a
//! touchpad gesture, and is never used for a private/sandbox target.
use std::{fs::OpenOptions, io::Write, os::unix::fs::OpenOptionsExt};

#[repr(C)]
#[derive(Clone, Copy)]
struct InputEvent { tv_sec: i64, tv_usec: i64, kind: u16, code: u16, value: i32 }
const EV_SYN: u16 = 0; const EV_KEY: u16 = 1; const EV_ABS: u16 = 3;
const SYN_REPORT: u16 = 0;
const BTN_TOUCH: u16 = 0x14a;
const ABS_MT_SLOT: u16 = 0x2f; const ABS_MT_TRACKING_ID: u16 = 0x39;
const ABS_MT_POSITION_X: u16 = 0x35; const ABS_MT_POSITION_Y: u16 = 0x36; const ABS_MT_TOOL_TYPE: u16 = 0x37;

pub struct TouchMapping { pub device: String, pub max_x: i32, pub max_y: i32, pub output: String }
pub fn parse_mapping(cfg: &str) -> Result<TouchMapping, String> {
    // /dev/input/eventN:WIDTHxHEIGHT@OUTPUT
    let (device, rest) = cfg.split_once(':').ok_or("touch mapping must be /dev/input/eventN:WIDTHxHEIGHT@OUTPUT")?;
    let (size, output) = rest.split_once('@').ok_or("touch mapping must include @OUTPUT")?;
    let (w, h) = size.split_once('x').ok_or("touch mapping size must be WIDTHxHEIGHT")?;
    Ok(TouchMapping { device: device.into(), max_x: w.parse().map_err(|_| "bad touch mapping width")?, max_y: h.parse().map_err(|_| "bad touch mapping height")?, output: output.into() })
}

pub struct TouchDevice { file: std::fs::File, pub mapping: TouchMapping, next_tracking: i32 }
impl TouchDevice {
    pub fn open(mapping: TouchMapping) -> Result<Self, String> {
        let file = OpenOptions::new().write(true).custom_flags(libc::O_NONBLOCK).open(&mapping.device)
            .map_err(|e| format!("open configured touchscreen {}: {e} (check permissions/that this is the touchscreen, not another device)", mapping.device))?;
        Ok(Self { file, mapping, next_tracking: 1000 })
    }
    fn send(&mut self, events: &[(u16, u16, i32)]) -> Result<(), String> {
        let mut bytes = Vec::with_capacity(events.len() * 24);
        for (kind, code, value) in events {
            let ev = InputEvent { tv_sec: 0, tv_usec: 0, kind: *kind, code: *code, value: *value };
            bytes.extend_from_slice(unsafe { std::slice::from_raw_parts(&ev as *const InputEvent as *const u8, 24) });
        }
        self.file.write_all(&bytes).map_err(|e| format!("touch event write: {e}"))
    }
    /// contacts: list of (slot, x_device, y_device, down)
    pub fn frame(&mut self, contacts: &[(u32, i32, i32, bool)]) -> Result<(), String> {
        let mut events = Vec::new();
        let any_down = contacts.iter().any(|c| c.3);
        for (slot, x, y, down) in contacts {
            events.push((EV_ABS, ABS_MT_SLOT, *slot as i32));
            if *down {
                self.next_tracking += 1;
                events.push((EV_ABS, ABS_MT_TRACKING_ID, self.next_tracking));
                events.push((EV_ABS, ABS_MT_TOOL_TYPE, 0));
                events.push((EV_ABS, ABS_MT_POSITION_X, *x));
                events.push((EV_ABS, ABS_MT_POSITION_Y, *y));
            } else {
                events.push((EV_ABS, ABS_MT_TRACKING_ID, -1));
            }
        }
        events.push((EV_KEY, BTN_TOUCH, if any_down { 1 } else { 0 }));
        events.push((EV_SYN, SYN_REPORT, 0));
        self.send(&events)
    }
    pub fn move_contact(&mut self, slot: u32, x: i32, y: i32) -> Result<(), String> {
        self.send(&[(EV_ABS, ABS_MT_SLOT, slot as i32), (EV_ABS, ABS_MT_POSITION_X, x), (EV_ABS, ABS_MT_POSITION_Y, y), (EV_SYN, SYN_REPORT, 0)])
    }
}
