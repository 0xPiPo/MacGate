//! Auto RemoteFX quality (config `rfx_quality = "auto"`): steps the quant level with the
//! connection. RemoteFX has no reference chain, so a new level applies from the next frame.

use std::fmt;
use std::time::{Duration, Instant};

use ironrdp_server::gfx::RfxQuality;

/// Backlog must last this long before it counts: one late ack is not congestion.
const BACKLOG_SUSTAIN: Duration = Duration::from_millis(300);
/// At most one step down per second, so a level gets a chance to drain the queue.
const DOWN_COOLDOWN: Duration = Duration::from_secs(1);
/// A clear link this long earns one step up; any congestion sign restarts the wait.
const UP_AFTER_CLEAR: Duration = Duration::from_secs(10);
/// RTT counts as congested above twice the session minimum and at least this much over it.
const RTT_MARGIN_MS: f64 = 30.0;

/// The connection as one RemoteFX frame is about to be sent.
#[derive(Debug, Clone, Copy, Default)]
pub struct RfxSample {
    /// Unacked frames beyond the path's baseline in flight (`GfxState::backlog`).
    pub backlog: u32,
    /// Captures the ack gate dropped (coalesced) since the last sent frame.
    pub coalesced: u32,
    /// Smoothed RTT in ms (0 = none yet).
    pub rtt_ms: f64,
    /// Lowest RTT this session in ms (0 = none yet).
    pub rtt_min_ms: f64,
}

/// Why the level changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    Backlog,
    Coalesced,
    Rtt,
    Clear,
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Backlog => "backlog",
            Self::Coalesced => "frames coalesced",
            Self::Rtt => "RTT",
            Self::Clear => "clear",
        })
    }
}

/// A level change, for the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Change {
    pub from: RfxQuality,
    pub to: RfxQuality,
    pub reason: Reason,
}

/// Starts at medium. Congestion (backlog for 300 ms, coalesced frames, or RTT over twice the
/// minimum and 30 ms over it) steps down one level, at most once a second; 10 s without any
/// sign of congestion steps up one level. The slow climb is the hysteresis against flapping.
pub struct RfxQualityController {
    level: RfxQuality,
    backlog_since: Option<Instant>,
    clear_since: Option<Instant>,
    last_down: Option<Instant>,
}

impl Default for RfxQualityController {
    fn default() -> Self {
        Self::new()
    }
}

impl RfxQualityController {
    pub fn new() -> Self {
        Self { level: RfxQuality::Medium, backlog_since: None, clear_since: None, last_down: None }
    }

    pub fn level(&self) -> RfxQuality {
        self.level
    }

    /// Feed one sample taken at `now`; returns the change if the level moved.
    pub fn observe(&mut self, s: RfxSample, now: Instant) -> Option<Change> {
        let backlog_for = if s.backlog > 0 {
            now.saturating_duration_since(*self.backlog_since.get_or_insert(now))
        } else {
            self.backlog_since = None;
            Duration::ZERO
        };
        let rtt_high = s.rtt_min_ms > 0.0 && s.rtt_ms > 2.0 * s.rtt_min_ms && s.rtt_ms > s.rtt_min_ms + RTT_MARGIN_MS;
        let congestion = if backlog_for >= BACKLOG_SUSTAIN {
            Some(Reason::Backlog)
        } else if s.coalesced > 0 {
            Some(Reason::Coalesced)
        } else if rtt_high {
            Some(Reason::Rtt)
        } else {
            None
        };

        if congestion.is_some() || s.backlog > 0 {
            self.clear_since = None;
        }
        if let Some(reason) = congestion {
            let cooled = self.last_down.is_none_or(|t| now.saturating_duration_since(t) >= DOWN_COOLDOWN);
            let to = match self.level {
                RfxQuality::High => RfxQuality::Medium,
                _ => RfxQuality::Low,
            };
            if !cooled || to == self.level {
                return None;
            }
            self.last_down = Some(now);
            return Some(self.step(to, reason));
        }
        // Backlog too short to count holds the level: not congested, not clear either.
        if s.backlog > 0 {
            return None;
        }

        let since = *self.clear_since.get_or_insert(now);
        let to = match self.level {
            RfxQuality::Low => RfxQuality::Medium,
            _ => RfxQuality::High,
        };
        if to == self.level || now.saturating_duration_since(since) < UP_AFTER_CLEAR {
            return None;
        }
        self.clear_since = Some(now);
        Some(self.step(to, Reason::Clear))
    }

