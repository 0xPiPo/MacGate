//! Curtain input block: while a curtain session runs, local keyboard, mouse, trackpad and media
//! keys are dropped at the HID event tap so only the RDP client controls the Mac.
//!
//! Ours vs local: every event macrdp injects carries a per-process random value in
//! kCGEventSourceUserData (field 42, see [`crate::mark`]); hardware events carry 0 and other
//! processes cannot guess the value. The tap runs on its own run-loop thread (an active tap
//! that is not serviced stalls input system-wide) and re-enables itself when macOS disables it.

use std::collections::hash_map::RandomState;
use std::ffi::c_void;
use std::hash::BuildHasher;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use core_foundation::base::TCFType;
use core_foundation::mach_port::{CFMachPort, CFMachPortRef};
use core_foundation::runloop::{kCFRunLoopCommonModes, kCFRunLoopDefaultMode, CFRunLoop};

/// kCGEventSourceUserData.
pub(crate) const USER_DATA_FIELD: u32 = 42;

const TAP_DISABLED_BY_TIMEOUT: u32 = 0xFFFF_FFFE;
const TAP_DISABLED_BY_USER_INPUT: u32 = 0xFFFF_FFFF;

/// Local input the curtain drops: mouse buttons/moves/drags (1-7, 25-27), keys and modifiers
/// (10-12), media/brightness/volume keys (14, NX_SYSDEFINED), scroll (22), tablet (23-24) and
/// trackpad gestures (18-20, 29-32, 34).
const BLOCKED_TYPES: [u32; 25] = [
    1, 2, 3, 4, 5, 6, 7, 10, 11, 12, 14, 18, 19, 20, 22, 23, 24, 25, 26, 27, 29, 30, 31, 32, 34,
];

fn event_mask() -> u64 {
    BLOCKED_TYPES.iter().fold(0, |m, t| m | (1u64 << t))
}

/// Random per process, never 0 (the value of every unmarked event).
pub fn injected_marker() -> i64 {
    static MARKER: OnceLock<i64> = OnceLock::new();
    *MARKER.get_or_init(|| match RandomState::new().hash_one(std::process::id()) as i64 {
        0 => 1,
        m => m,
    })
}

#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    Pass,
    Drop,
    /// macOS disabled the tap (slow callback or user input); turn it back on.
    Reenable,
}

fn verdict(event_type: u32, user_data: i64, marker: i64) -> Verdict {
    match event_type {
        TAP_DISABLED_BY_TIMEOUT | TAP_DISABLED_BY_USER_INPUT => Verdict::Reenable,
        _ if user_data == marker => Verdict::Pass,
        _ => Verdict::Drop,
    }
}

type TapCallback = unsafe extern "C" fn(*mut c_void, u32, *mut c_void, *mut c_void) -> *mut c_void;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventTapCreate(tap: u32, place: u32, options: u32, mask: u64, callback: TapCallback, info: *mut c_void) -> *mut c_void;
    fn CGEventTapEnable(tap: *mut c_void, enable: bool);
    fn CGEventGetIntegerValueField(event: *mut c_void, field: u32) -> i64;
}

/// kCGHIDEventTap, kCGHeadInsertEventTap, kCGEventTapOptionDefault (an active filter).
const HID_TAP: u32 = 0;
const HEAD_INSERT: u32 = 0;
const ACTIVE_FILTER: u32 = 0;

struct TapState {
    marker: i64,
    port: AtomicPtr<c_void>,
}

/// Must not panic: it runs inside CoreGraphics.
unsafe extern "C" fn tap_callback(_proxy: *mut c_void, etype: u32, event: *mut c_void, info: *mut c_void) -> *mut c_void {
    // SAFETY: `info` is the TapState the tap thread owns until after the tap is invalidated.
    let state = unsafe { &*(info as *const TapState) };
    let user_data = match etype {
        TAP_DISABLED_BY_TIMEOUT | TAP_DISABLED_BY_USER_INPUT => 0,
        // SAFETY: CoreGraphics hands a valid event for every other type.
        _ => unsafe { CGEventGetIntegerValueField(event, USER_DATA_FIELD) },
    };
    match verdict(etype, user_data, state.marker) {
        Verdict::Pass => event,
        Verdict::Drop => null_mut(),
        Verdict::Reenable => {
            // SAFETY: the port is valid while callbacks run (set before the tap is enabled).
            unsafe { CGEventTapEnable(state.port.load(Ordering::SeqCst), true) };
            tracing::warn!(etype, "Local input block was disabled by macOS; re-enabled");
            event
        }
    }
}

/// Drops local input while alive. Dropping it removes the tap.
pub struct LocalInputBlocker {
    stop: Arc<AtomicBool>,
    run_loop: CFRunLoop,
    thread: Option<JoinHandle<()>>,
}

