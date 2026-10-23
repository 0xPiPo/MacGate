use std::time::{Duration, Instant};

/// Per-frame statistics for BitrateController evaluation.
#[derive(Debug, Clone, Copy)]
pub struct FrameStats {
    /// GFX frames unacknowledged when this frame was sent, beyond the frames a
    /// minimum-RTT path holds in flight (`GfxState::backlog`): queueing, not distance.
    pub pending_acks: u32,
    /// Captures dropped (coalesced to a newer one) while waiting to send this frame.
    pub coalesced: u32,
    pub frame_bytes: u32,
    pub is_keyframe: bool,
}

/// Decision output from BitrateController.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdaptiveDecision {
    pub bitrate_bps: u32,
    pub fps: u32,
}

const FPS_TIERS: [f32; 3] = [1.0, 0.5, 0.25];

/// Congestion-driven rate control: reacts only to ack backlog and coalescing.
/// Encode time and RTT are deliberately not inputs (they rise on a healthy link too);
/// the backlog already excludes the in-flight frames of a long but uncongested path.
pub struct BitrateController {
    initial_bitrate: u32,
    current_bitrate: u32,
    target_fps: u32,
    current_fps_tier: usize,
    eval_window: Vec<FrameStats>,
    last_eval_time: Instant,
    eval_interval: Duration,
}

fn scale_pct(v: u32, pct: u64) -> u32 {
    (u64::from(v) * pct / 100) as u32
}

impl BitrateController {
    pub fn new(initial_bitrate: u32, target_fps: u32) -> Self {
        Self {
            initial_bitrate,
            current_bitrate: initial_bitrate,
            target_fps,
            current_fps_tier: 0,
            eval_window: Vec::with_capacity(128),
            last_eval_time: Instant::now(),
            eval_interval: Duration::from_secs(1),
        }
    }

    pub fn record_frame(&mut self, stats: FrameStats) { self.eval_window.push(stats); }
    pub fn current_bitrate(&self) -> u32 { self.current_bitrate }
    pub fn current_fps(&self) -> u32 { (self.target_fps as f32 * FPS_TIERS[self.current_fps_tier]) as u32 }
    pub fn target_fps(&self) -> u32 { self.target_fps }

    pub fn on_idle_recovery(&mut self) {
        self.eval_window.clear();
        self.current_bitrate = self.initial_bitrate;
        self.current_fps_tier = 0;
        self.last_eval_time = Instant::now();
    }

    /// Congested (avg pending >= 2, or >= 10% coalesced): bitrate -15% down to 30% of
    /// initial, then step fps down. Clear (avg pending < 1, no coalescing): restore fps
    /// first, then bitrate +10% up to initial. Otherwise hold.
    pub fn evaluate(&mut self) -> AdaptiveDecision {
        let n = self.eval_window.len() as u64;
        if n > 0 {
            let pending: u64 = self.eval_window.iter().map(|s| u64::from(s.pending_acks)).sum();
            let coalesced: u64 = self.eval_window.iter().map(|s| u64::from(s.coalesced)).sum();
            let floor = scale_pct(self.initial_bitrate, 30);

            if pending >= 2 * n || coalesced * 10 >= n {
                if self.current_bitrate > floor {
                    self.current_bitrate = scale_pct(self.current_bitrate, 85).max(floor);
                } else if self.current_fps_tier < FPS_TIERS.len() - 1 {
                    self.current_fps_tier += 1;
                }
            } else if pending < n && coalesced == 0 {
                if self.current_fps_tier > 0 {
                    self.current_fps_tier -= 1;
                } else {
                    self.current_bitrate = scale_pct(self.current_bitrate, 110).min(self.initial_bitrate);
                }
            }
        }

        self.eval_window.clear();
        self.last_eval_time = Instant::now();
        AdaptiveDecision { bitrate_bps: self.current_bitrate, fps: self.current_fps() }
    }

    pub fn should_evaluate(&self) -> bool {
        self.last_eval_time.elapsed() >= self.eval_interval && !self.eval_window.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INITIAL: u32 = 10_000_000;
    const FLOOR: u32 = 3_000_000;

    fn ctrl() -> BitrateController {
        BitrateController::new(INITIAL, 60)
    }

    /// Record 40 frames sent with `pending` unacked frames; `coalesced` captures dropped in total.
    fn window(c: &mut BitrateController, pending: u32, coalesced: u32) -> AdaptiveDecision {
        for i in 0..40 {
            c.record_frame(FrameStats {
                pending_acks: pending,
                coalesced: u32::from(i < coalesced),
                frame_bytes: 50_000,
                is_keyframe: false,
            });
        }
        c.evaluate()
    }

    fn drive_to_floor(c: &mut BitrateController) {
        for _ in 0..20 {
            if c.current_bitrate() == FLOOR {
                return;
            }
            window(c, 2, 0);
        }
        panic!("never reached floor");
    }

    #[test]
    fn backlog_lowers_bitrate_15_percent_per_evaluation() {
        let mut c = ctrl();
        assert_eq!(window(&mut c, 2, 0).bitrate_bps, 8_500_000);
        assert_eq!(window(&mut c, 3, 0).bitrate_bps, 7_225_000);
    }

    #[test]
    fn backlog_at_floor_steps_fps_down() {
        let mut c = ctrl();
        drive_to_floor(&mut c);
        assert_eq!(c.current_fps(), 60);
        let d = window(&mut c, 2, 0);
        assert_eq!(d, AdaptiveDecision { bitrate_bps: FLOOR, fps: 30 });
    }

    #[test]
    fn coalescing_counts_as_congestion() {
        let mut c = ctrl();
        assert_eq!(window(&mut c, 0, 4).bitrate_bps, 8_500_000);
    }

    #[test]
    fn some_backlog_or_coalescing_below_threshold_holds() {
        let mut c = ctrl();
        assert_eq!(window(&mut c, 2, 0).bitrate_bps, 8_500_000);
        assert_eq!(window(&mut c, 1, 0).bitrate_bps, 8_500_000);
        assert_eq!(window(&mut c, 0, 3).bitrate_bps, 8_500_000);
    }

    #[test]
    fn clear_restores_fps_first_then_bitrate_up_to_initial() {
        let mut c = ctrl();
        drive_to_floor(&mut c);
        window(&mut c, 2, 0);
        assert_eq!(c.current_fps(), 30);

        let d = window(&mut c, 0, 0);
        assert_eq!(d, AdaptiveDecision { bitrate_bps: FLOOR, fps: 60 });
        assert_eq!(window(&mut c, 0, 0).bitrate_bps, 3_300_000);
        for _ in 0..30 {
            window(&mut c, 0, 0);
        }
        assert_eq!(c.current_bitrate(), INITIAL);
    }

    #[test]
    fn clear_at_initial_changes_nothing() {
        let mut c = ctrl();
        let d = window(&mut c, 0, 0);
        assert_eq!(d, AdaptiveDecision { bitrate_bps: INITIAL, fps: 60 });
    }

    #[test]
    fn idle_recovery_resets() {
        let mut c = ctrl();
        drive_to_floor(&mut c);
        window(&mut c, 2, 0);
        c.on_idle_recovery();
        assert_eq!(c.current_bitrate(), INITIAL);
        assert_eq!(c.current_fps(), 60);
    }
}
