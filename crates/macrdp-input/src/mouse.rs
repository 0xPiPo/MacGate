use anyhow::Result;
use core_graphics::event::{CGEvent, CGEventType, CGMouseButton, EventField};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::CGPoint;
use foreign_types::ForeignType;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

extern "C" {
    fn CGEventCreateScrollWheelEvent2(
        source: *mut core_graphics::sys::CGEventSource,
        units: u32,
        wheel_count: u32,
        wheel1: i32,
        wheel2: i32,
        wheel3: i32,
    ) -> *mut core_graphics::sys::CGEvent;
}

/// ScrollEventUnit::Pixel — most apps treat it as smooth (continuous) scrolling
const SCROLL_UNIT_PIXEL: u32 = 0;
/// RDP wheel units per notch (WHEEL_DELTA)
const WHEEL_DELTA: i32 = 120;
/// Pixels per wheel notch; Chromium scrolls 40 px per Cocoa line tick
const PIXELS_PER_NOTCH: i32 = 40;
/// Max per-axis distance (px) between the presses of one multi-click
const CLICK_SLOP: u32 = 4;
/// NSEvent.doubleClickInterval default
const DEFAULT_DOUBLE_CLICK: Duration = Duration::from_millis(500);

pub struct MouseInjector {
    double_click: Duration,
    state: Mutex<MouseState>,
}

#[derive(Default)]
struct MouseState {
    /// Bitmask of held buttons (`MouseButton::bit`)
    held: u8,
    clicks: ClickTracker,
    vertical: ScrollAccumulator,
    horizontal: ScrollAccumulator,
}

impl MouseInjector {
    pub fn new() -> Result<Self> {
        let _ = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|_| anyhow::anyhow!("Failed to create CGEventSource — check Accessibility permission"))?;
        Ok(Self {
            double_click: system_double_click_interval(),
            state: Mutex::default(),
        })
    }

    fn source() -> Result<CGEventSource> {
        CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|_| anyhow::anyhow!("Failed to create CGEventSource"))
    }

    fn state(&self) -> MutexGuard<'_, MouseState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn post_mouse(
        event_type: CGEventType,
        button: MouseButton,
        x: i32,
        y: i32,
        click_state: Option<i64>,
    ) -> Result<()> {
        let point = CGPoint::new(f64::from(x), f64::from(y));
        let source = Self::source()?;
        let event = CGEvent::new_mouse_event(source, event_type, point, button.cg())
            .map_err(|_| anyhow::anyhow!("Failed to create mouse event"))?;
        event.set_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER, button.number());
        if let Some(count) = click_state {
            event.set_integer_value_field(EventField::MOUSE_EVENT_CLICK_STATE, count);
        }
        crate::post(&event);
        Ok(())
    }

    /// Move the pointer; posts `*MouseDragged` while a button is held
    pub fn move_to(&self, x: i32, y: i32) -> Result<()> {
        let (event_type, button) = move_event(self.state().held);
        Self::post_mouse(event_type, button, x, y, None)?;
        tracing::trace!(x, y, "Mouse moved");
        Ok(())
    }

    pub fn button_event(&self, button: MouseButton, pressed: bool, x: i32, y: i32) -> Result<()> {
        let click_state = {
            let mut s = self.state();
            if pressed {
                s.held |= button.bit();
                s.clicks.press(button, x, y, Instant::now(), self.double_click)
            } else {
                s.held &= !button.bit();
                s.clicks.release(button)
            }
        };
        Self::post_mouse(button_event_type(button, pressed), button, x, y, Some(click_state))?;
        tracing::trace!(?button, pressed, x, y, click_state, "Mouse button event");
        Ok(())
    }

    /// Vertical wheel in RDP units (120 per notch, positive = away from the user)
    pub fn scroll(&self, vertical: i16) -> Result<()> {
        let px = self.state().vertical.add(vertical);
        Self::post_scroll(px, 0)
    }

    /// Horizontal wheel in RDP units (120 per notch, positive = right)
    pub fn scroll_horizontal(&self, horizontal: i16) -> Result<()> {
        let px = self.state().horizontal.add(horizontal);
        // CG wheel2 is positive towards the left.
        Self::post_scroll(0, -px)
    }

    fn post_scroll(vertical_px: i32, horizontal_px: i32) -> Result<()> {
        if vertical_px == 0 && horizontal_px == 0 {
            return Ok(());
        }
        unsafe {
            let event_ref = CGEventCreateScrollWheelEvent2(
                std::ptr::null_mut(),
                SCROLL_UNIT_PIXEL,
                2,
                vertical_px,
                horizontal_px,
                0,
            );
            if event_ref.is_null() {
                return Err(anyhow::anyhow!("Failed to create scroll event"));
            }
            let event = CGEvent::from_ptr(event_ref);
            crate::post(&event);
        }
        tracing::trace!(vertical_px, horizontal_px, "Mouse scroll");
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    /// RDP X button 1 (back)
    X1,
    /// RDP X button 2 (forward)
    X2,
}