impl LocalInputBlocker {
    /// Fails without Accessibility permission (macOS refuses the tap).
    pub fn install() -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel();
        let thread_stop = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("macrdp-input-block".into())
            .spawn(move || run_tap(&thread_stop, &tx))
            .context("spawn input block thread")?;
        let started = rx.recv().map_err(|_| anyhow!("input block thread exited during setup"));
        match started.and_then(|r| r) {
            Ok(run_loop) => {
                tracing::info!("Local keyboard and mouse blocked for the session");
                Ok(Self { stop, run_loop, thread: Some(thread) })
            }
            Err(e) => {
                let _ = thread.join();
                Err(e)
            }
        }
    }
}

fn run_tap(stop: &AtomicBool, tx: &std::sync::mpsc::Sender<Result<CFRunLoop>>) {
    let state = Box::new(TapState { marker: injected_marker(), port: AtomicPtr::new(null_mut()) });
    let info = std::ptr::from_ref::<TapState>(&state).cast_mut().cast::<c_void>();
    // SAFETY: plain CoreGraphics call; `state` outlives the tap (invalidated below first).
    let port = unsafe { CGEventTapCreate(HID_TAP, HEAD_INSERT, ACTIVE_FILTER, event_mask(), tap_callback, info) };
    if port.is_null() {
        let _ = tx.send(Err(anyhow!("CGEventTapCreate refused (Accessibility permission missing?)")));
        return;
    }
    state.port.store(port, Ordering::SeqCst);
    // SAFETY: CGEventTapCreate follows the create rule; CFMachPort releases it on drop.
    let mach_port = unsafe { CFMachPort::wrap_under_create_rule(port as CFMachPortRef) };
    let run_loop = CFRunLoop::get_current();
    let source = match mach_port.create_runloop_source(0) {
        Ok(source) => source,
        Err(()) => {
            // SAFETY: valid port we own.
            unsafe { core_foundation_sys::mach_port::CFMachPortInvalidate(port as CFMachPortRef) };
            let _ = tx.send(Err(anyhow!("event tap run-loop source failed")));
            return;
        }
    };
    // SAFETY: static CF mode constants; the port is ours.
    unsafe {
        run_loop.add_source(&source, kCFRunLoopCommonModes);
        CGEventTapEnable(port, true);
    }
    if tx.send(Ok(run_loop.clone())).is_err() {
        stop.store(true, Ordering::SeqCst);
    }
    // Bounded runs: a stop that lands before the loop starts is still seen.
    while !stop.load(Ordering::SeqCst) {
        // SAFETY: static CF mode constant.
        CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, Duration::from_millis(500), false);
    }
    // SAFETY: valid port we own; after invalidation no callback can reach `state`.
    unsafe {
        CGEventTapEnable(port, false);
        run_loop.remove_source(&source, kCFRunLoopCommonModes);
        core_foundation_sys::mach_port::CFMachPortInvalidate(port as CFMachPortRef);
    }
    drop(source);
    drop(mach_port);
    drop(state);
    tracing::info!("Local keyboard and mouse unblocked");
}

impl Drop for LocalInputBlocker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.run_loop.stop();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MOUSE_MOVED: u32 = 5;
    const KEY_DOWN: u32 = 10;
    const SCROLL_WHEEL: u32 = 22;

    #[test]
    fn marker_is_nonzero_and_stable() {
        assert_ne!(injected_marker(), 0, "0 is what every unmarked event carries");
        assert_eq!(injected_marker(), injected_marker());
    }

    #[test]
    fn marked_events_carry_the_marker() {
        use core_graphics::event::{CGEvent, EventField};
        use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState).expect("source");
        let event = CGEvent::new(source).expect("event");
        assert_eq!(event.get_integer_value_field(EventField::EVENT_SOURCE_USER_DATA), 0);
        crate::mark(&event);
        assert_eq!(event.get_integer_value_field(EventField::EVENT_SOURCE_USER_DATA), injected_marker());
    }

    #[test]
    fn our_events_pass_and_local_ones_drop() {
        let m = injected_marker();
        assert_eq!(verdict(KEY_DOWN, m, m), Verdict::Pass);
        assert_eq!(verdict(KEY_DOWN, 0, m), Verdict::Drop);
        assert_eq!(verdict(MOUSE_MOVED, m ^ 1, m), Verdict::Drop);
        assert_eq!(verdict(SCROLL_WHEEL, 0, m), Verdict::Drop);
    }

    #[test]
    fn a_disabled_tap_is_reenabled() {
        let m = injected_marker();
        assert_eq!(verdict(TAP_DISABLED_BY_TIMEOUT, 0, m), Verdict::Reenable);
        assert_eq!(verdict(TAP_DISABLED_BY_USER_INPUT, 0, m), Verdict::Reenable);
    }

    #[test]
    fn mask_covers_keyboard_mouse_scroll_gestures_and_media_keys() {
        let mask = event_mask();
        for t in [1, 2, 3, 4, 5, 6, 7, 10, 11, 12, 14, 22, 25, 26, 27, 29, 30, 31] {
            assert!(mask & (1 << t) != 0, "event type {t} not blocked");
        }
        assert_eq!(mask & 1, 0, "null events are not input");
    }
}
