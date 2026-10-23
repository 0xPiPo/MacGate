//! Auto-unlock (config `auto_unlock`): types the session's own password into the macOS lock
//! screen once, so a client that connects to a locked Mac need not type it again.
//!
//! The password is the one CredSSP delegated and OpenDirectory just accepted. It lives in a
//! zeroized buffer for at most [`HOLD`], is typed at most once, and is never logged, sent to
//! the app or written anywhere.

use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use core_foundation::base::TCFType;
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
use core_foundation::string::CFString;
use macrdp_input::KeyboardInjector;
use zeroize::Zeroizing;

/// Longest the password is kept after authentication.
pub(crate) const HOLD: Duration = Duration::from_secs(10);
/// Return (scancode 0x1C) submits the lock screen.
const RETURN: u8 = 0x1C;

/// The process's one pending password.
pub(crate) static PENDING: Pending = Pending::new();

/// Type the password only for a locked console with auto-unlock on.
pub(crate) fn should_type(auto_unlock: bool, locked: bool) -> bool {
    auto_unlock && locked
}

struct Slot {
    password: Option<Zeroizing<String>>,
    /// The session started on a locked screen: the first frame may type it.
    armed: bool,
    /// Tells an older stash's expiry timer from the current one.
    generation: u64,
}

/// A password held between authentication and the session's first frame.
pub(crate) struct Pending {
    slot: Mutex<Slot>,
}

impl Pending {
    pub(crate) const fn new() -> Self {
        Self { slot: Mutex::new(Slot { password: None, armed: false, generation: 0 }) }
    }

    fn slot(&self) -> MutexGuard<'_, Slot> {
        // Only a buffer and flags: a panic elsewhere cannot leave them inconsistent.
        self.slot.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Holds `password` (unarmed) for at most `hold`, replacing and zeroizing any older one.
    pub(crate) fn stash(&'static self, password: &str, hold: Duration) {
        let generation = {
            let mut slot = self.slot();
            slot.generation += 1;
            slot.password = Some(Zeroizing::new(password.to_owned()));
            slot.armed = false;
            slot.generation
        };
        let spawned = std::thread::Builder::new().name("macrdp-unlock-expiry".into()).spawn(move || {
            std::thread::sleep(hold);
            self.expire(generation);
        });
        if spawned.is_err() {
            // Without a timer it is not held at all.
            self.clear();
        }
    }

    /// Session start: arms the held password for a locked screen, otherwise drops it now.
    pub(crate) fn session_started(&self, auto_unlock: bool, locked: bool) {
        let mut slot = self.slot();
        if slot.password.is_none() {
            return;
        }
        if should_type(auto_unlock, locked) {
            slot.armed = true;
            tracing::info!("Screen is locked: auto-unlock types the password at the first frame");
        } else {
            slot.password = None;
            slot.armed = false;
            tracing::debug!(locked, "Auto-unlock not needed; password dropped");
        }
    }

    /// The armed password, once.
    fn take(&self) -> Option<Zeroizing<String>> {
        let mut slot = self.slot();
        if !std::mem::take(&mut slot.armed) {
            return None;
        }
        slot.password.take()
    }

    fn clear(&self) {
        let mut slot = self.slot();
        slot.password = None;
        slot.armed = false;
    }

    fn expire(&self, generation: u64) {
        let mut slot = self.slot();
        if slot.generation == generation && slot.password.is_some() {
            slot.password = None;
            slot.armed = false;
            tracing::debug!("Auto-unlock password expired unused");
        }
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.slot().password.is_none()
    }
}

/// Where the keystrokes go; [`KeyboardInjector`] outside tests.
pub(crate) trait Keys {
    fn unicode(&self, unit: u16) -> anyhow::Result<()>;
    fn key(&self, scancode: u8, pressed: bool) -> anyhow::Result<()>;
}

impl Keys for KeyboardInjector {
    fn unicode(&self, unit: u16) -> anyhow::Result<()> {
        self.inject_unicode(unit, true)
    }
    fn key(&self, scancode: u8, pressed: bool) -> anyhow::Result<()> {
        self.inject_key(scancode, false, pressed)
    }
}

/// Types the armed password and Return; the buffer is zeroized when this returns. Stops before
/// Return if a character fails, so half a password is never submitted.
pub(crate) fn unlock_with(pending: &Pending, keys: &dyn Keys) -> bool {
    let Some(password) = pending.take() else { return false };
    let typed = password.encode_utf16().try_for_each(|unit| keys.unicode(unit)).and_then(|()| {
        keys.key(RETURN, true)?;
        keys.key(RETURN, false)
    });
    drop(password);
    match typed {
        Ok(()) => {
            tracing::info!("Auto-unlock: password typed into the lock screen");
            true
        }
        Err(e) => {
            tracing::warn!("Auto-unlock failed: {e:#}");
            false
        }
    }
}

/// The session's first frame went out: type the password if a locked session armed it.
pub(crate) fn first_frame_sent() {
    if !PENDING.slot().armed {
        return;
    }
    match KeyboardInjector::new() {
        Ok(keys) => {
            unlock_with(&PENDING, &keys);
        }
        Err(e) => {
            PENDING.clear();
            tracing::warn!("Auto-unlock unavailable: {e:#}");
        }
    }
}