impl MouseButton {
    /// Precedence when picking the button of a drag event
    const ALL: [MouseButton; 5] = [Self::Left, Self::Right, Self::Middle, Self::X1, Self::X2];

    /// `kCGMouseEventButtonNumber`
    fn number(self) -> i64 {
        match self {
            Self::Left => 0,
            Self::Right => 1,
            Self::Middle => 2,
            Self::X1 => 3,
            Self::X2 => 4,
        }
    }

    fn bit(self) -> u8 {
        1 << self.number()
    }

    fn cg(self) -> CGMouseButton {
        match self {
            Self::Left => CGMouseButton::Left,
            Self::Right => CGMouseButton::Right,
            // Other buttons are told apart by kCGMouseEventButtonNumber.
            _ => CGMouseButton::Center,
        }
    }
}

fn button_event_type(button: MouseButton, pressed: bool) -> CGEventType {
    match (button, pressed) {
        (MouseButton::Left, true) => CGEventType::LeftMouseDown,
        (MouseButton::Left, false) => CGEventType::LeftMouseUp,
        (MouseButton::Right, true) => CGEventType::RightMouseDown,
        (MouseButton::Right, false) => CGEventType::RightMouseUp,
        (_, true) => CGEventType::OtherMouseDown,
        (_, false) => CGEventType::OtherMouseUp,
    }
}

/// Event type and button for a pointer move with `held` buttons down
fn move_event(held: u8) -> (CGEventType, MouseButton) {
    match MouseButton::ALL.into_iter().find(|b| held & b.bit() != 0) {
        None => (CGEventType::MouseMoved, MouseButton::Left),
        Some(MouseButton::Left) => (CGEventType::LeftMouseDragged, MouseButton::Left),
        Some(MouseButton::Right) => (CGEventType::RightMouseDragged, MouseButton::Right),
        Some(other) => (CGEventType::OtherMouseDragged, other),
    }
}

/// Click count (`kCGMouseEventClickState`) from the previous press
#[derive(Default)]
struct ClickTracker {
    last: Option<LastPress>,
}

#[derive(Clone, Copy)]
struct LastPress {
    button: MouseButton,
    at: Instant,
    x: i32,
    y: i32,
    count: i64,
}

impl ClickTracker {
    fn press(&mut self, button: MouseButton, x: i32, y: i32, now: Instant, interval: Duration) -> i64 {
        let count = match self.last {
            Some(l)
                if l.button == button
                    && now.saturating_duration_since(l.at) <= interval
                    && l.x.abs_diff(x) <= CLICK_SLOP
                    && l.y.abs_diff(y) <= CLICK_SLOP =>
            {
                l.count + 1
            }
            _ => 1,
        };
        self.last = Some(LastPress { button, at: now, x, y, count });
        count
    }

    /// A release carries the count of its press
    fn release(&self, button: MouseButton) -> i64 {
        match self.last {
            Some(l) if l.button == button => l.count,
            _ => 1,
        }
    }
}

/// Converts RDP wheel units to whole pixels, carrying the remainder
#[derive(Default)]
struct ScrollAccumulator {
    /// Leftover, in 1/WHEEL_DELTA px
    rem: i32,
}

impl ScrollAccumulator {
    fn add(&mut self, units: i16) -> i32 {
        let scaled = i32::from(units) * PIXELS_PER_NOTCH;
        // A direction change drops the leftover of the old direction.
        if scaled.signum() == -self.rem.signum() {
            self.rem = 0;
        }
        let total = self.rem + scaled;
        let px = total / WHEEL_DELTA;
        self.rem = total - px * WHEEL_DELTA;
        px
    }
}

fn double_click_from_pref(secs: Option<f64>) -> Duration {
    match secs {
        Some(s) if s.is_finite() && s > 0.0 && s <= 5.0 => Duration::from_secs_f64(s),
        _ => DEFAULT_DOUBLE_CLICK,
    }
}

