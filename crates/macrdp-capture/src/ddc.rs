//! Monitor standby over DDC/CI (VCP 0xD6 power mode) for the curtain.
//!
//! The monitor itself sleeps; macOS still sees the display as normal, so nothing in the
//! display configuration can get stuck (unlike CGSConfigureDisplayEnabled, which left HDMI
//! dark until reboot). If macrdp dies mid-session, the monitor's power button wakes it, and
//! the next start wakes it too (`wake_if_left_asleep`).
//!
//! Uses the private IOAVService I2C calls on Apple Silicon (as m1ddc, MonitorControl and
//! BetterDisplay do). Used only once the app's standby test showed the monitors honour it
//! (`curtain_standby_ok`); otherwise the curtain uses the app's black shield.

use std::path::PathBuf;

/// VCP 0xD6 values. Live test 2026-10-24 (ASUS XG27AQDMES): 4 was ignored, 5 slept and 1 woke.
const POWER_ON: u16 = 1;
const POWER_OFF: u16 = 5;

/// DDC/CI "Set VCP Feature" packet for `IOAVServiceWriteI2C(service, 0x37, 0x51, ..)`.
fn set_vcp_packet(vcp: u8, value: u16) -> [u8; 6] {
    let [hi, lo] = value.to_be_bytes();
    let mut p = [0x84, 0x03, vcp, hi, lo, 0];
    // Checksum over the destination (0x6E), source (0x51) and payload bytes.
    p[5] = p[..5].iter().fold(0x6E ^ 0x51, |acc, b| acc ^ b);
    p
}

/// Put every external monitor in standby (`on = false`) or wake it; returns how many monitors
/// accepted the write (a monitor can accept it and still ignore the value). Best effort: logs,
/// never fails.
pub fn set_external_power(on: bool) -> usize {
    let marker = marker_path();
    if !on {
        // Written first, so a crash between standby and wake is recovered on the next start.
        if let Some(m) = &marker {
            let _ = std::fs::write(m, b"");
        }
    }
    let sent = ffi::write_vcp_to_externals(&set_vcp_packet(0xD6, if on { POWER_ON } else { POWER_OFF }));
    tracing::info!(on, monitors = sent, "DDC monitor power");
    if on {
        if let Some(m) = &marker {
            let _ = std::fs::remove_file(m);
        }
    }
    sent
}

/// Standby again during a curtain (the monitor's power button may have woken it). Debug log
/// only: it runs every few seconds.
pub(crate) fn resend_standby() {
    let sent = ffi::write_vcp_to_externals(&set_vcp_packet(0xD6, POWER_OFF));
    tracing::debug!(monitors = sent, "DDC standby re-sent");
}

/// Wake monitors a previous macrdp run put in standby and never woke (crash, kill).
pub fn wake_if_left_asleep() {
    if marker_path().is_some_and(|m| m.exists()) {
        tracing::warn!("Monitors were left in DDC standby by a previous run; waking them");
        set_external_power(true);
    }
}

fn marker_path() -> Option<PathBuf> {
    let dir = dirs::home_dir()?.join("Library/Application Support/macrdp");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join(".monitors-in-standby"))
}

mod ffi {
    use core_foundation::base::TCFType;
    use core_foundation::string::CFString;
    use std::ffi::c_void;

    type IoObject = u32;
    type AvService = *const c_void;

    #[link(name = "IOKit", kind = "framework")]
    extern "C" {
        fn IOServiceMatching(name: *const std::ffi::c_char) -> *mut c_void;
        fn IOServiceGetMatchingServices(main_port: u32, matching: *mut c_void, iter: *mut IoObject) -> i32;
        fn IOIteratorNext(iter: IoObject) -> IoObject;
        fn IOObjectRelease(obj: IoObject) -> i32;
        fn IORegistryEntryCreateCFProperty(entry: IoObject, key: *const c_void, alloc: *const c_void, opts: u32) -> *const c_void;
        fn IOAVServiceCreateWithService(alloc: *const c_void, service: IoObject) -> AvService;
        fn IOAVServiceWriteI2C(service: AvService, chip: u32, offset: u32, data: *const u8, len: u32) -> i32;
    }
    extern "C" {
        fn CFRelease(cf: *const c_void);
    }

    /// Write `packet` to every external DCP AV service; returns how many accepted it.
    pub(super) fn write_vcp_to_externals(packet: &[u8; 6]) -> usize {
        let mut sent = 0;
        // SAFETY: plain IOKit calls; every object we get is released, CF objects too. The
        // private IOAV signatures match m1ddc's declarations, used against the same IOKit.
        unsafe {
            let mut iter: IoObject = 0;
            if IOServiceGetMatchingServices(0, IOServiceMatching(c"DCPAVServiceProxy".as_ptr()), &mut iter) != 0 {
                return 0;
            }
            let key = CFString::from_static_string("Location");
            loop {
                let svc = IOIteratorNext(iter);
                if svc == 0 {
                    break;
                }
                let loc = IORegistryEntryCreateCFProperty(svc, key.as_concrete_TypeRef().cast(), std::ptr::null(), 0);
                let external = !loc.is_null()
                    && CFString::wrap_under_create_rule(loc.cast()) == CFString::from_static_string("External");
                if external {
                    let av = IOAVServiceCreateWithService(std::ptr::null(), svc);
                    if !av.is_null() {
                        // Monitors miss single writes now and then; m1ddc also sends twice.
                        let ok = (0..2).fold(false, |ok, _| {
                            std::thread::sleep(std::time::Duration::from_millis(10));
                            IOAVServiceWriteI2C(av, 0x37, 0x51, packet.as_ptr(), 6) == 0 || ok
                        });
                        sent += usize::from(ok);
                        CFRelease(av);
                    }
                }
                IOObjectRelease(svc);
            }
            IOObjectRelease(iter);
        }
        sent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn power_packets_match_the_bytes_that_worked_live() {
        // Same bytes as the scratch test that slept and woke the owner's monitor.
        assert_eq!(set_vcp_packet(0xD6, 5), [0x84, 0x03, 0xD6, 0x00, 0x05, 0x6E ^ 0x51 ^ 0x84 ^ 0x03 ^ 0xD6 ^ 0x05]);
        assert_eq!(set_vcp_packet(0xD6, 1)[5], 0x6E ^ 0x51 ^ 0x84 ^ 0x03 ^ 0xD6 ^ 0x01);
    }
}

#[cfg(test)]
mod live {
    /// Sleeps the real external monitors for 3 s. Run only at the desk:
    /// `cargo test -p macrdp-capture ddc::live -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn standby_then_wake() {
        assert!(super::ffi::write_vcp_to_externals(&super::set_vcp_packet(0xD6, super::POWER_OFF)) > 0);
        std::thread::sleep(std::time::Duration::from_secs(3));
        assert!(super::ffi::write_vcp_to_externals(&super::set_vcp_packet(0xD6, super::POWER_ON)) > 0);
    }
}
