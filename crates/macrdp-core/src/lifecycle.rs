//! The Mac side of a remote session: stay awake while it runs; when it ends, restore the
//! displays and local input (curtain), then lock.

use std::ffi::c_void;
use std::net::SocketAddr;

use ironrdp_server::{SessionGuard, SessionLifecycle};
use objc2_foundation::NSString;

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    // IOPMLib.h; CFStringRef arguments, IOPMAssertionID out.
    fn IOPMAssertionCreateWithName(kind: *const c_void, level: u32, name: *const c_void, id: *mut u32) -> i32;
    fn IOPMAssertionRelease(id: u32) -> i32;
    fn IOPMAssertionDeclareUserActivity(name: *const c_void, user_type: u32, id: *mut u32) -> i32;
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGMainDisplayID() -> u32;
    fn CGDisplayIsAsleep(display: u32) -> u32;
}

/// Wake sleeping displays before a session sets them up. A power assertion only stops sleep;
/// it does not wake a display that is already off, and an asleep display both fails display
/// configuration (CG error 1001) and yields no frames (live: client stuck on "Connecting").
pub fn wake_displays() {
    // SAFETY: plain CoreGraphics query.
    if unsafe { CGDisplayIsAsleep(CGMainDisplayID()) } == 0 {
        return;
    }
    let name = NSString::from_str(ASSERTION_NAME);
    let mut id = 0u32;
    // SAFETY: toll-free bridged CFString outlives the call; `id` is a valid out pointer.
    // kIOPMUserActiveLocal = 0: the same signal as touching the local keyboard.
    let rc = unsafe { IOPMAssertionDeclareUserActivity((&*name as *const NSString).cast(), 0, &mut id) };
    if rc != 0 {
        tracing::warn!(rc, "IOPMAssertionDeclareUserActivity failed; displays stay asleep");
        return;
    }
    let started = std::time::Instant::now();
    // SAFETY: plain CoreGraphics query.
    while unsafe { CGDisplayIsAsleep(CGMainDisplayID()) } != 0 && started.elapsed() < std::time::Duration::from_secs(3) {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    tracing::info!(waited_ms = started.elapsed().as_millis() as u64, "Woke the displays for the session");
    // SAFETY: `id` came from a successful IOPMAssertionDeclareUserActivity.
    unsafe { IOPMAssertionRelease(id) };
}

/// kIOPMAssertionLevelOn
const ASSERTION_LEVEL_ON: u32 = 255;
/// Keep the display on and the Mac out of idle sleep while someone works remotely.
const ASSERTION_TYPES: [&str; 2] = ["PreventUserIdleDisplaySleep", "PreventUserIdleSystemSleep"];
const ASSERTION_NAME: &str = "macrdp remote session";

/// IOKit power assertions, released on drop.
pub struct PowerAssertion {
    ids: Vec<u32>,
}

impl PowerAssertion {
    pub fn new() -> std::io::Result<Self> {
        let mut assertion = Self { ids: Vec::new() };
        let name = NSString::from_str(ASSERTION_NAME);
        for kind in ASSERTION_TYPES {
            let kind = NSString::from_str(kind);
            let mut id = 0u32;
            // SAFETY: NSString is toll-free bridged to CFStringRef and both strings outlive
            // the call; `id` is a valid out pointer.
            let rc = unsafe {
                IOPMAssertionCreateWithName(
                    (&*kind as *const NSString).cast(),
                    ASSERTION_LEVEL_ON,
                    (&*name as *const NSString).cast(),
                    &mut id,
                )
            };
            if rc != 0 {
                // `assertion` drops here and releases the ones already taken.
                return Err(std::io::Error::other(format!("IOPMAssertionCreateWithName({kind}) failed: {rc:#x}")));
            }
            assertion.ids.push(id);
        }
        Ok(assertion)
    }
}

impl Drop for PowerAssertion {
    fn drop(&mut self) {
        for id in self.ids.drain(..) {
            // SAFETY: `id` came from a successful IOPMAssertionCreateWithName.
            let rc = unsafe { IOPMAssertionRelease(id) };
            if rc != 0 {
                tracing::warn!(rc, "IOPMAssertionRelease failed");
            }
        }
    }
}

fn keep_awake() -> Option<PowerAssertion> {
    PowerAssertion::new()
        .map_err(|e| tracing::warn!("Cannot keep the Mac awake during the session: {e}"))
        .ok()
}

/// Locks the Mac's console session now.
///
/// No public API does this. Uses the private `SACLockScreenImmediate` from
/// `login.framework` (what the menu bar "Lock Screen" item calls), loaded with dlopen so a
/// missing symbol is handled. Falls back to `pmset displaysleepnow`, which locks only when
/// "Require password immediately" is on.
pub fn lock_screen() {
    tracing::info!("Locking the Mac (lock_on_disconnect)");
    const LOGIN_FRAMEWORK: &std::ffi::CStr = c"/System/Library/PrivateFrameworks/login.framework/Versions/Current/login";
    // SAFETY: dlopen/dlsym get NUL-terminated strings; the symbol, when present, has the
    // signature `int SACLockScreenImmediate(void)`. The handle is kept open on purpose.
    unsafe {
        let handle = libc::dlopen(LOGIN_FRAMEWORK.as_ptr(), libc::RTLD_LAZY);
        if !handle.is_null() {
            let symbol = libc::dlsym(handle, c"SACLockScreenImmediate".as_ptr());
            if !symbol.is_null() {
                let lock: extern "C" fn() -> i32 = std::mem::transmute(symbol);
                let rc = lock();
                tracing::debug!(rc, "SACLockScreenImmediate called");
                return;
            }
        }
    }
    tracing::warn!(
        "SACLockScreenImmediate unavailable; running `pmset displaysleepnow` (locks only when \"Require password immediately\" is on)"
    );
    if let Err(e) = std::process::Command::new("/usr/bin/pmset").arg("displaysleepnow").status() {
        tracing::error!("Cannot lock the Mac: {e}");
    }
}

/// Keeps the Mac awake during a session and, if enabled, locks it when the session ends, unless a
/// new authenticated connection replaced it. A replaced session hands its screen (curtain) over.
pub struct MacSessionLifecycle {
    /// Config `auto_unlock`: a session starting on the lock screen types the password.
    auto_unlock: bool,
    keep_awake: fn() -> Option<PowerAssertion>,
    lock: fn(),
    replaced: fn(),
    end_screen: fn(),
}

fn screen_replaced() {
    crate::curtain::SCREEN.replaced();
}

fn end_screen() {
    crate::curtain::SCREEN.end_session(crate::curtain::TEARDOWN_WAIT);
}

impl MacSessionLifecycle {
    /// Locking is off by default: the next client then has to type the password into the lock
    /// screen (or `auto_unlock` types it).
    pub fn new(lock_on_disconnect: bool, auto_unlock: bool) -> Self {
        let lifecycle =
            Self::with_hooks(keep_awake, if lock_on_disconnect { lock_screen } else { || {} }, screen_replaced, end_screen);
        Self { auto_unlock, ..lifecycle }
    }

    fn with_hooks(keep_awake: fn() -> Option<PowerAssertion>, lock: fn(), replaced: fn(), end_screen: fn()) -> Self {
        Self { auto_unlock: false, keep_awake, lock, replaced, end_screen }
    }
}

impl SessionLifecycle for MacSessionLifecycle {
    fn session_started(&self, peer: SocketAddr) -> Box<dyn SessionGuard> {
        // Arms or drops the password the authenticator just kept.
        let locked = self.auto_unlock && crate::unlock::screen_locked();
        crate::unlock::PENDING.session_started(self.auto_unlock, locked);
        Box::new(MacSessionGuard {
            peer,
            awake: (self.keep_awake)(),
            lock: self.lock,
            on_replaced: self.replaced,
            end_screen: self.end_screen,
            replaced: false,
        })
    }
}

struct MacSessionGuard {
    peer: SocketAddr,
    awake: Option<PowerAssertion>,
    lock: fn(),
    on_replaced: fn(),
    end_screen: fn(),
    replaced: bool,
}

impl SessionGuard for MacSessionGuard {
    fn replaced(&mut self) {
        self.replaced = true;
        (self.on_replaced)();
    }
}

impl Drop for MacSessionGuard {
    fn drop(&mut self) {
        drop(self.awake.take());
        if self.replaced {
            tracing::info!(peer = %self.peer, "Session replaced");
        } else {
            tracing::info!(peer = %self.peer, "Session ended");
            // Tap removed, displays back on, virtual display gone: only then lock.
            (self.end_screen)();
            (self.lock)();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    use super::*;

    fn peer() -> std::net::SocketAddr {
        "192.0.2.7:50000".parse().unwrap()
    }

    fn no_assertion() -> Option<PowerAssertion> {
        None
    }

    static ENDED_LOCKS: AtomicUsize = AtomicUsize::new(0);
    fn count_ended_lock() {
        ENDED_LOCKS.fetch_add(1, Ordering::SeqCst);
    }

    static REPLACED_LOCKS: AtomicUsize = AtomicUsize::new(0);
    fn count_replaced_lock() {
        REPLACED_LOCKS.fetch_add(1, Ordering::SeqCst);
    }

    fn noop() {}

    static ENDED: Mutex<Vec<&str>> = Mutex::new(Vec::new());
    fn ended_screen_down() {
        ENDED.lock().unwrap().push("screen down");
    }
    fn ended_lock() {
        ENDED.lock().unwrap().push("lock");
    }
    fn ended_replaced() {
        ENDED.lock().unwrap().push("replaced");
    }

    #[test]
    fn ended_session_restores_the_screen_before_locking() {
        let lifecycle = MacSessionLifecycle::with_hooks(no_assertion, ended_lock, ended_replaced, ended_screen_down);
        drop(lifecycle.session_started(peer()));
        assert_eq!(*ENDED.lock().unwrap(), ["screen down", "lock"]);
    }

    static HANDED: Mutex<Vec<&str>> = Mutex::new(Vec::new());
    fn handed_screen_down() {
        HANDED.lock().unwrap().push("screen down");
    }
    fn handed_lock() {
        HANDED.lock().unwrap().push("lock");
    }
    fn handed_replaced() {
        HANDED.lock().unwrap().push("replaced");
    }

    #[test]
    fn replaced_session_hands_the_screen_over() {
        let lifecycle = MacSessionLifecycle::with_hooks(no_assertion, handed_lock, handed_replaced, handed_screen_down);
        let mut guard = lifecycle.session_started(peer());
        guard.replaced();
        drop(guard);
        assert_eq!(*HANDED.lock().unwrap(), ["replaced"]);
    }

    #[test]
    fn ended_session_locks_the_mac() {
        let lifecycle = MacSessionLifecycle::with_hooks(no_assertion, count_ended_lock, noop, noop);
        let guard = lifecycle.session_started(peer());
        assert_eq!(ENDED_LOCKS.load(Ordering::SeqCst), 0);
        drop(guard);
        assert_eq!(ENDED_LOCKS.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn replaced_session_does_not_lock() {
        let lifecycle = MacSessionLifecycle::with_hooks(no_assertion, count_replaced_lock, noop, noop);
        let mut guard = lifecycle.session_started(peer());
        guard.replaced();
        drop(guard);
        assert_eq!(REPLACED_LOCKS.load(Ordering::SeqCst), 0);
    }
}
