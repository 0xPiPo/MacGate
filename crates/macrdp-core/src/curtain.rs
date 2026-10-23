//! Curtain continuity across a session replace, and teardown order at session end.
//!
//! A curtain session's screen (local input block, then the virtual display that owns the
//! displays-off configuration) lives on its pipeline thread. When a new authenticated connection
//! replaces the session, that thread keeps its screen until the next session's display has taken
//! the configuration over, so the physical displays never come back on in between. When a
//! session ends for good, its guard waits for every pipeline to tear its screen down (tap
//! removed, displays restored, virtual display destroyed) before locking the Mac.

use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

/// The process's one active session screen.
pub(crate) static SCREEN: LazyLock<Arc<Handover>> = LazyLock::new(|| Arc::new(Handover::new()));

/// Longest a replaced session keeps its curtain for a successor that never shows a display.
pub(crate) const HOLD_LIMIT: Duration = Duration::from_secs(30);
/// Longest a session end waits for pipelines to restore the displays before locking.
pub(crate) const TEARDOWN_WAIT: Duration = Duration::from_secs(5);

pub(crate) struct Handover {
    state: Mutex<State>,
    changed: Condvar,
}

struct State {
    /// Pipeline threads that may still hold a screen.
    live: u32,
    /// The active session was replaced by a new connection.
    replacing: bool,
    /// A replaced pipeline keeps its screen for the next session.
    parked: bool,
}

impl Handover {
    pub(crate) const fn new() -> Self {
        Self { state: Mutex::new(State { live: 0, replacing: false, parked: false }), changed: Condvar::new() }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        // Plain counters and flags: a panic elsewhere cannot leave them inconsistent.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A pipeline thread started; it counts until the token drops (unwinding included).
    pub(crate) fn pipeline_started(self: &Arc<Self>) -> Live {
        self.state().live += 1;
        Live(Arc::clone(self))
    }

    /// A new authenticated connection takes over the active session.
    pub(crate) fn replaced(&self) {
        self.state().replacing = true;
    }

    /// At a pipeline's end: true when its curtain must stay up for the replacing session (then
    /// call [`Self::wait_released`] before tearing it down). Consumes the replace either way.
    pub(crate) fn park_if_replaced(&self, curtain: bool) -> bool {
        let mut st = self.state();
        st.parked = std::mem::take(&mut st.replacing) && curtain;
        st.parked
    }

    /// Hold until the next session's display took over, the session ended, or `bound` passed.
    pub(crate) fn wait_released(&self, bound: Duration) {
        let (mut st, wait) = self
            .changed
            .wait_timeout_while(self.state(), bound, |s| s.parked)
            .unwrap_or_else(PoisonError::into_inner);
        if wait.timed_out() {
            tracing::warn!("No new session display took over the curtain; restoring the displays");
            st.parked = false;
        }
    }

    /// The next session's display is configured (or failed): a kept screen may go.
    pub(crate) fn release(&self) {
        self.state().parked = false;
        self.changed.notify_all();
    }

    /// The session ended for good: release any kept screen and wait until every pipeline has
    /// torn its screen down.
    pub(crate) fn end_session(&self, bound: Duration) {
        let mut st = self.state();
        st.replacing = false;
        st.parked = false;
        self.changed.notify_all();
        let (_st, wait) = self
            .changed
            .wait_timeout_while(st, bound, |s| s.live > 0)
            .unwrap_or_else(PoisonError::into_inner);
        if wait.timed_out() {
            tracing::warn!("Display pipeline still running at session end; continuing");
        }
    }
}

/// Counts a running pipeline thread.
pub(crate) struct Live(Arc<Handover>);

impl Drop for Live {
    fn drop(&mut self) {
        let mut st = self.0.state();
        st.live = st.live.saturating_sub(1);
        self.0.changed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use super::*;

    type Log = Arc<Mutex<Vec<&'static str>>>;

    /// A pipeline thread at its end: keeps its screen while held, then tears it down.
    fn pipeline(h: &Arc<Handover>, log: &Log, curtain: bool) -> std::thread::JoinHandle<()> {
        let (h, log) = (Arc::clone(h), Arc::clone(log));
        let live = h.pipeline_started();
        std::thread::spawn(move || {
            let _live = live;
            if h.park_if_replaced(curtain) {
                h.wait_released(Duration::from_secs(5));
            }
            log.lock().unwrap().push("screen down");
        })
    }

    #[test]
    fn a_replaced_curtain_stays_up_until_the_next_display_takes_over() {
        let (h, log) = (Arc::new(Handover::new()), Log::default());
        h.replaced();
        let t = pipeline(&h, &log, true);
        std::thread::sleep(Duration::from_millis(50));
        assert!(log.lock().unwrap().is_empty(), "the physical displays would flash on");
        h.release();
        t.join().unwrap();
        assert_eq!(*log.lock().unwrap(), ["screen down"]);
    }

    #[test]
    fn an_ended_session_tears_the_screen_down_before_the_lock() {
        let (h, log) = (Arc::new(Handover::new()), Log::default());
        h.replaced();
        let t = pipeline(&h, &log, true);
        // The replacing session ended without taking over.
        h.end_session(Duration::from_secs(5));
        log.lock().unwrap().push("lock");
        t.join().unwrap();
        assert_eq!(*log.lock().unwrap(), ["screen down", "lock"]);
    }

    #[test]
    fn only_a_replaced_curtain_is_kept() {
        let (h, log) = (Arc::new(Handover::new()), Log::default());
        pipeline(&h, &log, true).join().unwrap();
        h.replaced();
        pipeline(&h, &log, false).join().unwrap();
        assert_eq!(*log.lock().unwrap(), ["screen down", "screen down"]);
        // The replace was consumed: the next curtain session is not held either.
        pipeline(&h, &log, true).join().unwrap();
        assert_eq!(log.lock().unwrap().len(), 3);
    }

    #[test]
    fn a_held_screen_is_bounded() {
        let h = Handover::new();
        h.replaced();
        assert!(h.park_if_replaced(true));
        let t0 = Instant::now();
        h.wait_released(Duration::from_millis(50));
        assert!(t0.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn end_session_returns_at_once_without_a_pipeline() {
        let t0 = Instant::now();
        Handover::new().end_session(Duration::from_secs(5));
        assert!(t0.elapsed() < Duration::from_millis(500));
    }
}
