use ironrdp_pdu::input::fast_path::SynchronizeFlags;
use ironrdp_server::{KeyboardEvent, MouseEvent, RdpServerInputHandler};
use macrdp_input::{KeyboardInjector, MouseButton, MouseInjector};
use std::sync::{Arc, Mutex};

/// Button press/release carried by a mouse event
fn button_change(event: &MouseEvent) -> Option<(MouseButton, bool)> {
    Some(match event {
        MouseEvent::LeftPressed => (MouseButton::Left, true),
        MouseEvent::LeftReleased => (MouseButton::Left, false),
        MouseEvent::RightPressed => (MouseButton::Right, true),
        MouseEvent::RightReleased => (MouseButton::Right, false),
        MouseEvent::MiddlePressed => (MouseButton::Middle, true),
        MouseEvent::MiddleReleased => (MouseButton::Middle, false),
        MouseEvent::Button4Pressed => (MouseButton::X1, true),
        MouseEvent::Button4Released => (MouseButton::X1, false),
        MouseEvent::Button5Pressed => (MouseButton::X2, true),
        MouseEvent::Button5Released => (MouseButton::X2, false),
        _ => return None,
    })
}

/// Maps RDP desktop coordinates to macOS global coordinates (CG points, what CGEvent expects).
///
/// The captured display (global bounds) is shown aspect-fit inside the RDP desktop, the same
/// letterbox ScreenCaptureKit draws; with a virtual display both aspects match and the
/// content rect is the whole desktop. Points in a bar clamp to the content edge.
#[derive(Debug, Clone)]
pub struct MouseCoordMapper(Arc<Mutex<CoordState>>);

#[derive(Debug)]
struct CoordState {
    rdp_w: f64,
    rdp_h: f64,
    /// Captured display bounds: x, y, w, h in global points.
    bounds: (f64, f64, f64, f64),
}

impl MouseCoordMapper {
    pub fn new(logical_w: u16, logical_h: u16, rdp_w: u16, rdp_h: u16) -> Self {
        tracing::info!(
            logical_w, logical_h, rdp_w, rdp_h,
            "MouseCoordMapper initialized"
        );
        let m = Self(Arc::new(Mutex::new(CoordState { rdp_w: 1.0, rdp_h: 1.0, bounds: (0.0, 0.0, 1.0, 1.0) })));
        m.set_geometry(rdp_w, rdp_h, (0.0, 0.0, f64::from(logical_w), f64::from(logical_h)));
        m
    }

    /// Global CoreGraphics point (may be negative: a display left of or above the main one).
    pub fn map(&self, rdp_x: u16, rdp_y: u16) -> (i32, i32) {
        let s = self.0.lock().unwrap();
        let (bx, by, bw, bh) = s.bounds;
        let (cx, cy, cw, ch) = crate::resolution::aspect_fit((bw, bh), (s.rdp_w, s.rdp_h));
        let axis = |v: u16, c: f64, len: f64, origin: f64, size: f64| {
            let t = ((f64::from(v) - c) / len).clamp(0.0, 1.0);
            // The last point inside the bounds is origin + size - 1.
            (origin + t * size).min(origin + size - 1.0).max(origin) as i32
        };
        (axis(rdp_x, cx, cw, bx, bw), axis(rdp_y, cy, ch, by, bh))
    }

    /// RDP desktop size and the captured display's global bounds (points).
    pub fn set_geometry(&self, rdp_w: u16, rdp_h: u16, bounds: (f64, f64, f64, f64)) {
        let mut s = self.0.lock().unwrap();
        s.rdp_w = f64::from(rdp_w.max(1));
        s.rdp_h = f64::from(rdp_h.max(1));
        s.bounds = (bounds.0, bounds.1, bounds.2.max(1.0), bounds.3.max(1.0));
        tracing::info!(rdp_w, rdp_h, ?bounds, "MouseCoordMapper: geometry updated");
    }
}

/// Bridges RDP input events to macOS CGEvent injection
pub struct MacInputHandler {
    keyboard: Option<KeyboardInjector>,
    mouse: Option<MouseInjector>,
    last_mouse_x: i32,
    last_mouse_y: i32,
    coord_mapper: MouseCoordMapper,
}

impl MacInputHandler {
    pub fn new(coord_mapper: MouseCoordMapper) -> Self {
        let keyboard = KeyboardInjector::new()
            .map_err(|e| tracing::error!("Failed to create keyboard injector: {e}"))
            .ok();
        let mouse = MouseInjector::new()
            .map_err(|e| tracing::error!("Failed to create mouse injector: {e}"))
            .ok();

        if keyboard.is_none() || mouse.is_none() {
            tracing::warn!(
                "Input injection may fail — ensure Accessibility permission is granted"
            );
        }

        Self {
            keyboard,
            mouse,
            last_mouse_x: 0,
            last_mouse_y: 0,
            coord_mapper,
        }
    }
}

impl RdpServerInputHandler for MacInputHandler {
    fn keyboard(&mut self, event: KeyboardEvent) {
        let Some(kb) = &self.keyboard else { return };

        let result = match event {
            KeyboardEvent::Pressed { code, extended } => kb.inject_key(code, extended, true),
            KeyboardEvent::Released { code, extended } => kb.inject_key(code, extended, false),
            KeyboardEvent::UnicodePressed(ch) => kb.inject_unicode(ch, true),
            KeyboardEvent::UnicodeReleased(ch) => kb.inject_unicode(ch, false),
            // macOS has no num/scroll/kana lock; only caps lock applies.
            KeyboardEvent::Synchronize(flags) => {
                kb.sync_caps_lock(flags.contains(SynchronizeFlags::CAPS_LOCK))
            }
        };

        if let Err(e) = result {
            tracing::warn!("Keyboard injection failed: {e}");
        }
    }

