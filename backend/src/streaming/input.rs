// src/streaming/input.rs
//
// Virtual input device injector for DroidKer containers.
//
// When a container starts, the daemon opens `/dev/uinput` on the host and
// creates a virtual touchscreen + keypad with the following capabilities:
//
//   EV_ABS (type 3):
//     ABS_MT_SLOT           (code 47)  — multitouch slot index
//     ABS_MT_TRACKING_ID    (code 57)  — touch finger ID (-1 = lifted)
//     ABS_MT_POSITION_X     (code 53)  — X in [0, width-1]
//     ABS_MT_POSITION_Y     (code 54)  — Y in [0, height-1]
//     ABS_MT_PRESSURE       (code 58)  — pressure [0, 255]
//   EV_KEY (type 1):
//     BTN_TOUCH             (code 330) — touch down/up
//     KEY_HOMEPAGE          (code 172) — Home button
//     KEY_BACK              (code 158) — Back button
//     KEY_APPSELECT         (code 374) — Recents button
//   EV_SYN (type 0):
//     SYN_REPORT            (code 0)   — flush frame
//
// The kernel then creates `/dev/input/eventN` which the daemon bind-mounts
// into the container's /dev/input/. Android's EventHub auto-detects it on
// boot and registers it as an `InputDevice` of type "touchscreen".
//
// All input injection is done by writing `input_event` structs to the
// uinput fd:
//
//   struct input_event {
//       struct timeval time;  // 16 bytes on x86_64 and aarch64
//       __u16 type;
//       __u16 code;
//       __s32 value;
//   };
//
// On a 1-vCPU VPS we don't pool uinput fds or batch across containers —
// each container gets exactly one injector that lives for its lifetime.

use crate::error::{DroidkerError, Result};
use serde::{Deserialize, Serialize};
use std::ffi::CString;
use std::fs::File;
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd};
use uuid::Uuid;

// ---- Linux input-event-codes.h constants ----------------------------------

pub const EV_SYN: u16 = 0x00;
pub const EV_KEY: u16 = 0x01;
pub const EV_ABS: u16 = 0x03;

pub const SYN_REPORT: u16 = 0x00;

pub const ABS_MT_SLOT: u16 = 0x2F;
pub const ABS_MT_POSITION_X: u16 = 0x35;
pub const ABS_MT_POSITION_Y: u16 = 0x36;
pub const ABS_MT_TRACKING_ID: u16 = 0x39;
pub const ABS_MT_PRESSURE: u16 = 0x3A;

pub const BTN_TOUCH: u16 = 0x14A;
pub const KEY_BACK: u16 = 0x9E;
pub const KEY_HOMEPAGE: u16 = 0xAC;
pub const KEY_APPSELECT: u16 = 0x176; // Recents

// ---- /dev/uinput ioctl numbers (computed from <asm-generic/ioctl.h>) ------

const UINPUT_IOCTL_BASE: u8 = b'U';

// _IOC(dir, type, nr, size) = (dir<<30) | (size<<16) | (type<<8) | nr
const fn _ioc(dir: u32, magic: u8, nr: u8, size: u32) -> u64 {
    ((dir as u64) << 30)
        | ((size as u64 & 0x3FFF) << 16)
        | ((magic as u64) << 8)
        | (nr as u64)
}

// _IO(type, nr) = _IOC(0, ...)
const fn _io(magic: u8, nr: u8) -> u64 {
    _ioc(0, magic, nr, 0)
}
// _IOW(type, nr, size) = _IOC(1, ...)
const fn _iow(magic: u8, nr: u8, size: u32) -> u64 {
    _ioc(1, magic, nr, size)
}

// Struct sizes used by the ioctls.
const SIZE_OF_INT: u32 = 4;
// struct uinput_setup { input_id (8B) + name[80] (80B) + ff_effects_max (4B) } = 92 bytes.
// sizeof in C is 92 — no padding because name[80] already provides 4-byte alignment for the trailing u32.
const SIZE_OF_UINPUT_SETUP: u32 = 92;
// struct uinput_abs_setup { u16 code + u16 pad + 4*i32 } = 24 bytes.
const SIZE_OF_UINPUT_ABS_SETUP: u32 = 24;

