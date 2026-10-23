//! Client-driven resolution: layout validation, debounce, aspect-fit geometry. Pure logic, no I/O.

use std::time::{Duration, Instant};

use ironrdp_server::DisplayControlMonitorLayout;

/// One device rotation fires several layout PDUs; rebuild once they stop.
pub const LAYOUT_DEBOUNCE: Duration = Duration::from_millis(150);

/// Clamp a desktop size to MS-RDPEDISP 2.2.2.2.1 bounds (200..=8192) and round down to even.
pub fn clamp_desktop_size(w: u32, h: u32) -> (u16, u16) {
    // Even sizes: H.264 4:2:0 chroma and exact HiDPI halving.
    let c = |v: u32| (v.clamp(200, 8192) & !1) as u16;
    (c(w), c(h))
}

/// Session size in pixels; `hidpi` renders it at 2x backing (logical = pixels / 2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TargetLayout {
    pub width: u16,
    pub height: u16,
    pub hidpi: bool,
}

impl TargetLayout {
    pub fn logical_size(&self) -> (u16, u16) {
        if self.hidpi { (self.width / 2, self.height / 2) } else { (self.width, self.height) }
    }
}

/// Target for a client layout: the primary monitor (else the first), validated.
pub fn layout_target(layout: &DisplayControlMonitorLayout) -> Option<TargetLayout> {
    let monitors = layout.monitors();
    let entry = monitors.iter().find(|m| m.is_primary()).or_else(|| monitors.first())?;
    let (w, h) = entry.dimensions();
    let (width, height) = clamp_desktop_size(w, h);
    let hidpi = entry.desktop_scale_factor().is_some_and(|s| s > 100);
    Some(TargetLayout { width, height, hidpi })
}

/// Largest `src`-aspect rect centered inside `dst`, as (x, y, w, h) in `dst` units (letterbox).
pub fn aspect_fit(src: (f64, f64), dst: (f64, f64)) -> (f64, f64, f64, f64) {
    let (sw, sh) = (src.0.max(1.0), src.1.max(1.0));
    // The constrained side takes the exact dst length.
    let (w, h) = if dst.0 * sh > dst.1 * sw {
        (dst.1 * sw / sh, dst.1)
    } else {
        (dst.0, dst.0 * sh / sw)
    };
    ((dst.0 - w) / 2.0, (dst.1 - h) / 2.0, w, h)
}

/// Keeps only the latest value and releases it once `delay` passes without a newer push.
pub struct Debounce<T> {
    delay: Duration,
    pending: Option<(T, Instant)>,
}

impl<T> Debounce<T> {
    pub fn new(delay: Duration) -> Self {
        Self { delay, pending: None }
    }

    pub fn push(&mut self, value: T, now: Instant) {
        self.pending = Some((value, now + self.delay));
    }

    pub fn due(&self) -> Option<Instant> {
        self.pending.as_ref().map(|(_, due)| *due)
    }