    fn step(&mut self, to: RfxQuality, reason: Reason) -> Change {
        let from = std::mem::replace(&mut self.level, to);
        Change { from, to, reason }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use RfxQuality::{High, Low, Medium};

    const CLEAR: RfxSample = RfxSample { backlog: 0, coalesced: 0, rtt_ms: 20.0, rtt_min_ms: 20.0 };
    const BACKLOG: RfxSample = RfxSample { backlog: 1, ..CLEAR };
    const COALESCED: RfxSample = RfxSample { coalesced: 1, ..CLEAR };

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// Feeds `s` every 50 ms over `[from, to)` ms after `t0`; returns every change with its time.
    fn feed(c: &mut RfxQualityController, t0: Instant, s: RfxSample, from: u64, to: u64) -> Vec<(u64, Change)> {
        (from..to).step_by(50).filter_map(|t| c.observe(s, t0 + ms(t)).map(|ch| (t, ch))).collect()
    }

    fn levels(changes: &[(u64, Change)]) -> Vec<(u64, RfxQuality)> {
        changes.iter().map(|(t, c)| (*t, c.to)).collect()
    }

    #[test]
    fn starts_at_medium() {
        assert_eq!(RfxQualityController::new().level(), Medium);
    }

    #[test]
    fn backlog_steps_down_only_once_sustained_300ms() {
        let (mut c, t0) = (RfxQualityController::new(), Instant::now());
        let changes = feed(&mut c, t0, BACKLOG, 0, 400);
        assert_eq!(changes, [(300, Change { from: Medium, to: Low, reason: Reason::Backlog })]);
    }

    #[test]
    fn backlog_blip_shorter_than_300ms_holds() {
        let (mut c, t0) = (RfxQualityController::new(), Instant::now());
        for round in 0..10 {
            let base = round * 300;
            assert!(feed(&mut c, t0, BACKLOG, base, base + 250).is_empty());
            assert!(feed(&mut c, t0, CLEAR, base + 250, base + 300).is_empty());
        }
        assert_eq!(c.level(), Medium);
    }

    #[test]
    fn coalesced_frames_step_down_at_once() {
        let (mut c, t0) = (RfxQualityController::new(), Instant::now());
        assert_eq!(c.observe(COALESCED, t0), Some(Change { from: Medium, to: Low, reason: Reason::Coalesced }));
    }

    #[test]
    fn rtt_steps_down_only_over_twice_min_and_30ms_over() {
        let t0 = Instant::now();
        let rtt = |rtt_ms, rtt_min_ms| RfxSample { rtt_ms, rtt_min_ms, ..CLEAR };
        // 2x but only 20 ms over (LAN jitter); 35 ms over but under 2x (long path).
        for s in [rtt(40.0, 20.0), rtt(135.0, 100.0), rtt(50.0, 0.0)] {
            let mut c = RfxQualityController::new();
            assert_eq!(c.observe(s, t0), None, "{s:?}");
        }
        let mut c = RfxQualityController::new();
        assert_eq!(c.observe(rtt(51.0, 20.0), t0), Some(Change { from: Medium, to: Low, reason: Reason::Rtt }));
    }

    #[test]
    fn steps_down_at_most_once_per_second() {
        let (mut c, t0) = (RfxQualityController::new(), Instant::now());
        // Clear for 10 s to reach high, then congestion every 50 ms.
        assert_eq!(levels(&feed(&mut c, t0, CLEAR, 0, 10_050)), [(10_000, High)]);
        let changes = feed(&mut c, t0, COALESCED, 20_000, 23_000);
        assert_eq!(levels(&changes), [(20_000, Medium), (21_000, Low)]);
    }

    #[test]
    fn low_is_the_floor_and_high_the_ceiling() {
        let (mut c, t0) = (RfxQualityController::new(), Instant::now());
        c.observe(COALESCED, t0);
        assert!(feed(&mut c, t0, COALESCED, 1_000, 5_000).is_empty());
        assert_eq!(c.level(), Low);
        assert_eq!(levels(&feed(&mut c, t0, CLEAR, 5_000, 40_000)), [(15_000, Medium), (25_000, High)]);
    }

    #[test]
    fn steps_up_one_level_after_10s_clear() {
        let (mut c, t0) = (RfxQualityController::new(), Instant::now());
        c.observe(COALESCED, t0);
        assert!(feed(&mut c, t0, CLEAR, 50, 10_050).is_empty(), "the congested sample restarts the wait");
        assert_eq!(
            feed(&mut c, t0, CLEAR, 10_050, 10_100),
            [(10_050, Change { from: Low, to: Medium, reason: Reason::Clear })]
        );
    }

    #[test]
    fn short_backlog_restarts_the_clear_wait_without_stepping_down() {
        let (mut c, t0) = (RfxQualityController::new(), Instant::now());
        assert!(feed(&mut c, t0, CLEAR, 0, 9_000).is_empty());
        assert!(feed(&mut c, t0, BACKLOG, 9_000, 9_100).is_empty());
        assert!(feed(&mut c, t0, CLEAR, 9_100, 19_100).is_empty());
        assert_eq!(levels(&feed(&mut c, t0, CLEAR, 19_100, 19_150)), [(19_100, High)]);
    }

    #[test]
    fn congestion_every_few_seconds_never_flaps_up() {
        let (mut c, t0) = (RfxQualityController::new(), Instant::now());
        let mut changes = Vec::new();
        for round in 0..10 {
            let base = round * 5_000;
            changes.extend(feed(&mut c, t0, COALESCED, base, base + 50));
            changes.extend(feed(&mut c, t0, CLEAR, base + 50, base + 5_000));
        }
        assert_eq!(levels(&changes), [(0, Low)]);
    }
}