// UI_DEV_CREATE  = _IO('U', 1)
const UI_DEV_CREATE: u64 = _io(UINPUT_IOCTL_BASE, 1);
// UI_DEV_DESTROY = _IO('U', 2)
const UI_DEV_DESTROY: u64 = _io(UINPUT_IOCTL_BASE, 2);
// UI_DEV_SETUP   = _IOW('U', 3, struct uinput_setup)
const UI_DEV_SETUP: u64 = _iow(UINPUT_IOCTL_BASE, 3, SIZE_OF_UINPUT_SETUP);
// UI_SET_EVBIT   = _IOW('U', 1, int)
const UI_SET_EVBIT: u64 = _iow(UINPUT_IOCTL_BASE, 1, SIZE_OF_INT);
// UI_SET_KEYBIT  = _IOW('U', 21, int)
const UI_SET_KEYBIT: u64 = _iow(UINPUT_IOCTL_BASE, 21, SIZE_OF_INT);
// UI_SET_ABSBIT  = _IOW('U', 34, int)
const UI_SET_ABSBIT: u64 = _iow(UINPUT_IOCTL_BASE, 34, SIZE_OF_INT);
// UI_ABS_SETUP   = _IOW('U', 54, struct uinput_abs_setup)
const UI_ABS_SETUP: u64 = _iow(UINPUT_IOCTL_BASE, 54, SIZE_OF_UINPUT_ABS_SETUP);

// uinput_setup as defined in <linux/uinput.h>. The kernel expects exactly
// 96 bytes here — the repr(C) struct below is 92 bytes naturally and the
// compiler pads it to 96 because ff_effects_max is u32 (4-byte aligned).
//
// We don't `#[derive(Default)]` because `[u8; 80]` doesn't impl Default
// on stable Rust (only arrays up to length 32 do). We provide our own
// `Default` impl below.
#[repr(C)]
#[derive(Clone, Copy)]
struct UinputSetup {
    id_bustype: u16,
    id_vendor: u16,
    id_product: u16,
    id_version: u16,
    name: [u8; 80],
    ff_effects_max: u32,
}

impl Default for UinputSetup {
    fn default() -> Self {
        Self {
            id_bustype: 0,
            id_vendor: 0,
            id_product: 0,
            id_version: 0,
            name: [0u8; 80],
            ff_effects_max: 0,
        }
    }
}

// uinput_abs_setup: code + absinfo (min, max, fuzz, flat, resolution).
#[repr(C)]
#[derive(Clone, Copy)]
struct UinputAbsSetup {
    code: u16,
    // The kernel header has this struct packed naturally without padding
    // because absinfo is already 4-byte aligned. But the C struct has a
    // 16-bit hole here on most arches — we mirror it with a u16 pad.
    _pad: u16,
    minimum: i32,
    maximum: i32,
    fuzz: i32,
    flat: i32,
    resolution: i32,
}

impl UinputAbsSetup {
    fn new(code: u16, min: i32, max: i32) -> Self {
        Self {
            code,
            _pad: 0,
            minimum: min,
            maximum: max,
            fuzz: 0,
            flat: 0,
            resolution: 0,
        }
    }
}