/// The user's double-click speed (the preference behind NSEvent.doubleClickInterval)
fn system_double_click_interval() -> Duration {
    use core_foundation::base::{CFType, TCFType};
    use core_foundation::number::CFNumber;
    use core_foundation::string::CFString;
    use core_foundation_sys::preferences::{kCFPreferencesAnyApplication, CFPreferencesCopyAppValue};

    let key = CFString::from_static_string("com.apple.mouse.doubleClickThreshold");
    let secs = unsafe {
        let value = CFPreferencesCopyAppValue(key.as_concrete_TypeRef(), kCFPreferencesAnyApplication);
        if value.is_null() {
            None
        } else {
            CFType::wrap_under_create_rule(value)
                .downcast::<CFNumber>()
                .and_then(|n| n.to_f64())
        }
    };
    double_click_from_pref(secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DC: Duration = Duration::from_millis(500);

    #[test]
    fn x_buttons_map_to_other_mouse_numbers_3_and_4() {
        assert_eq!(MouseButton::X1.number(), 3);
        assert_eq!(MouseButton::X2.number(), 4);
        assert_eq!(button_event_type(MouseButton::X1, true) as u32, CGEventType::OtherMouseDown as u32);
        assert_eq!(button_event_type(MouseButton::X2, false) as u32, CGEventType::OtherMouseUp as u32);
        assert_eq!(button_event_type(MouseButton::Left, true) as u32, CGEventType::LeftMouseDown as u32);
        assert_eq!(button_event_type(MouseButton::Right, false) as u32, CGEventType::RightMouseUp as u32);
    }

    #[test]
    fn move_without_buttons_is_mouse_moved() {
        assert_eq!(move_event(0).0 as u32, CGEventType::MouseMoved as u32);
    }

    #[test]
    fn move_with_button_held_is_dragged() {
        let left = MouseButton::Left.bit();
        let right = MouseButton::Right.bit();
        let x2 = MouseButton::X2.bit();
        assert_eq!(move_event(left).0 as u32, CGEventType::LeftMouseDragged as u32);
        assert_eq!(move_event(right).0 as u32, CGEventType::RightMouseDragged as u32);
        let (ty, button) = move_event(x2);
        assert_eq!(ty as u32, CGEventType::OtherMouseDragged as u32);
        assert_eq!(button, MouseButton::X2);
        // Left wins when several are held.
        assert_eq!(move_event(left | right | x2).0 as u32, CGEventType::LeftMouseDragged as u32);
    }

    #[test]
    fn click_count_increments_within_interval_and_slop() {
        let mut t = ClickTracker::default();
        let t0 = Instant::now();
        assert_eq!(t.press(MouseButton::Left, 100, 100, t0, DC), 1);
        assert_eq!(t.release(MouseButton::Left), 1);
        assert_eq!(t.press(MouseButton::Left, 103, 96, t0 + Duration::from_millis(200), DC), 2);
        assert_eq!(t.release(MouseButton::Left), 2);
        assert_eq!(t.press(MouseButton::Left, 104, 100, t0 + Duration::from_millis(400), DC), 3);
    }

    #[test]
    fn click_count_resets_on_timeout_distance_or_other_button() {
        let mut t = ClickTracker::default();
        let t0 = Instant::now();
        t.press(MouseButton::Left, 10, 10, t0, DC);
        assert_eq!(t.press(MouseButton::Left, 10, 10, t0 + Duration::from_millis(501), DC), 1);
        assert_eq!(t.press(MouseButton::Left, 15, 10, t0 + Duration::from_millis(600), DC), 1);
        assert_eq!(t.press(MouseButton::Right, 15, 10, t0 + Duration::from_millis(700), DC), 1);
        assert_eq!(t.release(MouseButton::Left), 1);
    }

    #[test]
    fn scroll_notch_maps_to_pixels() {
        let mut acc = ScrollAccumulator::default();
        assert_eq!(acc.add(120), PIXELS_PER_NOTCH);
        assert_eq!(acc.add(-120), -PIXELS_PER_NOTCH);
    }

    #[test]
    fn scroll_carries_sub_pixel_remainder() {
        let mut acc = ScrollAccumulator::default();
        // 1 unit = 1/3 px at 40 px per 120-unit notch.
        assert_eq!(acc.add(1), 0);
        assert_eq!(acc.add(1), 0);
        assert_eq!(acc.add(1), 1);
        let total: i32 = (0..4).map(|_| acc.add(30)).sum();
        assert_eq!(total, PIXELS_PER_NOTCH);
    }

    #[test]
    fn scroll_direction_change_drops_remainder() {
        let mut acc = ScrollAccumulator::default();
        assert_eq!(acc.add(2), 0);
        assert_eq!(acc.add(-3), -1);
    }

    #[test]
    fn double_click_interval_falls_back_to_500ms() {
        assert_eq!(double_click_from_pref(None), DC);
        assert_eq!(double_click_from_pref(Some(0.0)), DC);
        assert_eq!(double_click_from_pref(Some(f64::NAN)), DC);
        assert_eq!(double_click_from_pref(Some(0.8)), Duration::from_millis(800));
    }
}
