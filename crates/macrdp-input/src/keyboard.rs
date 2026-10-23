use anyhow::Result;
use core_graphics::event::{CGEvent, CGKeyCode};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use std::sync::Mutex;

use crate::keymap::scancode_to_keycode;

pub struct KeyboardInjector {
    surrogates: Mutex<SurrogateJoiner>,
}

impl KeyboardInjector {
    pub fn new() -> Result<Self> {
        // Verify we can create an event source (permission check)
        let _ = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|_| anyhow::anyhow!("Failed to create CGEventSource — check Accessibility permission"))?;
        Ok(Self {
            surrogates: Mutex::default(),
        })
    }

    fn source() -> Result<CGEventSource> {
        CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|_| anyhow::anyhow!("Failed to create CGEventSource"))
    }

    /// Inject a key press or release event
    pub fn inject_key(&self, scancode: u8, extended: bool, pressed: bool) -> Result<()> {
        let keycode = match scancode_to_keycode(scancode, extended) {
            Some(kc) => kc,
            None => {
                tracing::warn!(scancode, extended, "Unknown scancode, ignoring");
                return Ok(());
            }
        };

        let source = Self::source()?;
        let event = CGEvent::new_keyboard_event(source, keycode as CGKeyCode, pressed)
            .map_err(|_| anyhow::anyhow!("Failed to create keyboard event"))?;

        crate::post(&event);
        tracing::trace!(scancode, keycode, pressed, "Keyboard event injected");
        Ok(())
    }

    /// Inject one UTF-16 unit of a unicode key press/release.
    ///
    /// A press types the whole character (down + up, both carrying the string);
    /// a surrogate pair waits for its low half. Releases post nothing.
    pub fn inject_unicode(&self, ch: u16, pressed: bool) -> Result<()> {
        if !pressed {
            return Ok(());
        }
        let Some(units) = self.surrogates.lock().unwrap_or_else(|e| e.into_inner()).press(ch) else {
            return Ok(());
        };

        for down in [true, false] {
            let source = Self::source()?;
            let event = CGEvent::new_keyboard_event(source, 0, down)
                .map_err(|_| anyhow::anyhow!("Failed to create unicode event"))?;
            event.set_string_from_utf16_unchecked(&units);
            crate::post(&event);
        }
        Ok(())
    }

    /// Apply the client's caps lock state (RDP Synchronize).
    pub fn sync_caps_lock(&self, on: bool) -> Result<()> {
        let hid = HidSystem::open()?;
        if hid.caps_lock()? != on {
            hid.set_caps_lock(on)?;
            tracing::debug!(on, "Caps lock synchronized");
        }
        Ok(())
    }
}

/// Pairs UTF-16 surrogates so a non-BMP character posts as one event
#[derive(Default)]
struct SurrogateJoiner {
    high: Option<u16>,
}

impl SurrogateJoiner {
    /// The UTF-16 units to type for this press, once a character is complete
    fn press(&mut self, unit: u16) -> Option<Vec<u16>> {
        match unit {
            0xd800..=0xdbff => {
                self.high = Some(unit);
                None
            }
            0xdc00..=0xdfff => self.high.take().map(|high| vec![high, unit]),
            _ => {
                self.high = None;
                Some(vec![unit])
            }
        }
    }
}

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOServiceMatching(name: *const std::ffi::c_char) -> *mut std::ffi::c_void;
    fn IOServiceGetMatchingService(main_port: u32, matching: *mut std::ffi::c_void) -> u32;
    fn IOServiceOpen(service: u32, owning_task: u32, connect_type: u32, connect: *mut u32) -> i32;
    fn IOServiceClose(connect: u32) -> i32;
    fn IOObjectRelease(object: u32) -> i32;
    fn IOHIDGetModifierLockState(handle: u32, selector: i32, state: *mut bool) -> i32;
    fn IOHIDSetModifierLockState(handle: u32, selector: i32, state: bool) -> i32;
    static mach_task_self_: u32;
}

/// kIOHIDParamConnectType
const HID_PARAM_CONNECT_TYPE: u32 = 1;
/// kIOHIDCapsLockState
const HID_CAPS_LOCK_STATE: i32 = 1;

/// Connection to IOHIDSystem, which owns the system-wide lock-key state
/// (CGEventSource cannot set it).
struct HidSystem(u32);

impl HidSystem {
    fn open() -> Result<Self> {
        unsafe {
            // IOServiceGetMatchingService consumes the matching dictionary.
            let service = IOServiceGetMatchingService(0, IOServiceMatching(c"IOHIDSystem".as_ptr()));
            if service == 0 {
                anyhow::bail!("IOHIDSystem service not found");
            }
            let mut connect = 0;
            let kr = IOServiceOpen(service, mach_task_self_, HID_PARAM_CONNECT_TYPE, &mut connect);
            IOObjectRelease(service);
            if kr != 0 {
                anyhow::bail!("IOServiceOpen(IOHIDSystem) failed: {kr:#x}");
            }
            Ok(Self(connect))
        }
    }

    fn caps_lock(&self) -> Result<bool> {
        let mut state = false;
        let kr = unsafe { IOHIDGetModifierLockState(self.0, HID_CAPS_LOCK_STATE, &mut state) };
        if kr != 0 {
            anyhow::bail!("IOHIDGetModifierLockState failed: {kr:#x}");
        }
        Ok(state)
    }

    fn set_caps_lock(&self, on: bool) -> Result<()> {
        let kr = unsafe { IOHIDSetModifierLockState(self.0, HID_CAPS_LOCK_STATE, on) };
        if kr != 0 {
            anyhow::bail!("IOHIDSetModifierLockState failed: {kr:#x}");
        }
        Ok(())
    }
}

impl Drop for HidSystem {
    fn drop(&mut self) {
        unsafe { IOServiceClose(self.0) };
    }
}

#[cfg(test)]
fn caps_lock_state() -> Result<bool> {
    HidSystem::open()?.caps_lock()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bmp_unit_types_immediately() {
        let mut j = SurrogateJoiner::default();
        assert_eq!(j.press(0x00e9), Some(vec![0x00e9]));
    }

    #[test]
    fn surrogate_pair_types_as_one_string() {
        // U+1F600 = D83D DE00
        let mut j = SurrogateJoiner::default();
        assert_eq!(j.press(0xd83d), None);
        assert_eq!(j.press(0xde00), Some(vec![0xd83d, 0xde00]));
        assert_eq!(j.press(0xde00), None, "pair is consumed");
    }

    #[test]
    fn unpaired_surrogates_are_dropped() {
        let mut j = SurrogateJoiner::default();
        assert_eq!(j.press(0xdc00), None);
        assert_eq!(j.press(0xd83d), None);
        assert_eq!(j.press(0x0041), Some(vec![0x0041]));
        assert_eq!(j.press(0xde00), None, "stale high surrogate was cleared");
    }

    /// Reads the live caps lock state; needs a logged-in GUI session.
    #[test]
    #[ignore]
    fn caps_lock_state_is_readable() {
        caps_lock_state().unwrap();
    }
}