// ---- Public types ---------------------------------------------------------

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TouchPhase {
    /// Finger just touched the screen.
    Down,
    /// Finger moved while still touching.
    Move,
    /// Finger lifted.
    Up,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TouchEvent {
    /// X coordinate in container-screen pixels (0..width-1).
    pub x: i32,
    /// Y coordinate in container-screen pixels (0..height-1).
    pub y: i32,
    /// Touch phase.
    pub phase: TouchPhase,
    /// Pressure 0..255. Default 128 if omitted.
    #[serde(default = "default_pressure")]
    pub pressure: u32,
    /// Multitouch slot index. Default 0.
    #[serde(default)]
    pub slot: u32,
}

fn default_pressure() -> u32 {
    128
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum KeyCode {
    Home,
    Back,
    Recent,
}

#[derive(Debug, Clone, Deserialize)]
pub struct KeyEvent {
    pub code: KeyCode,
    /// true = key down, false = key up. A "tap" is a down+up pair.
    pub down: bool,
}

/// Handle to an open /dev/uinput device. Created once per container at
/// start time; lives for the lifetime of the container.
pub struct InputInjector {
    container_id: Uuid,
    file: Option<File>,
    /// Tracking ID counter — incremented for each new touch.
    next_tracking_id: i32,
    /// Currently-active tracking IDs per slot. 0 means "no finger here".
    /// We pre-allocate 8 slots which is plenty for typical phone UIs.
    active_tracking_ids: Vec<i32>,
    width: u32,
    height: u32,
}

impl InputInjector {
    /// Open /dev/uinput, configure it as a touchscreen + keypad, then call
    /// UI_DEV_CREATE so the kernel allocates the /dev/input/eventN node.
    ///
    /// Returns an injector whose `find_event_path()` can be called to
    /// discover the eventN path for bind-mounting into the container.
    pub fn new(container_id: Uuid, width: u32, height: u32) -> Result<Self> {
        let path = CString::new("/dev/uinput").unwrap();
        let fd = unsafe {
            libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_NONBLOCK)
        };
        if fd < 0 {
            return Err(DroidkerError::Syscall(format!(
                "open /dev/uinput: {} (need rw on /dev/uinput + CAP_SYS_ADMIN)",
                std::io::Error::last_os_error()
            )));
        }
        // SAFETY: we just opened fd, it's exclusively ours.
        let file = unsafe { File::from_raw_fd(fd) };

        let injector = Self {
            container_id,
            file: Some(file),
            next_tracking_id: 1,
            active_tracking_ids: vec![0; 8],
            width,
            height,
        };
        injector.setup_device()?;
        tracing::info!(
            container_id = %container_id,
            width,
            height,
            "uinput touchscreen created"
        );
        Ok(injector)
    }

    fn setup_device(&self) -> Result<()> {
        let file = self.file.as_ref().ok_or_else(|| {
            DroidkerError::InvalidState("uinput device already closed".into())
        })?;
        let fd = file.as_raw_fd();

        // 1. Enable event types.
        set_bit(fd, UI_SET_EVBIT, EV_KEY as i64)?;
        set_bit(fd, UI_SET_EVBIT, EV_ABS as i64)?;
        set_bit(fd, UI_SET_EVBIT, EV_SYN as i64)?;

        // 2. Enable specific keys.
        set_bit(fd, UI_SET_KEYBIT, BTN_TOUCH as i64)?;
        set_bit(fd, UI_SET_KEYBIT, KEY_BACK as i64)?;
        set_bit(fd, UI_SET_KEYBIT, KEY_HOMEPAGE as i64)?;
        set_bit(fd, UI_SET_KEYBIT, KEY_APPSELECT as i64)?;

        // 3. Enable ABS axes.
        set_bit(fd, UI_SET_ABSBIT, ABS_MT_SLOT as i64)?;
        set_bit(fd, UI_SET_ABSBIT, ABS_MT_TRACKING_ID as i64)?;
        set_bit(fd, UI_SET_ABSBIT, ABS_MT_POSITION_X as i64)?;
        set_bit(fd, UI_SET_ABSBIT, ABS_MT_POSITION_Y as i64)?;
        set_bit(fd, UI_SET_ABSBIT, ABS_MT_PRESSURE as i64)?;

        // 4. Configure ABS ranges via UI_ABS_SETUP (kernel 4.5+).
        //    On older kernels we silently fall back to [0,0] ranges — the
        //    device still works but events with non-zero positions may be
        //    filtered out by the input subsystem.
        for setup in [
            UinputAbsSetup::new(ABS_MT_SLOT, 0, 7),
            UinputAbsSetup::new(ABS_MT_TRACKING_ID, 0, 65535),
            UinputAbsSetup::new(ABS_MT_POSITION_X, 0, self.width as i32),
            UinputAbsSetup::new(ABS_MT_POSITION_Y, 0, self.height as i32),
            UinputAbsSetup::new(ABS_MT_PRESSURE, 0, 255),
        ] {
            let rc = unsafe { libc::ioctl(fd, UI_ABS_SETUP as _, &setup) };
            if rc < 0 {
                tracing::warn!(
                    code = setup.code,
                    errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0),
                    "UI_ABS_SETUP failed (kernel < 4.5?) — abs range may default to [0,0]"
                );
            }
        }

        // 5. Set device identity + name. The name shows up in
        //    /proc/bus/input/devices and Android's InputDevice.getName().
        let mut setup = UinputSetup::default();
        setup.id_bustype = 0x03; // BUS_USB
        setup.id_vendor = 0xD10D; // "D1OD" — looks like "Droid"
        setup.id_product = 0x0001;
        setup.id_version = 0x0100;
        let name = format!("droidker-touch-{}", &self.container_id.to_string()[..8]);
        let bytes = name.as_bytes();
        setup.name[..bytes.len()].copy_from_slice(bytes);
        let rc = unsafe { libc::ioctl(fd, UI_DEV_SETUP as _, &setup) };
        if rc < 0 {
            return Err(DroidkerError::Syscall(format!(
                "UI_DEV_SETUP: {}",
                std::io::Error::last_os_error()
            )));
        }

        // 6. Create the device — kernel allocates /dev/input/eventN.
        let rc = unsafe { libc::ioctl(fd, UI_DEV_CREATE as _, 0u64) };
        if rc < 0 {
            return Err(DroidkerError::Syscall(format!(
                "UI_DEV_CREATE: {}",
                std::io::Error::last_os_error()
            )));
        }
        Ok(())
    }

    /// Inject a touch event using the standard Linux multi-touch (Type B)
    /// protocol. The kernel buffers events until SYN_REPORT, then delivers
    /// them as a single atomic frame to userspace readers.
    pub fn inject_touch(&mut self, ev: &TouchEvent) -> Result<()> {
        let slot = ev.slot as usize;
        if slot >= self.active_tracking_ids.len() {
            self.active_tracking_ids.resize(slot + 1, 0);
        }
        let file = self.file.as_ref().ok_or_else(|| {
            DroidkerError::InvalidState("uinput device closed".into())
        })?;

        let mut events: Vec<InputEvent> = Vec::with_capacity(8);

        events.push(InputEvent::abs(ABS_MT_SLOT, slot as i32));
        match ev.phase {
            TouchPhase::Down => {
                let tid = self.next_tracking_id;
                self.next_tracking_id += 1;
                self.active_tracking_ids[slot] = tid;
                events.push(InputEvent::abs(ABS_MT_TRACKING_ID, tid));
                events.push(InputEvent::abs(ABS_MT_POSITION_X, ev.x));
                events.push(InputEvent::abs(ABS_MT_POSITION_Y, ev.y));
                events.push(InputEvent::abs(ABS_MT_PRESSURE, ev.pressure as i32));
                events.push(InputEvent::key(BTN_TOUCH, 1));
            }
            TouchPhase::Move => {
                events.push(InputEvent::abs(ABS_MT_POSITION_X, ev.x));
                events.push(InputEvent::abs(ABS_MT_POSITION_Y, ev.y));
                events.push(InputEvent::abs(ABS_MT_PRESSURE, ev.pressure as i32));
            }
            TouchPhase::Up => {
                events.push(InputEvent::abs(ABS_MT_TRACKING_ID, -1));
                self.active_tracking_ids[slot] = 0;
                let any_active = self.active_tracking_ids.iter().any(|&t| t > 0);
                if !any_active {
                    events.push(InputEvent::key(BTN_TOUCH, 0));
                }
            }
        }
        events.push(InputEvent::sync());

        let bytes = InputEvent::encode_batch(&events);
        let mut file = file;
        file.write_all(&bytes).map_err(|e| {
            DroidkerError::Syscall(format!("write uinput: {e}"))
        })?;
        file.flush().ok();
        Ok(())
    }

    /// Inject a key event (Home / Back / Recent). The key codes map to the
    /// standard Android buttons — Android's InputReader translates them
    /// into `KEYCODE_HOME`, `KEYCODE_BACK`, `KEYCODE_APP_SWITCH`.
    pub fn inject_key(&mut self, ev: &KeyEvent) -> Result<()> {
        let code = match ev.code {
            KeyCode::Home => KEY_HOMEPAGE,
            KeyCode::Back => KEY_BACK,
            KeyCode::Recent => KEY_APPSELECT,
        };
        let value = if ev.down { 1 } else { 0 };
        let events = [InputEvent::key(code, value), InputEvent::sync()];
        let bytes = InputEvent::encode_batch(&events);
        let file = self.file.as_ref().ok_or_else(|| {
            DroidkerError::InvalidState("uinput device closed".into())
        })?;
        let mut file = file;
        file.write_all(&bytes)?;
        file.flush().ok();
        Ok(())
    }

    /// Walk /sys/class/input/ looking for the device whose name matches
    /// the one we set in UidSetup. Returns /dev/input/eventN on success.
    pub fn find_event_path(&self) -> Option<std::path::PathBuf> {
        let expected_name = format!(
            "droidker-touch-{}",
            &self.container_id.to_string()[..8]
        );
        let entries = std::fs::read_dir("/sys/class/input").ok()?;
        for entry in entries.flatten() {
            // entry.path() looks like /sys/class/input/event12
            let name_path = entry.path().join("device/name");
            if let Ok(name) = std::fs::read_to_string(&name_path) {
                if name.trim() == expected_name {
                    // The entry's filename is the eventN we want.
                    let fname = entry.file_name();
                    let fname = fname.to_string_lossy();
                    if fname.starts_with("event") {
                        return Some(std::path::PathBuf::from("/dev/input").join(fname.to_string()));
                    }
                }
            }
        }
        None
    }

    /// Like `find_event_path` but polls /sys/class/input for up to `timeout_ms`
    /// waiting for the kernel to register the device. The kernel creates the
    /// /dev/input/eventN node asynchronously after `UI_DEV_CREATE`, and on a
    /// loaded 1-vCPU VPS this can take 50–200 ms. Without this retry loop
    /// we'd race the bind-mount on every container start.
    pub fn wait_for_event_path(&self, timeout_ms: u64) -> Option<std::path::PathBuf> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
        loop {
            if let Some(p) = self.find_event_path() {
                return Some(p);
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}

impl Drop for InputInjector {
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            let fd = file.as_raw_fd();
            // Destroy the device first so /dev/input/eventN disappears.
            unsafe {
                libc::ioctl(fd, UI_DEV_DESTROY as _, 0u64);
            }
            // file's Drop closes the fd.
            drop(file);
            tracing::info!(container_id = %self.container_id, "uinput device destroyed");
        }
    }
}