/// The console session shows the lock screen. `CGSSessionScreenIsLocked` is present (true) in
/// the session dictionary only while locked (the key is in this macOS's CoreGraphics; absent in
/// an unlocked session's dictionary).
pub(crate) fn screen_locked() -> bool {
    // SAFETY: plain CoreGraphics call; a non-null result is a +1 dictionary we take over.
    let raw = unsafe { CGSessionCopyCurrentDictionary() };
    if raw.is_null() {
        return false;
    }
    // SAFETY: non-null, owned (create rule).
    let session: CFDictionary<CFString, core_foundation::base::CFType> = unsafe { CFDictionary::wrap_under_create_rule(raw) };
    session
        .find(CFString::from_static_string("CGSSessionScreenIsLocked"))
        .and_then(|v| v.downcast::<CFBoolean>())
        .is_some_and(bool::from)
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGSessionCopyCurrentDictionary() -> CFDictionaryRef;
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use super::*;

    const SECRET: &str = "correct horse ✓ battery";

    /// Records every key it is asked to post.
    #[derive(Default)]
    struct FakeKeys {
        units: Mutex<Vec<u16>>,
        keys: Mutex<Vec<(u8, bool)>>,
    }

    impl Keys for FakeKeys {
        fn unicode(&self, unit: u16) -> anyhow::Result<()> {
            self.units.lock().unwrap().push(unit);
            Ok(())
        }
        fn key(&self, scancode: u8, pressed: bool) -> anyhow::Result<()> {
            self.keys.lock().unwrap().push((scancode, pressed));
            Ok(())
        }
    }

    #[test]
    fn types_only_when_enabled_and_the_screen_is_locked() {
        assert!(should_type(true, true));
        assert!(!should_type(true, false), "an unlocked Mac needs no password");
        assert!(!should_type(false, true));
        assert!(!should_type(false, false));
    }

    #[test]
    fn a_locked_session_types_the_password_then_return_once() {
        static P: Pending = Pending::new();
        P.stash(SECRET, Duration::from_secs(60));
        P.session_started(true, true);
        let keys = FakeKeys::default();
        assert!(unlock_with(&P, &keys));
        assert_eq!(*keys.units.lock().unwrap(), SECRET.encode_utf16().collect::<Vec<_>>());
        assert_eq!(*keys.keys.lock().unwrap(), [(RETURN, true), (RETURN, false)]);
        assert!(P.is_empty(), "the buffer is gone after use");
        assert!(!unlock_with(&P, &keys), "never a second use");
        assert_eq!(keys.keys.lock().unwrap().len(), 2);
    }

    #[test]
    fn an_unlocked_or_disabled_session_drops_the_password_at_start() {
        for (enabled, locked) in [(true, false), (false, true), (false, false)] {
            let p: &'static Pending = Box::leak(Box::new(Pending::new()));
            p.stash(SECRET, Duration::from_secs(60));
            p.session_started(enabled, locked);
            assert!(p.is_empty(), "enabled={enabled} locked={locked}");
            let keys = FakeKeys::default();
            assert!(!unlock_with(p, &keys));
            assert!(keys.units.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn nothing_is_typed_before_the_session_starts() {
        static P: Pending = Pending::new();
        P.stash(SECRET, Duration::from_secs(60));
        let keys = FakeKeys::default();
        assert!(!unlock_with(&P, &keys), "only a started, locked session arms it");
        assert!(keys.units.lock().unwrap().is_empty());
    }

    #[test]
    fn the_password_expires_unused() {
        static P: Pending = Pending::new();
        P.stash(SECRET, Duration::from_millis(20));
        let gone = (0..200).any(|_| {
            std::thread::sleep(Duration::from_millis(10));
            P.is_empty()
        });
        assert!(gone, "held only briefly");
        P.session_started(true, true);
        assert!(!unlock_with(&P, &FakeKeys::default()));
    }

    #[test]
    fn a_newer_password_is_not_expired_by_the_older_timer() {
        static P: Pending = Pending::new();
        P.stash("old", Duration::from_millis(20));
        P.stash(SECRET, Duration::from_secs(60));
        std::thread::sleep(Duration::from_millis(100));
        assert!(!P.is_empty());
        P.session_started(true, true);
        let keys = FakeKeys::default();
        assert!(unlock_with(&P, &keys));
        assert_eq!(*keys.units.lock().unwrap(), SECRET.encode_utf16().collect::<Vec<_>>());
    }

    #[test]
    fn a_failed_key_stops_before_return() {
        struct Broken;
        impl Keys for Broken {
            fn unicode(&self, _: u16) -> anyhow::Result<()> {
                anyhow::bail!("cannot post")
            }
            fn key(&self, _: u8, _: bool) -> anyhow::Result<()> {
                panic!("Return after a failed character would submit half a password")
            }
        }
        static P: Pending = Pending::new();
        P.stash(SECRET, Duration::from_secs(60));
        P.session_started(true, true);
        assert!(!unlock_with(&P, &Broken));
        assert!(P.is_empty());
    }

    /// Reads the live console session (read-only). Run by hand while unlocked: `--ignored`.
    #[test]
    #[ignore = "reads the live console session"]
    fn an_unlocked_console_reads_as_unlocked() {
        assert!(!screen_locked());
    }

    /// Captures everything the fmt subscriber writes.
    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn the_password_never_reaches_the_log() {
        static P: Pending = Pending::new();
        let out = Captured::default();
        let writer = out.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            P.stash(SECRET, Duration::from_secs(60));
            P.session_started(true, true);
            assert!(unlock_with(&P, &FakeKeys::default()));
            P.stash(SECRET, Duration::from_secs(60));
            P.session_started(true, false);
        });
        let log = String::from_utf8(out.0.lock().unwrap().clone()).unwrap();
        assert!(!log.is_empty(), "the flow logs something");
        assert!(!log.contains("horse"), "{log}");
    }
}