    fn mouse(&mut self, event: MouseEvent) {
        let Some(m) = &self.mouse else { return };

        let result = match event {
            MouseEvent::Move { x, y } => {
                let (mx, my) = self.coord_mapper.map(x, y);
                self.last_mouse_x = mx;
                self.last_mouse_y = my;
                m.move_to(mx, my)
            }
            MouseEvent::VerticalScroll { value } => m.scroll(value),
            MouseEvent::HorizontalScroll { value } => m.scroll_horizontal(value),
            event => match button_change(&event) {
                Some((button, pressed)) => {
                    m.button_event(button, pressed, self.last_mouse_x, self.last_mouse_y)
                }
                None => {
                    tracing::trace!(?event, "Unhandled mouse event");
                    Ok(())
                }
            },
        };

        if let Err(e) = result {
            tracing::warn!("Mouse injection failed: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironrdp_pdu::input::{MousePdu, MouseXPdu};

    /// Slow-path/fast-path pointer PDU from its raw flags, at (0, 0)
    fn mouse(flags: u16) -> MouseEvent {
        let mut raw = flags.to_le_bytes().to_vec();
        raw.extend_from_slice(&[0; 4]);
        ironrdp_pdu::decode::<MousePdu>(&raw).unwrap().into()
    }

    fn mouse_x(flags: u16) -> MouseEvent {
        let mut raw = flags.to_le_bytes().to_vec();
        raw.extend_from_slice(&[0; 4]);
        ironrdp_pdu::decode::<MouseXPdu>(&raw).unwrap().into()
    }

    #[test]
    fn wheel_rotation_is_nine_bit_twos_complement() {
        // PTR_FLAGS_WHEEL | 0x78: one notch away from the user.
        assert!(matches!(mouse(0x0278), MouseEvent::VerticalScroll { value: 120 }));
        // PTR_FLAGS_WHEEL | PTR_FLAGS_WHEEL_NEGATIVE | 0x88: one notch towards the user.
        assert!(matches!(mouse(0x0388), MouseEvent::VerticalScroll { value: -120 }));
    }

    #[test]
    fn horizontal_wheel_is_passed_through() {
        assert!(matches!(mouse(0x0478), MouseEvent::HorizontalScroll { value: 120 }));
        assert!(matches!(mouse(0x0588), MouseEvent::HorizontalScroll { value: -120 }));
    }

    #[test]
    fn middle_button_is_mapped() {
        assert!(matches!(mouse(0xc000), MouseEvent::MiddlePressed));
        assert!(matches!(mouse(0x4000), MouseEvent::MiddleReleased));
    }

    #[test]
    fn extended_buttons_are_x_buttons() {
        assert!(matches!(mouse_x(0x8001), MouseEvent::Button4Pressed));
        assert!(matches!(mouse_x(0x0002), MouseEvent::Button5Released));
    }

    #[test]
    fn mapper_letterbox_maps_content_and_clamps_bars() {
        let m = MouseCoordMapper::new(1, 1, 1, 1);
        m.set_geometry(1920, 1080, (0.0, 0.0, 1512.0, 982.0));
        assert_eq!(m.map(960, 540), (756, 491));
        assert_eq!(m.map(10, 540).0, 0, "left bar clamps to the content edge");
        let content_right: f64 = 960.0 + 1080.0 * 1512.0 / 982.0 / 2.0;
        assert_eq!(m.map(content_right.ceil() as u16, 540).0, 1511);
        assert_eq!(m.map(1919, 540).0, 1511, "right bar clamps");
    }

    #[test]
    fn mapper_offsets_into_display_bounds() {
        let m = MouseCoordMapper::new(1, 1, 1, 1);
        m.set_geometry(2388, 1668, (100.0, 50.0, 1194.0, 834.0));
        assert_eq!(m.map(0, 0), (100, 50));
        assert_eq!(m.map(2388, 1668), (1293, 883));
        assert_eq!(m.map(1194, 834), (697, 467));
    }

    #[test]
    fn mapper_reaches_a_display_left_of_the_main_one() {
        // macOS placed the session display at x = -836 (live log 2026-10-23T21:13:08Z).
        let m = MouseCoordMapper::new(1, 1, 1, 1);
        m.set_geometry(836, 1580, (-836.0, 0.0, 836.0, 1580.0));
        assert_eq!(m.map(0, 0), (-836, 0));
        assert_eq!(m.map(418, 790), (-418, 790));
    }

    #[test]
    fn mapper_new_keeps_proportional_mapping() {
        let m = MouseCoordMapper::new(1512, 982, 3024, 1964);
        assert_eq!(m.map(1512, 982), (756, 491));
    }

    #[test]
    fn x_buttons_map_to_input_buttons() {
        assert_eq!(button_change(&MouseEvent::Button4Pressed), Some((MouseButton::X1, true)));
        assert_eq!(button_change(&MouseEvent::Button5Released), Some((MouseButton::X2, false)));
        assert_eq!(button_change(&MouseEvent::MiddlePressed), Some((MouseButton::Middle, true)));
        assert_eq!(button_change(&MouseEvent::Move { x: 1, y: 1 }), None);
    }
}