// ----- helpers --------------------------------------------------------------

fn set_bit(fd: std::os::fd::RawFd, op: u64, bit: i64) -> Result<()> {
    let rc = unsafe { libc::ioctl(fd, op as _, bit) };
    if rc < 0 {
        return Err(DroidkerError::Syscall(format!(
            "ioctl(0x{:x}, {}): {}",
            op,
            bit,
            std::io::Error::last_os_error()
        )));
    }
    Ok(())
}

/// `struct input_event` — what we write to /dev/uinput (or /dev/input/eventN).
/// Layout matches the kernel's: timeval(16) + type(2) + code(2) + value(4) = 24 bytes.
#[repr(C)]
#[derive(Clone, Copy)]
struct InputEvent {
    tv_sec: i64,
    tv_usec: i64,
    type_: u16,
    code: u16,
    value: i32,
}

impl InputEvent {
    fn abs(code: u16, value: i32) -> Self {
        Self::new(EV_ABS, code, value)
    }
    fn key(code: u16, value: i32) -> Self {
        Self::new(EV_KEY, code, value)
    }
    fn sync() -> Self {
        Self::new(EV_SYN, SYN_REPORT, 0)
    }
    fn new(type_: u16, code: u16, value: i32) -> Self {
        let mut t = libc::timeval { tv_sec: 0, tv_usec: 0 };
        // SAFETY: gettimeofday writes into a valid timeval pointer.
        unsafe { libc::gettimeofday(&mut t, std::ptr::null_mut()); }
        Self {
            tv_sec: t.tv_sec as i64,
            tv_usec: t.tv_usec as i64,
            type_,
            code,
            value,
        }
    }