    pub fn take_if_due(&mut self, now: Instant) -> Option<T> {
        if self.due()? > now {
            return None;
        }
        self.pending.take().map(|(v, _)| v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironrdp_server::MonitorLayoutEntry;

    fn layout(entries: &[MonitorLayoutEntry]) -> DisplayControlMonitorLayout {
        DisplayControlMonitorLayout::new(entries).unwrap()
    }

    #[test]
    fn clamp_desktop_size_bounds_and_even() {
        assert_eq!(clamp_desktop_size(1, 1), (200, 200));
        assert_eq!(clamp_desktop_size(9000, 9000), (8192, 8192));
        assert_eq!(clamp_desktop_size(1921, 1081), (1920, 1080));
        assert_eq!(clamp_desktop_size(70_000, 65_537), (8192, 8192));
        assert_eq!(clamp_desktop_size(u32::MAX, 0), (8192, 200));
    }

    #[test]
    fn layout_target_primary_hidpi() {
        let l = layout(&[MonitorLayoutEntry::new_primary(2388, 1668).unwrap()
            .with_desktop_scale_factor(200).unwrap()]);
        let t = layout_target(&l).unwrap();
        assert_eq!(t, TargetLayout { width: 2388, height: 1668, hidpi: true });
        assert_eq!(t.logical_size(), (1194, 834));
    }

    #[test]
    fn layout_target_prefers_primary_then_first() {
        let l = layout(&[
            MonitorLayoutEntry::new_secondary(1280, 800).unwrap(),
            MonitorLayoutEntry::new_primary(1920, 1080).unwrap(),
        ]);
        assert_eq!(layout_target(&l).map(|t| (t.width, t.height)), Some((1920, 1080)));

        // The constructor insists on one primary; the wire decoder does not.
        let l = decode_layout(&[(1280, 800), (1920, 1080)]);
        assert_eq!(layout_target(&l).map(|t| (t.width, t.height)), Some((1280, 800)));

        assert_eq!(layout_target(&decode_layout(&[])), None);
    }

    /// Wire-format layout of non-primary monitors (MS-RDPEDISP 2.2.2.2).
    fn decode_layout(sizes: &[(u32, u32)]) -> DisplayControlMonitorLayout {
        let mut raw = Vec::new();
        for v in [40, sizes.len() as u32] {
            raw.extend_from_slice(&v.to_le_bytes());
        }
        for &(w, h) in sizes {
            for v in [0, 0, 0, w, h, 0, 0, 0, 0, 0] {
                raw.extend_from_slice(&v.to_le_bytes());
            }
        }
        ironrdp_pdu::decode(&raw).unwrap()
    }

    #[test]
    fn layout_target_scale_selects_hidpi() {
        let at = |scale: Option<u32>| {
            let mut e = MonitorLayoutEntry::new_primary(1920, 1080).unwrap();
            if let Some(s) = scale {
                e = e.with_desktop_scale_factor(s).unwrap();
            }
            layout_target(&layout(&[e])).unwrap().hidpi
        };
        assert!(!at(None));
        assert!(!at(Some(100)));
        assert!(at(Some(125)));
        assert!(at(Some(150)));
        assert!(at(Some(200)));
        let t = TargetLayout { width: 1920, height: 1080, hidpi: false };
        assert_eq!(t.logical_size(), (1920, 1080));
    }

    #[test]
    fn aspect_fit_pillarboxes_narrower_source() {
        let (x, y, w, h) = aspect_fit((1512.0, 982.0), (1920.0, 1080.0));
        assert!(x > 0.0);
        assert_eq!(y, 0.0);
        assert_eq!(h, 1080.0);
        assert!((w - 1080.0 * 1512.0 / 982.0).abs() < 0.5);
        assert!((x * 2.0 + w - 1920.0).abs() < 1e-9, "centered");
        assert_eq!(aspect_fit((1194.0, 834.0), (2388.0, 1668.0)), (0.0, 0.0, 2388.0, 1668.0));
    }

    #[test]
    fn debounce_releases_latest_after_quiet_period() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut d = Debounce::new(LAYOUT_DEBOUNCE);
        assert_eq!(d.due(), None);
        d.push('A', t0);
        d.push('B', t0 + ms(50));
        assert_eq!(d.due(), Some(t0 + ms(200)));
        assert_eq!(d.take_if_due(t0 + ms(100)), None);
        assert_eq!(d.take_if_due(t0 + ms(200)), Some('B'));
        assert_eq!(d.take_if_due(t0 + ms(300)), None);
        assert_eq!(d.due(), None);
    }

    #[test]
    fn resolve_resolution_is_bounded() {
        use crate::server::resolve_resolution;
        assert_eq!(resolve_resolution("99999x99999", 1512, 982), (8192, 8192));
        assert_eq!(resolve_resolution("0x0", 1512, 982), (200, 200));
        assert_eq!(resolve_resolution("1921x1081", 1512, 982), (1920, 1080));
        assert_eq!(resolve_resolution("auto", 60_000, 60_000), (8192, 8192));
        assert_eq!(resolve_resolution("4", 30_000, 30_000), (8192, 8192));
        assert_eq!(resolve_resolution("2", 1512, 982), (3024, 1964));
    }
}
