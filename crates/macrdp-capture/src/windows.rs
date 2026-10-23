//! Moves windows between displays with the Accessibility API, for the curtain without standby.
//!
//! When a display goes away macOS gathers its windows onto the remaining displays, but not onto
//! the display each came from; so the curtain records where every window was and puts it back
//! before the session display is destroyed.

use std::ffi::c_void;

use core_foundation::array::CFArray;
use core_foundation::base::{CFType, CFTypeRef, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
use core_foundation::number::CFNumber;
use core_foundation::string::{CFString, CFStringRef};
use core_graphics::window::{
    copy_window_info, kCGWindowLayer, kCGWindowListExcludeDesktopElements, kCGWindowListOptionOnScreenOnly,
    kCGWindowOwnerPID,
};

use crate::virtual_display::{display_bounds, online_displays};

/// Global bounds in points: x, y, width, height (as [`crate::virtual_display::display_bounds`]).
pub type Rect = (f64, f64, f64, f64);

/// Where a window at `win` on display `from` goes on display `to`: the same relative position,
/// shrunk to fit and kept inside `to`.
pub(crate) fn relocate(win: Rect, from: Rect, to: Rect) -> Rect {
    let (w, h) = (win.2.min(to.2), win.3.min(to.3));
    let x = to.0 + (win.0 - from.0) / from.2.max(1.0) * to.2;
    let y = to.1 + (win.1 - from.1) / from.3.max(1.0) * to.3;
    (x.clamp(to.0, to.0 + to.2 - w), y.clamp(to.1, to.1 + to.3 - h), w, h)
}

/// The display holding the window's centre.
pub(crate) fn display_of(win: Rect, displays: &[(u32, Rect)]) -> Option<u32> {
    let (cx, cy) = (win.0 + win.2 / 2.0, win.1 + win.3 / 2.0);
    displays
        .iter()
        .find(|(_, d)| cx >= d.0 && cx < d.0 + d.2 && cy >= d.1 && cy < d.1 + d.3)
        .map(|&(id, _)| id)
}

/// A window's origin relative to its display's, which survives the display moving.
fn offset_in(win: Rect, display: Rect) -> (f64, f64) {
    (win.0 - display.0, win.1 - display.1)
}

/// The global origin for `offset` on a display now at `display`.
fn home(offset: (f64, f64), display: Rect) -> (f64, f64) {
    (display.0 + offset.0, display.1 + offset.1)
}

/// An AXUIElement for a window.
struct Window(CFType);

// SAFETY: an AXUIElementRef is an immutable CF object; retain/release are thread-safe and AX
// calls on it are IPC messages that may come from any thread.
unsafe impl Send for Window {}

/// A window moved into the session display, with what it takes to put it back.
pub(crate) struct Moved {
    window: Window,
    display: u32,
    offset: (f64, f64),
    /// Its own size when it had to shrink to fit.
    size: Option<(f64, f64)>,
}

/// Moves every window on `displays` (physical displays with their current bounds) into
/// `target`. Minimized and full-screen windows stay; a window that refuses is skipped.
pub(crate) fn move_into(target: Rect, displays: &[(u32, Rect)]) -> Vec<Moved> {
    let (mut moved, mut skipped, mut failed) = (Vec::new(), 0, 0);
    for pid in window_owners() {
        // SAFETY: plain AX call; a null result means no such application.
        let app = unsafe { AXUIElementCreateApplication(pid) };
        if app.is_null() {
            continue;
        }
        // SAFETY: `app` is a +1 reference we own from here.
        let app = unsafe { CFType::wrap_under_create_rule(app) };
        // A hung application must not stall the session start for the default 6 s per call.
        // SAFETY: plain AX call on a live element.
        unsafe { AXUIElementSetMessagingTimeout(app.as_CFTypeRef(), 0.5) };
        let Some(windows) = attr(&app, "AXWindows").and_then(|v| v.downcast_into::<CFArray>()) else { continue };
        for item in windows.iter() {
            // SAFETY: AXWindows holds AXUIElements; the array keeps them alive, we retain ours.
            let window = Window(unsafe { CFType::wrap_under_get_rule(*item) });
            if flag(&window.0, "AXMinimized") || flag(&window.0, "AXFullScreen") {
                skipped += 1;
                continue;
            }
            let Some(frame) = frame(&window.0) else {
                failed += 1;
                continue;
            };
            let Some(display) = display_of(frame, displays) else { continue };
            let from = displays.iter().find(|d| d.0 == display).map_or(frame, |d| d.1);
            let to = relocate(frame, from, target);
            let resized = (to.2, to.3) != (frame.2, frame.3);
            // Shrink first, so the move is not refused for a window larger than the display.
            let ok = (!resized || set_pair(&window.0, "AXSize", AX_SIZE, (to.2, to.3)))
                && set_pair(&window.0, "AXPosition", AX_POINT, (to.0, to.1));
            if !ok {
                failed += 1;
                continue;
            }
            moved.push(Moved {
                window,
                display,
                offset: offset_in(frame, from),
                size: resized.then_some((frame.2, frame.3)),
            });
        }
    }
    tracing::info!(moved = moved.len(), skipped, failed, "Windows moved into the session display");
    moved
}

/// Puts moved windows back on their displays (where those still exist). Returns how many went back.
pub(crate) fn restore(moved: &[Moved]) -> usize {
    let online = online_displays();
    let mut back = 0;
    for m in moved.iter().filter(|m| online.contains(&m.display)) {
        let mut ok = set_pair(&m.window.0, "AXPosition", AX_POINT, home(m.offset, display_bounds(Some(m.display))));
        // Size after the move: back on its own display it fits again.
        if let Some(size) = m.size {
            ok &= set_pair(&m.window.0, "AXSize", AX_SIZE, size);
        }
        back += usize::from(ok);
    }
    tracing::info!(back, of = moved.len(), "Windows moved back to their displays");
    back
}

/// Processes owning a normal (layer 0) on-screen window, this one excluded.
fn window_owners() -> Vec<i32> {
    let Some(list) = copy_window_info(kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements, 0) else {
        return Vec::new();
    };
    let me = std::process::id() as i64;
    let mut pids = Vec::new();
    for item in list.iter() {
        // SAFETY: CGWindowListCopyWindowInfo returns an array of dictionaries; the array keeps
        // them alive while we borrow.
        let info: CFDictionary = unsafe { CFDictionary::wrap_under_get_rule(*item as CFDictionaryRef) };
        let number = |key: CFStringRef| {
            info.find(key.cast::<c_void>())
                // SAFETY: both keys hold CFNumbers.
                .and_then(|v| unsafe { CFNumber::wrap_under_get_rule((*v).cast()) }.to_i64())
        };
        // SAFETY: CoreGraphics' constant keys.
        let (pid, layer) = unsafe { (number(kCGWindowOwnerPID), number(kCGWindowLayer)) };
        if let (Some(pid), Some(0)) = (pid, layer) {
            if pid != me && !pids.contains(&(pid as i32)) {
                pids.push(pid as i32);
            }
        }
    }
    pids
}

fn attr(element: &CFType, name: &'static str) -> Option<CFType> {
    let key = CFString::from_static_string(name);
    let mut value: CFTypeRef = std::ptr::null();
    // SAFETY: plain AX call; on success `value` is a +1 reference we take over.
    let err = unsafe { AXUIElementCopyAttributeValue(element.as_CFTypeRef(), key.as_concrete_TypeRef(), &mut value) };
    (err == 0 && !value.is_null()).then(|| unsafe { CFType::wrap_under_create_rule(value) })
}

fn flag(element: &CFType, name: &'static str) -> bool {
    attr(element, name).and_then(|v| v.downcast_into::<CFBoolean>()).is_some_and(bool::from)
}

/// An AXValue CGPoint or CGSize attribute as a pair.
fn pair(element: &CFType, name: &'static str, kind: u32) -> Option<(f64, f64)> {
    let value = attr(element, name)?;
    let mut out = [0f64; 2];
    // SAFETY: CGPoint and CGSize are two f64s; AXValueGetValue checks the kind.
    unsafe { AXValueGetValue(value.as_CFTypeRef(), kind, out.as_mut_ptr().cast()) }.then_some((out[0], out[1]))
}

fn frame(element: &CFType) -> Option<Rect> {
    let (x, y) = pair(element, "AXPosition", AX_POINT)?;
    let (w, h) = pair(element, "AXSize", AX_SIZE)?;
    Some((x, y, w, h))
}

fn set_pair(element: &CFType, name: &'static str, kind: u32, (a, b): (f64, f64)) -> bool {
    let raw = [a, b];
    // SAFETY: CGPoint and CGSize are two f64s; AXValueCreate copies them.
    let value = unsafe { AXValueCreate(kind, raw.as_ptr().cast()) };
    if value.is_null() {
        return false;
    }
    // SAFETY: AXValueCreate returned a +1 reference.
    let value = unsafe { CFType::wrap_under_create_rule(value) };
    let key = CFString::from_static_string(name);
    // SAFETY: plain AX call on live objects.
    unsafe { AXUIElementSetAttributeValue(element.as_CFTypeRef(), key.as_concrete_TypeRef(), value.as_CFTypeRef()) == 0 }
}

/// kAXValueCGPointType, kAXValueCGSizeType.
const AX_POINT: u32 = 1;
const AX_SIZE: u32 = 2;

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXUIElementCreateApplication(pid: i32) -> CFTypeRef;
    fn AXUIElementCopyAttributeValue(element: CFTypeRef, attribute: CFStringRef, value: *mut CFTypeRef) -> i32;
    fn AXUIElementSetAttributeValue(element: CFTypeRef, attribute: CFStringRef, value: CFTypeRef) -> i32;
    fn AXUIElementSetMessagingTimeout(element: CFTypeRef, seconds: f32) -> i32;
    fn AXValueCreate(kind: u32, value: *const c_void) -> CFTypeRef;
    fn AXValueGetValue(value: CFTypeRef, kind: u32, out: *mut c_void) -> bool;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_keeps_its_relative_place_on_the_session_display() {
        // Middle of a 2560x1440 monitor onto a 1280x720 session display.
        let to = (0.0, 0.0, 1280.0, 720.0);
        assert_eq!(relocate((1280.0, 360.0, 640.0, 360.0), (0.0, 0.0, 2560.0, 1440.0), to), (640.0, 180.0, 640.0, 360.0));
        // Source left of main, target at a non-zero origin.
        assert_eq!(
            relocate((-1920.0 + 480.0, 270.0, 400.0, 300.0), (-1920.0, 0.0, 1920.0, 1080.0), (100.0, 50.0, 1600.0, 900.0)),
            (500.0, 275.0, 400.0, 300.0)
        );
    }

    #[test]
    fn a_window_too_big_for_the_session_display_shrinks_to_fit() {
        let to = (0.0, 0.0, 1280.0, 720.0);
        assert_eq!(relocate((0.0, 0.0, 2560.0, 1440.0), (0.0, 0.0, 2560.0, 1440.0), to), to);
        assert_eq!(relocate((100.0, 100.0, 1600.0, 300.0), (0.0, 0.0, 2560.0, 1440.0), to), (0.0, 50.0, 1280.0, 300.0));
    }

    #[test]
    fn a_window_near_an_edge_stays_inside_the_session_display() {
        let to = (0.0, 0.0, 1280.0, 720.0);
        // Right/bottom: proportional origin (1120, 630) would hang off; pulled back in.
        assert_eq!(relocate((2240.0, 1260.0, 800.0, 400.0), (0.0, 0.0, 2560.0, 1440.0), to), (480.0, 320.0, 800.0, 400.0));
        // Partly off the source display's top-left: clamped to the target's origin.
        assert_eq!(relocate((-50.0, -20.0, 300.0, 200.0), (0.0, 0.0, 2560.0, 1440.0), to), (0.0, 0.0, 300.0, 200.0));
    }

    #[test]
    fn a_window_belongs_to_the_display_holding_its_centre() {
        let displays = [(1, (0.0, 0.0, 1512.0, 982.0)), (2, (1512.0, 0.0, 2560.0, 1440.0))];
        assert_eq!(display_of((1400.0, 100.0, 400.0, 300.0), &displays), Some(2), "centre x 1600");
        assert_eq!(display_of((100.0, 100.0, 400.0, 300.0), &displays), Some(1));
        assert_eq!(display_of((-900.0, 100.0, 400.0, 300.0), &displays), None, "on no physical display");
    }

    #[test]
    fn a_moved_window_goes_back_to_the_same_place_on_its_display() {
        // Recorded while its monitor sat at x=1280 (session display main); restored after the
        // monitor is back at x=1512.
        let offset = offset_in((1580.0, 120.0, 500.0, 400.0), (1280.0, 0.0, 2560.0, 1440.0));
        assert_eq!(offset, (300.0, 120.0));
        assert_eq!(home(offset, (1512.0, 0.0, 2560.0, 1440.0)), (1812.0, 120.0));
    }
}