    /// Encode a batch of events as a raw byte buffer. Each event is 24 bytes
    /// on x86_64 and aarch64 (16-byte timeval + 8-byte type/code/value).
    fn encode_batch(events: &[Self]) -> Vec<u8> {
        let mut buf = Vec::with_capacity(events.len() * 24);
        for ev in events {
            buf.extend_from_slice(&ev.tv_sec.to_le_bytes());
            buf.extend_from_slice(&ev.tv_usec.to_le_bytes());
            buf.extend_from_slice(&ev.type_.to_le_bytes());
            buf.extend_from_slice(&ev.code.to_le_bytes());
            buf.extend_from_slice(&ev.value.to_le_bytes());
        }
        buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn touch_event_deserializes_minimal_json() {
        let json = r#"{"x":100,"y":200,"phase":"down"}"#;
        let ev: TouchEvent = serde_json::from_str(json).unwrap();
        assert_eq!(ev.x, 100);
        assert_eq!(ev.y, 200);
        assert_eq!(ev.phase, TouchPhase::Down);
        assert_eq!(ev.pressure, 128);
        assert_eq!(ev.slot, 0);
    }

    #[test]
    fn touch_event_deserializes_full_json() {
        let json = r#"{"x":100,"y":200,"phase":"move","pressure":200,"slot":2}"#;
        let ev: TouchEvent = serde_json::from_str(json).unwrap();
        assert_eq!(ev.phase, TouchPhase::Move);
        assert_eq!(ev.slot, 2);
    }

    #[test]
    fn key_event_decodes_home_down() {
        let json = r#"{"code":"home","down":true}"#;
        let ev: KeyEvent = serde_json::from_str(json).unwrap();
        assert_eq!(ev.code, KeyCode::Home);
        assert!(ev.down);
    }

    #[test]
    fn input_event_encoding_is_24_bytes_per_event() {
        let evs = [InputEvent::sync(), InputEvent::key(BTN_TOUCH, 1)];
        let bytes = InputEvent::encode_batch(&evs);
        assert_eq!(bytes.len(), 48);
    }

    #[test]
    fn uinput_abs_setup_is_24_bytes() {
        assert_eq!(std::mem::size_of::<UinputAbsSetup>(), 24);
    }

    #[test]
    fn uinput_setup_is_92_bytes() {
        assert_eq!(std::mem::size_of::<UinputSetup>(), 92);
    }

    #[test]
    fn ioctl_constants_match_kernel_header_values() {
        // _IO('U', 1) = 0x5501 (low 16 bits).
        assert_eq!(UI_DEV_CREATE & 0xFFFF, 0x5501);
        assert_eq!(UI_DEV_DESTROY & 0xFFFF, 0x5502);

        // _IOW('U', 1, int) = (1<<30) | (4<<16) | ('U'<<8) | 1
        //                  = 0x40045501
        assert_eq!(UI_SET_EVBIT, 0x40045501);

        // _IOW('U', 21, int) = 0x40045515
        assert_eq!(UI_SET_KEYBIT, 0x40045515);

        // _IOW('U', 34, int) = 0x40045522
        assert_eq!(UI_SET_ABSBIT, 0x40045522);

        // _IOW('U', 54, struct uinput_abs_setup) = (1<<30) | (24<<16) | ('U'<<8) | 54
        //                                       = 0x40185536
        assert_eq!(UI_ABS_SETUP, 0x40185536);

        // _IOW('U', 3, struct uinput_setup) = (1<<30) | (92<<16) | ('U'<<8) | 3
        //                                  = 0x405C5503
        assert_eq!(UI_DEV_SETUP, 0x405C5503);
    }
}
