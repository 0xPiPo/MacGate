//! Mac-account authentication.
//!
//! - Enrollment verifies the Mac password with OpenDirectory and stores only its NT hash
//!   (plus OpenDirectory's password stamp) in the login Keychain.
//! - The hash is read once at start and again after an enroll, never per TCP connect.
//! - At connect, CredSSP/NTLM runs against that hash; the delegated plaintext password is
//!   then re-verified with OpenDirectory.
//! - Failed attempts put the source IP into exponential backoff; many credential failures
//!   across all sources slow every new handshake down.

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use anyhow::Result;
use ironrdp_server::{NtlmIdentity, RdpAuthenticator};
use zeroize::Zeroizing;

/// Keychain service name of the stored NT hash (account = Mac short user name).
pub const KEYCHAIN_SERVICE: &str = "macrdp";

/// Set when the stored hash no longer matches the Mac password: OpenDirectory rejected a
/// password that matched the hash, or its password stamp moved since enrollment.
pub static REENROLL_NEEDED: AtomicBool = AtomicBool::new(false);

/// Bumped by every successful [`enroll`] in this process; a running authenticator reloads
/// the hash when it changes.
pub static ENROLL_EPOCH: AtomicU64 = AtomicU64::new(0);

/// The message shown when the stored hash no longer matches the Mac password.
pub const REENROLL_MESSAGE: &str = "Mac password changed — re-enter to re-enable remote login";

/// MD4(UTF-16LE(password)): the NT hash that NTLM checks against.
pub fn nt_hash(password: &str) -> Zeroizing<[u8; 16]> {
    use md4::{Digest, Md4};
    let utf16le = Zeroizing::new(password.encode_utf16().flat_map(u16::to_le_bytes).collect::<Vec<u8>>());
    Zeroizing::new(Md4::digest(utf16le.as_slice()).into())
}

/// True when the client account names the allowed Mac user.
///
/// Case is ignored. The domain may be empty, `.` or one of this Mac's host names.
pub fn account_allowed(allowed_user: &str, client_user: &str, client_domain: &str, host_names: &[String]) -> bool {
    let same = |a: &str, b: &str| a.to_lowercase() == b.to_lowercase();
    let domain_ok = client_domain.is_empty() || client_domain == "." || host_names.iter().any(|h| same(h, client_domain));
    !allowed_user.is_empty() && same(allowed_user, client_user) && domain_ok
}

/// What enrollment stores: the NT hash and OpenDirectory's passwordLastSetTime then.
#[derive(Clone)]
pub struct Enrollment {
    pub nt_hash: Zeroizing<[u8; 16]>,
    /// Seconds; `None` for enrollments made before the stamp was stored.
    pub password_set: Option<f64>,
}

/// Mac account checks and secret storage, behind a trait so the policy is testable.
pub trait AccountStore: Send + Sync {
    /// Checks the password with OpenDirectory.
    fn verify_password(&self, user: &str, password: &str) -> Result<bool>;
    /// The stored enrollment, or `None` when the user is not enrolled.
    fn load_hash(&self, user: &str) -> Result<Option<Enrollment>>;
    fn store_hash(&self, user: &str, enrollment: &Enrollment) -> Result<()>;
    /// OpenDirectory's passwordLastSetTime for `user`, `None` when it has none.
    fn password_last_set(&self, user: &str) -> Result<Option<f64>>;
}

/// Enrolls `user`: verifies the Mac password, then stores only its NT hash and the
/// current password stamp.
///
/// Returns `Ok(false)` when OpenDirectory rejects the password.
pub fn enroll(store: &dyn AccountStore, user: &str, password: &str) -> Result<bool> {
    if password.is_empty() || !store.verify_password(user, password)? {
        return Ok(false);
    }
    let password_set = store.password_last_set(user).unwrap_or_else(|e| {
        tracing::debug!("OpenDirectory password stamp unavailable: {e:#}");
        None
    });
    store.store_hash(user, &Enrollment { nt_hash: nt_hash(password), password_set })?;
    ENROLL_EPOCH.fetch_add(1, Ordering::AcqRel);
    REENROLL_NEEDED.store(false, Ordering::Relaxed);
    Ok(true)
}

/// Keychain payload: the 16-byte NT hash, then the stamp as 8-byte little-endian f64.
fn encode_enrollment(enrollment: &Enrollment) -> Zeroizing<Vec<u8>> {
    let mut payload = Zeroizing::new(Vec::with_capacity(24));
    payload.extend_from_slice(enrollment.nt_hash.as_slice());
    if let Some(stamp) = enrollment.password_set {
        payload.extend_from_slice(&stamp.to_le_bytes());
    }
    payload
}

/// Reads [`encode_enrollment`] output; a 16-byte item is a legacy enrollment without stamp.
fn decode_enrollment(payload: &[u8]) -> Result<Enrollment> {
    let password_set = match payload.len() {
        16 => None,
        24 => Some(f64::from_le_bytes(payload[16..].try_into()?)),
        n => anyhow::bail!("not an NT hash ({n} bytes)"),
    };
    let mut nt_hash = Zeroizing::new([0u8; 16]);
    nt_hash.copy_from_slice(&payload[..16]);
    Ok(Enrollment { nt_hash, password_set })
}

/// A stamp that moved by more than this many seconds means the Mac password changed.
const STAMP_TOLERANCE: f64 = 1.0;

fn password_changed(stored: Option<f64>, current: Option<f64>) -> bool {
    matches!((stored, current), (Some(s), Some(c)) if (c - s).abs() > STAMP_TOLERANCE)
}

/// Failures an IP gets before any wait: the owner on WireGuard arrives NATed from one IP,
/// and a typo or two must not lock them out.
const FREE_FAILURES: u32 = 3;
/// Wait after the first non-free failure; doubles with each further failure.
const BACKOFF_BASE: Duration = Duration::from_secs(1);
/// Longest per-IP wait: guessing stays slow, the owner is never locked out for minutes.
const BACKOFF_MAX: Duration = Duration::from_secs(60);
/// A source with no failure for this long starts again from zero.
const BACKOFF_FORGET: Duration = Duration::from_secs(15 * 60);
/// Credential failures from all sources within `GLOBAL_WINDOW` above which every source is
/// slowed down: a /24 cannot parallelise guesses past this budget.
const GLOBAL_BUDGET: usize = 20;
const GLOBAL_WINDOW: Duration = Duration::from_secs(60);
/// While over the budget, one new handshake per this interval across all sources.
const GLOBAL_SPACING: Duration = Duration::from_secs(2);

/// Why a handshake failed, for [`Backoff::record_failure`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureKind {
    /// Wrong user or password: per IP and toward the global budget.
    Credential,
    /// Failure before authentication (garbage, TLS, timeout): per IP only.
    Protocol,
}

/// Exponential backoff per source IP after failures, plus a global guess budget.
#[derive(Default)]
pub struct Backoff {
    entries: HashMap<IpAddr, BackoffEntry>,
    /// Times of recent credential failures from any source, oldest first.
    global: VecDeque<Instant>,
    last_global_admit: Option<Instant>,
}

struct BackoffEntry {
    failures: u32,
    blocked_until: Instant,
}

impl Backoff {
    /// True when `ip` may start a handshake at `now`. Over the global budget this uses up
    /// the single admission of the current `GLOBAL_SPACING` interval.
    pub fn admit(&mut self, ip: IpAddr, now: Instant) -> bool {
        if self.entries.get(&ip).is_some_and(|e| now < e.blocked_until) {
            return false;
        }
        self.prune_global(now);
        if self.global.len() > GLOBAL_BUDGET {
            if self.last_global_admit.is_some_and(|t| now < t + GLOBAL_SPACING) {
                return false;
            }
            self.last_global_admit = Some(now);
        }
        true
    }

    pub fn record_failure(&mut self, ip: IpAddr, now: Instant, kind: FailureKind) {
        // Drop sources that have been quiet long enough; this also bounds the map.
        self.entries.retain(|_, e| now < e.blocked_until + BACKOFF_FORGET);
        let entry = self.entries.entry(ip).or_insert(BackoffEntry { failures: 0, blocked_until: now });
        entry.failures = entry.failures.saturating_add(1);
        let delay = match entry.failures.checked_sub(FREE_FAILURES + 1) {
            None => Duration::ZERO,
            Some(n) => BACKOFF_BASE.saturating_mul(1u32 << n.min(16)).min(BACKOFF_MAX),
        };
        entry.blocked_until = now + delay;
        if kind == FailureKind::Credential {
            self.prune_global(now);
            self.global.push_back(now);
        }
    }

    pub fn record_success(&mut self, ip: IpAddr) {
        self.entries.remove(&ip);
    }

    fn prune_global(&mut self, now: Instant) {
        while self.global.front().is_some_and(|&t| now.saturating_duration_since(t) >= GLOBAL_WINDOW) {
            self.global.pop_front();
        }
    }
}

struct CachedEnrollment {
    /// The [`ENROLL_EPOCH`] value the enrollment was loaded at.
    epoch: u64,
    enrollment: Enrollment,
}

/// The server's [`RdpAuthenticator`]: only `user`, with the enrolled hash and the current
/// Mac password, from a source that is not in backoff.
pub struct MacAuthenticator {
    user: String,
    host_names: Vec<String>,
    store: Arc<dyn AccountStore>,
    backoff: Mutex<Backoff>,
    cache: Mutex<Option<CachedEnrollment>>,
    reenroll: &'static AtomicBool,
    epoch: &'static AtomicU64,
    /// Config `auto_unlock`: where an accepted password waits for a locked session's first frame.
    unlock: Option<&'static crate::unlock::Pending>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl MacAuthenticator {
    pub fn new(user: String, host_names: Vec<String>, store: Arc<dyn AccountStore>) -> Self {
        Self::with_flags(user, host_names, store, &REENROLL_NEEDED, &ENROLL_EPOCH)
    }

    fn with_flags(
        user: String,
        host_names: Vec<String>,
        store: Arc<dyn AccountStore>,
        reenroll: &'static AtomicBool,
        epoch: &'static AtomicU64,
    ) -> Self {
        Self { user, host_names, store, backoff: Mutex::default(), cache: Mutex::default(), reenroll, epoch, unlock: None }
    }

    /// Config `auto_unlock`: keep an accepted password briefly so a session that starts on the
    /// lock screen can type it (see [`crate::unlock`]).
    pub fn with_auto_unlock(mut self, on: bool) -> Self {
        self.unlock = on.then_some(&crate::unlock::PENDING);
        self
    }

    /// Loads the enrollment into memory and checks the password stamp. Call once at start
    /// so no TCP connect reads the Keychain. Returns true when an enrollment exists.
    pub fn preload(&self) -> bool {
        let Some(enrollment) = self.enrollment() else {
            return false;
        };
        self.check_password_stamp(enrollment.password_set);
        true
    }

    /// The cached enrollment; reloaded only when nothing is cached or after an enroll.
    fn enrollment(&self) -> Option<Enrollment> {
        let epoch = self.epoch.load(Ordering::Acquire);
        let mut cache = lock(&self.cache);
        if let Some(cached) = cache.as_ref().filter(|c| c.epoch == epoch) {
            return Some(cached.enrollment.clone());
        }
        match self.store.load_hash(&self.user) {
            Ok(Some(enrollment)) => {
                *cache = Some(CachedEnrollment { epoch, enrollment: enrollment.clone() });
                Some(enrollment)
            }
            Ok(None) => {
                *cache = None;
                tracing::warn!(user = %self.user, "Refused: no Mac password enrolled");
                None
            }
            Err(e) => {
                tracing::error!("Refused: cannot read the enrolled hash: {e:#}");
                None
            }
        }
    }

    /// Flags re-enrollment when OpenDirectory's password stamp moved since enrollment.
    /// Reads OpenDirectory only, never the Keychain.
    fn check_password_stamp(&self, stored: Option<f64>) {
        if stored.is_none() {
            return;
        }
        match self.store.password_last_set(&self.user) {
            Ok(current) if password_changed(stored, current) => {
                self.reenroll.store(true, Ordering::Relaxed);
                tracing::warn!(user = %self.user, "{REENROLL_MESSAGE}");
            }
            Ok(_) => {}
            Err(e) => tracing::debug!("Cannot read the OpenDirectory password stamp: {e:#}"),
        }
    }

    fn decide(&self, peer: IpAddr, username: &str, domain: &str, password: &str) -> bool {
        // `?` (Debug) escapes control characters in client-supplied names.
        if !account_allowed(&self.user, username, domain, &self.host_names) {
            tracing::warn!(%peer, user = ?username, domain = ?domain, "Refused: not the Mac account running macrdp");
            return false;
        }
        if password.is_empty() {
            tracing::warn!(%peer, "Refused: empty password");
            return false;
        }
        // NTLM already matched the stored hash; OpenDirectory confirms the account is
        // enabled and the password is still the current one. Never skip this: sspi does
        // not verify NTProofStr, so the hash match alone is not proof of the password.
        match self.store.verify_password(&self.user, password) {
            Ok(true) => true,
            Ok(false) => {
                self.reenroll.store(true, Ordering::Relaxed);
                tracing::warn!(%peer, "Refused: {REENROLL_MESSAGE}");
                false
            }
            Err(e) => {
                tracing::error!(%peer, "Refused: OpenDirectory check failed: {e:#}");
                false
            }
        }
    }

    fn record_failure(&self, peer: IpAddr, kind: FailureKind) {
        lock(&self.backoff).record_failure(peer, Instant::now(), kind);
    }
}

impl RdpAuthenticator for MacAuthenticator {
    fn admit(&self, peer: IpAddr) -> bool {
        lock(&self.backoff).admit(peer, Instant::now())
    }

    fn ntlm_identity(&self) -> Option<NtlmIdentity> {
        let enrollment = self.enrollment()?;
        Some(NtlmIdentity { username: self.user.clone(), nt_hash: enrollment.nt_hash })
    }

    fn authorize(&self, peer: IpAddr, username: &str, domain: &str, password: &str) -> bool {
        let ok = self.decide(peer, username, domain, password);
        if ok {
            if let Some(unlock) = self.unlock {
                unlock.stash(password, crate::unlock::HOLD);
            }
            lock(&self.backoff).record_success(peer);
            tracing::info!(%peer, user = %self.user, "Authenticated with the Mac account");
        } else {
            self.record_failure(peer, FailureKind::Credential);
        }
        ok
    }

    fn auth_failed(&self, peer: IpAddr) {
        tracing::warn!(%peer, "NLA authentication failed (wrong user or password)");
        self.record_failure(peer, FailureKind::Credential);
        let stored = lock(&self.cache).as_ref().and_then(|c| c.enrollment.password_set);
        self.check_password_stamp(stored);
    }

    fn handshake_failed(&self, peer: IpAddr) {
        tracing::debug!(%peer, "Handshake failed before authentication");
        self.record_failure(peer, FailureKind::Protocol);
    }
}


/// Short name of the user running macrdp. Only this account may connect, because it
/// owns the captured console session.
pub fn current_user() -> Result<String> {
    let mut buf = vec![0 as libc::c_char; 4096];
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: every pointer refers to live, correctly sized storage owned by this frame.
    let rc = unsafe { libc::getpwuid_r(libc::geteuid(), &mut pwd, buf.as_mut_ptr(), buf.len(), &mut result) };
    if rc != 0 || result.is_null() {
        anyhow::bail!("cannot resolve the current user (getpwuid_r: {rc})");
    }
    // SAFETY: on success `pw_name` points to a NUL-terminated string inside `buf`.
    let name = unsafe { std::ffi::CStr::from_ptr(pwd.pw_name) };
    Ok(name.to_string_lossy().into_owned())
}

/// This Mac's host names, accepted as the client domain: the full name, the name
/// without its suffix, and the 15-character NetBIOS form.
pub fn host_names() -> Vec<String> {
    let mut buf = [0 as libc::c_char; 256];
    // SAFETY: the buffer is valid for `buf.len()` bytes; gethostname NUL-terminates on success.
    if unsafe { libc::gethostname(buf.as_mut_ptr(), buf.len()) } != 0 {
        return Vec::new();
    }
    // SAFETY: gethostname succeeded, so `buf` holds a NUL-terminated string.
    let full = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned();
    let short = full.split('.').next().unwrap_or_default().to_string();
    let netbios: String = short.chars().take(15).collect();
    let mut names = vec![full, short, netbios];
    names.dedup();
    names
}

/// The real store: OpenDirectory for password checks, the login Keychain for the hash.
pub struct SystemAccountStore;

/// errSecItemNotFound
const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;

impl AccountStore for SystemAccountStore {
    fn verify_password(&self, user: &str, password: &str) -> Result<bool> {
        od::verify_password(user, password)
    }

    fn load_hash(&self, user: &str) -> Result<Option<Enrollment>> {
        match security_framework::passwords::get_generic_password(KEYCHAIN_SERVICE, user) {
            Ok(bytes) => {
                let bytes = Zeroizing::new(bytes);
                let enrollment = decode_enrollment(&bytes)
                    .map_err(|e| anyhow::anyhow!("Keychain item {KEYCHAIN_SERVICE}/{user}: {e}"))?;
                Ok(Some(enrollment))
            }
            Err(e) if e.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(None),
            Err(e) => Err(anyhow::anyhow!("Keychain read failed: {e}")),
        }
    }

    fn store_hash(&self, user: &str, enrollment: &Enrollment) -> Result<()> {
        security_framework::passwords::set_generic_password(KEYCHAIN_SERVICE, user, &encode_enrollment(enrollment))
            .map_err(|e| anyhow::anyhow!("Keychain write failed: {e}"))
    }

    fn password_last_set(&self, user: &str) -> Result<Option<f64>> {
        od::password_last_set(user)
    }
}

/// OpenDirectory password check (`ODRecord verifyPassword:error:`), never via argv, and
/// the password stamp (`accountPolicyData` -> `passwordLastSetTime`).
mod od {
    use anyhow::Result;
    use objc2::rc::{autoreleasepool, Retained};
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    use objc2_foundation::{NSError, NSString};

    #[link(name = "OpenDirectory", kind = "framework")]
    extern "C" {}

    /// kODRecordTypeUsers
    const RECORD_TYPE_USERS: &str = "dsRecTypeStandard:Users";
    /// The authentication search node.
    const SEARCH_NODE: &str = "/Search";
    /// The account policy plist (readable by the user themself).
    const ACCOUNT_POLICY_DATA: &str = "dsAttrTypeNative:accountPolicyData";
    /// NSUTF8StringEncoding
    const UTF8_ENCODING: usize = 4;

    /// The user's OpenDirectory record, `None` when there is no such user.
    ///
    /// # Safety
    /// Call inside an autorelease pool.
    unsafe fn user_record(user: &str) -> Result<Option<Retained<AnyObject>>> {
        // SAFETY: the selectors and argument types match the OpenDirectory framework
        // headers (ODSession.h, ODNode.h, ODRecord.h).
        unsafe {
            let session: Option<Retained<AnyObject>> = msg_send![class!(ODSession), defaultSession];
            let session = session.ok_or_else(|| anyhow::anyhow!("OpenDirectory session unavailable"))?;
            let node: Result<Retained<AnyObject>, Retained<NSError>> = msg_send![
                class!(ODNode),
                nodeWithSession: &*session,
                name: &*NSString::from_str(SEARCH_NODE),
                error: _
            ];
            let node = node.map_err(|e| anyhow::anyhow!("OpenDirectory node: {}", e.localizedDescription()))?;
            let record: Result<Retained<AnyObject>, Retained<NSError>> = msg_send![
                &*node,
                recordWithRecordType: &*NSString::from_str(RECORD_TYPE_USERS),
                name: &*NSString::from_str(user),
                attributes: std::ptr::null::<AnyObject>(),
                error: _
            ];
            Ok(record.ok())
        }
    }

    pub(super) fn verify_password(user: &str, password: &str) -> Result<bool> {
        autoreleasepool(|_| {
            // SAFETY: inside an autorelease pool; `verifyPassword:error:` matches ODRecord.h.
            unsafe {
                let Some(record) = user_record(user)? else {
                    return Ok(false);
                };
                // shortcut: NSString copies the password and cannot be zeroized; it lives
                // only inside this autorelease pool.
                let verified: Result<(), Retained<NSError>> =
                    msg_send![&*record, verifyPassword: &*NSString::from_str(password), error: _];
                if let Err(e) = &verified {
                    tracing::debug!(code = e.code(), "OpenDirectory rejected the password");
                }
                Ok(verified.is_ok())
            }
        })
    }

    /// passwordLastSetTime (seconds) from the user's accountPolicyData plist.
    pub(super) fn password_last_set(user: &str) -> Result<Option<f64>> {
        autoreleasepool(|_| {
            // SAFETY: inside an autorelease pool; every selector matches ODRecord.h and the
            // Foundation headers, and each object's class is checked before class-specific
            // messages are sent.
            unsafe {
                let Some(record) = user_record(user)? else {
                    return Ok(None);
                };
                let values: Result<Retained<AnyObject>, Retained<NSError>> = msg_send![
                    &*record,
                    valuesForAttribute: &*NSString::from_str(ACCOUNT_POLICY_DATA),
                    error: _
                ];
                let Ok(values) = values else {
                    return Ok(None);
                };
                let count: usize = msg_send![&*values, count];
                if count == 0 {
                    return Ok(None);
                }
                let value: Retained<AnyObject> = msg_send![&*values, objectAtIndex: 0usize];
                let data: Option<Retained<AnyObject>> = if msg_send![&*value, isKindOfClass: class!(NSData)] {
                    Some(value)
                } else if msg_send![&*value, isKindOfClass: class!(NSString)] {
                    msg_send![&*value, dataUsingEncoding: UTF8_ENCODING]
                } else {
                    None
                };
                let Some(data) = data else {
                    return Ok(None);
                };
                let plist: Result<Retained<AnyObject>, Retained<NSError>> = msg_send![
                    class!(NSPropertyListSerialization),
                    propertyListWithData: &*data,
                    options: 0usize,
                    format: std::ptr::null_mut::<usize>(),
                    error: _
                ];
                let Ok(plist) = plist else {
                    return Ok(None);
                };
                if !msg_send![&*plist, isKindOfClass: class!(NSDictionary)] {
                    return Ok(None);
                }
                let stamp: Option<Retained<AnyObject>> =
                    msg_send![&*plist, objectForKey: &*NSString::from_str("passwordLastSetTime")];
                let Some(stamp) = stamp else {
                    return Ok(None);
                };
                if !msg_send![&*stamp, isKindOfClass: class!(NSNumber)] {
                    return Ok(None);
                }
                let seconds: f64 = msg_send![&*stamp, doubleValue];
                Ok(Some(seconds))
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;

    #[test]
    fn nt_hash_matches_known_vector() {
        let hex: String = nt_hash("password").iter().map(|b| format!("{b:02X}")).collect();
        assert_eq!(hex, "8846F7EAEE8FB117AD06BDD830B7586C");
    }

    fn hosts() -> Vec<String> {
        vec!["Pipos-MacBook.local".into(), "Pipos-MacBook".into()]
    }

    #[test]
    fn account_match_ignores_case() {
        assert!(account_allowed("pipo", "PIPO", "", &hosts()));
        assert!(account_allowed("pipo", "Pipo", "", &hosts()));
    }

    #[test]
    fn account_rejects_other_user() {
        assert!(!account_allowed("pipo", "root", "", &hosts()));
        assert!(!account_allowed("pipo", "pip", "", &hosts()));
        assert!(!account_allowed("pipo", "", "", &hosts()));
    }

    #[test]
    fn account_domain_may_be_empty_dot_or_host() {
        assert!(account_allowed("pipo", "pipo", ".", &hosts()));
        assert!(account_allowed("pipo", "pipo", "pipos-macbook", &hosts()));
        assert!(account_allowed("pipo", "pipo", "PIPOS-MACBOOK.LOCAL", &hosts()));
    }

    #[test]
    fn account_rejects_foreign_domain() {
        assert!(!account_allowed("pipo", "pipo", "CORP", &hosts()));
        assert!(!account_allowed("pipo", "pipo", "other-mac", &hosts()));
    }

    /// In-memory store: `password` is the Mac password OpenDirectory accepts,
    /// `od_stamp` its passwordLastSetTime.
    struct FakeStore {
        password: String,
        hashes: Mutex<HashMap<String, Enrollment>>,
        loads: AtomicUsize,
        od_stamp: Mutex<Option<f64>>,
    }

    impl FakeStore {
        fn new(password: &str) -> Self {
            Self { password: password.into(), hashes: Mutex::default(), loads: AtomicUsize::new(0), od_stamp: Mutex::new(Some(100.0)) }
        }
        fn hash(&self, user: &str) -> Option<[u8; 16]> {
            self.hashes.lock().unwrap().get(user).map(|e| *e.nt_hash)
        }
        fn set_od_stamp(&self, stamp: Option<f64>) {
            *self.od_stamp.lock().unwrap() = stamp;
        }
        fn loads(&self) -> usize {
            self.loads.load(Ordering::Relaxed)
        }
    }

    impl AccountStore for FakeStore {
        fn verify_password(&self, _user: &str, password: &str) -> Result<bool> {
            Ok(password == self.password)
        }
        fn load_hash(&self, user: &str) -> Result<Option<Enrollment>> {
            self.loads.fetch_add(1, Ordering::Relaxed);
            Ok(self.hashes.lock().unwrap().get(user).cloned())
        }
        fn store_hash(&self, user: &str, enrollment: &Enrollment) -> Result<()> {
            self.hashes.lock().unwrap().insert(user.into(), enrollment.clone());
            Ok(())
        }
        fn password_last_set(&self, _user: &str) -> Result<Option<f64>> {
            Ok(*self.od_stamp.lock().unwrap())
        }
    }

    #[test]
    fn enroll_stores_only_the_nt_hash() {
        let store = FakeStore::new("hunter2");
        assert!(enroll(&store, "pipo", "hunter2").unwrap());
        assert_eq!(store.hash("pipo"), Some(*nt_hash("hunter2")));
    }

    #[test]
    fn enroll_stores_the_password_stamp() {
        let store = FakeStore::new("hunter2");
        store.set_od_stamp(Some(123.5));
        enroll(&store, "pipo", "hunter2").unwrap();
        assert_eq!(store.load_hash("pipo").unwrap().unwrap().password_set, Some(123.5));
    }

    #[test]
    fn enroll_bumps_the_epoch() {
        let store = FakeStore::new("hunter2");
        let before = ENROLL_EPOCH.load(Ordering::Acquire);
        enroll(&store, "pipo", "hunter2").unwrap();
        assert!(ENROLL_EPOCH.load(Ordering::Acquire) > before);
    }

    #[test]
    fn enroll_rejects_wrong_password_and_stores_nothing() {
        let store = FakeStore::new("hunter2");
        assert!(!enroll(&store, "pipo", "wrong").unwrap());
        assert_eq!(store.hash("pipo"), None);
    }

    #[test]
    fn enroll_rejects_empty_password() {
        let store = FakeStore::new("");
        assert!(!enroll(&store, "pipo", "").unwrap());
        assert_eq!(store.hash("pipo"), None);
    }

    #[test]
    fn keychain_payload_round_trips_with_stamp() {
        let e = Enrollment { nt_hash: nt_hash("hunter2"), password_set: Some(1_700_000_000.25) };
        let payload = encode_enrollment(&e);
        assert_eq!(payload.len(), 24);
        let back = decode_enrollment(&payload).unwrap();
        assert_eq!(*back.nt_hash, *e.nt_hash);
        assert_eq!(back.password_set, e.password_set);
    }

    #[test]
    fn keychain_payload_reads_legacy_hash_without_stamp() {
        let back = decode_enrollment(&*nt_hash("hunter2")).unwrap();
        assert_eq!(*back.nt_hash, *nt_hash("hunter2"));
        assert_eq!(back.password_set, None);
    }

    #[test]
    fn keychain_payload_rejects_other_lengths() {
        assert!(decode_enrollment(&[0u8; 15]).is_err());
        assert!(decode_enrollment(&[0u8; 17]).is_err());
        assert!(decode_enrollment(&[0u8; 25]).is_err());
    }

    /// Talks to the real OpenDirectory; run by hand with `--ignored`.
    #[test]
    #[ignore]
    fn system_store_rejects_wrong_password_and_unknown_user() {
        let user = current_user().unwrap();
        assert!(!SystemAccountStore.verify_password(&user, "definitely-not-the-password").unwrap());
        assert!(!SystemAccountStore.verify_password("no-such-user-macrdp", "x").unwrap());
        assert!(SystemAccountStore.load_hash("no-such-user-macrdp").unwrap().is_none());
    }

    /// Reads the real OpenDirectory password stamp; run by hand with `--ignored`.
    #[test]
    #[ignore]
    fn system_store_reads_password_last_set() {
        let user = current_user().unwrap();
        let stamp = SystemAccountStore.password_last_set(&user).unwrap();
        assert!(stamp.is_some_and(|t| t > 0.0), "stamp: {stamp:?}");
    }

    struct Fixture {
        auth: MacAuthenticator,
        store: Arc<FakeStore>,
        flag: &'static AtomicBool,
        epoch: &'static AtomicU64,
    }

    fn fixture_with(store: FakeStore) -> Fixture {
        let store = Arc::new(store);
        let flag: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
        let epoch: &'static AtomicU64 = Box::leak(Box::new(AtomicU64::new(0)));
        let dyn_store: Arc<dyn AccountStore> = store.clone();
        let auth = MacAuthenticator::with_flags("pipo".into(), hosts(), dyn_store, flag, epoch);
        Fixture { auth, store, flag, epoch }
    }

    fn authenticator(mac_password: &str) -> (MacAuthenticator, &'static AtomicBool) {
        let store = FakeStore::new(mac_password);
        enroll(&store, "pipo", mac_password).unwrap();
        let f = fixture_with(store);
        (f.auth, f.flag)
    }

    #[test]
    fn ntlm_identity_is_none_until_enrolled() {
        let f = fixture_with(FakeStore::new("hunter2"));
        assert!(f.auth.ntlm_identity().is_none());
        enroll(f.store.as_ref(), "pipo", "hunter2").unwrap();
        let id = f.auth.ntlm_identity().unwrap();
        assert_eq!(id.username, "pipo");
        assert_eq!(*id.nt_hash, *nt_hash("hunter2"));
    }

    #[test]
    fn ntlm_identity_reads_the_store_once() {
        let store = FakeStore::new("hunter2");
        enroll(&store, "pipo", "hunter2").unwrap();
        let f = fixture_with(store);
        let before = f.store.loads();
        assert!(f.auth.ntlm_identity().is_some());
        assert!(f.auth.ntlm_identity().is_some());
        assert_eq!(f.store.loads() - before, 1);
    }

    #[test]
    fn ntlm_identity_reloads_after_enroll_epoch_changes() {
        let store = FakeStore::new("hunter2");
        enroll(&store, "pipo", "hunter2").unwrap();
        let f = fixture_with(store);
        f.auth.ntlm_identity().unwrap();
        let other = Enrollment { nt_hash: nt_hash("new-pass"), password_set: None };
        f.store.store_hash("pipo", &other).unwrap();
        f.epoch.fetch_add(1, Ordering::AcqRel);
        assert_eq!(*f.auth.ntlm_identity().unwrap().nt_hash, *nt_hash("new-pass"));
        assert_eq!(f.store.loads(), 2);
    }

    #[test]
    fn ntlm_identity_retries_while_not_enrolled() {
        let f = fixture_with(FakeStore::new("hunter2"));
        assert!(f.auth.ntlm_identity().is_none());
        assert!(f.auth.ntlm_identity().is_none());
        assert_eq!(f.store.loads(), 2);
    }

    #[test]
    fn preload_flags_a_changed_password_stamp() {
        let store = FakeStore::new("hunter2");
        store.set_od_stamp(Some(100.0));
        enroll(&store, "pipo", "hunter2").unwrap();
        store.set_od_stamp(Some(200.0));
        let f = fixture_with(store);
        f.auth.preload();
        assert!(f.flag.load(Ordering::Relaxed));
    }

    #[test]
    fn preload_keeps_flag_clear_for_same_stamp() {
        let store = FakeStore::new("hunter2");
        enroll(&store, "pipo", "hunter2").unwrap();
        let f = fixture_with(store);
        f.auth.preload();
        assert!(!f.flag.load(Ordering::Relaxed));
        assert_eq!(f.store.loads(), 1);
        f.auth.ntlm_identity().unwrap();
        assert_eq!(f.store.loads(), 1, "preload fills the cache");
    }

    #[test]
    fn preload_keeps_flag_clear_for_legacy_enrollment() {
        let store = FakeStore::new("hunter2");
        store.store_hash("pipo", &Enrollment { nt_hash: nt_hash("hunter2"), password_set: None }).unwrap();
        store.set_od_stamp(Some(999.0));
        let f = fixture_with(store);
        f.auth.preload();
        assert!(!f.flag.load(Ordering::Relaxed));
    }

    #[test]
    fn auth_failure_flags_a_changed_password_stamp() {
        let store = FakeStore::new("hunter2");
        enroll(&store, "pipo", "hunter2").unwrap();
        let f = fixture_with(store);
        f.auth.preload();
        f.store.set_od_stamp(Some(500.0));
        let loads = f.store.loads();
        f.auth.auth_failed(ip());
        assert!(f.flag.load(Ordering::Relaxed));
        assert_eq!(f.store.loads(), loads, "auth_failed never reads the Keychain");
    }

    #[test]
    fn authorize_accepts_enrolled_user_with_mac_password() {
        let (auth, _) = authenticator("hunter2");
        assert!(auth.authorize(ip(), "Pipo", "", "hunter2"));
    }

    #[test]
    fn auto_unlock_holds_only_an_accepted_password() {
        static HELD: crate::unlock::Pending = crate::unlock::Pending::new();
        let (mut auth, _) = authenticator("hunter2");
        auth.unlock = Some(&HELD);
        assert!(!auth.authorize(ip(), "pipo", "", "old-password"));
        assert!(HELD.is_empty(), "a refused password is never kept");
        assert!(auth.authorize(ip(), "pipo", "", "hunter2"));
        assert!(!HELD.is_empty());
        HELD.session_started(true, false);
        assert!(HELD.is_empty());
    }

    #[test]
    fn without_auto_unlock_no_password_is_kept() {
        let (auth, _) = authenticator("hunter2");
        assert!(auth.unlock.is_none(), "off unless the config enables it");
    }

    #[test]
    fn authorize_rejects_other_user() {
        let (auth, _) = authenticator("hunter2");
        assert!(!auth.authorize(ip(), "root", "", "hunter2"));
    }

    #[test]
    fn authorize_rejects_empty_password() {
        let (auth, _) = authenticator("hunter2");
        assert!(!auth.authorize(ip(), "pipo", "", ""));
    }

    #[test]
    fn authorize_rejects_changed_mac_password_and_requests_reenroll() {
        let (auth, flag) = authenticator("hunter2");
        // CredSSP matched the stored hash, but OpenDirectory now has a new password.
        assert!(!auth.authorize(ip(), "pipo", "", "old-password"));
        assert!(flag.load(Ordering::Relaxed));
    }

    #[test]
    fn failed_auth_puts_peer_in_backoff_after_free_failures() {
        let (auth, _) = authenticator("hunter2");
        for _ in 0..FREE_FAILURES {
            auth.auth_failed(ip());
            assert!(auth.admit(ip()));
        }
        auth.auth_failed(ip());
        assert!(!auth.admit(ip()));
    }

    #[test]
    fn rejected_authorization_puts_peer_in_backoff_after_free_failures() {
        let (auth, _) = authenticator("hunter2");
        for _ in 0..=FREE_FAILURES {
            auth.authorize(ip(), "root", "", "hunter2");
        }
        assert!(!auth.admit(ip()));
    }

    #[test]
    fn handshake_failures_put_peer_in_backoff() {
        let (auth, _) = authenticator("hunter2");
        for _ in 0..=FREE_FAILURES {
            auth.handshake_failed(ip());
        }
        assert!(!auth.admit(ip()));
    }

    fn ip() -> IpAddr {
        "192.0.2.7".parse().unwrap()
    }

    fn nth_ip(n: usize) -> IpAddr {
        IpAddr::V4(std::net::Ipv4Addr::from(0x0A00_0000 + u32::try_from(n).unwrap()))
    }

    #[test]
    fn backoff_admits_unknown_ip() {
        assert!(Backoff::default().admit(ip(), Instant::now()));
    }

    #[test]
    fn backoff_first_failures_are_free() {
        let mut b = Backoff::default();
        let t0 = Instant::now();
        for _ in 0..FREE_FAILURES {
            b.record_failure(ip(), t0, FailureKind::Credential);
            assert!(b.admit(ip(), t0));
        }
    }

    #[test]
    fn backoff_blocks_after_free_failures_then_releases() {
        let mut b = Backoff::default();
        let t0 = Instant::now();
        for _ in 0..=FREE_FAILURES {
            b.record_failure(ip(), t0, FailureKind::Credential);
        }
        assert!(!b.admit(ip(), t0));
        assert!(b.admit(ip(), t0 + BACKOFF_BASE));
    }

    #[test]
    fn backoff_doubles_and_caps_at_60s() {
        let mut b = Backoff::default();
        let t0 = Instant::now();
        // Failures 4, 5, 6: 1 s, 2 s, 4 s.
        for _ in 0..FREE_FAILURES + 3 {
            b.record_failure(ip(), t0, FailureKind::Protocol);
        }
        assert!(!b.admit(ip(), t0 + Duration::from_secs(3)));
        assert!(b.admit(ip(), t0 + Duration::from_secs(4)));
        for _ in 0..40 {
            b.record_failure(ip(), t0, FailureKind::Protocol);
        }
        assert_eq!(BACKOFF_MAX, Duration::from_secs(60));
        assert!(!b.admit(ip(), t0 + BACKOFF_MAX - Duration::from_secs(1)));
        assert!(b.admit(ip(), t0 + BACKOFF_MAX));
    }

    #[test]
    fn backoff_is_per_ip() {
        let mut b = Backoff::default();
        let t0 = Instant::now();
        for _ in 0..=FREE_FAILURES {
            b.record_failure(ip(), t0, FailureKind::Credential);
        }
        assert!(b.admit("192.0.2.8".parse().unwrap(), t0));
    }

    #[test]
    fn backoff_success_clears() {
        let mut b = Backoff::default();
        let t0 = Instant::now();
        for _ in 0..=FREE_FAILURES {
            b.record_failure(ip(), t0, FailureKind::Credential);
        }
        b.record_success(ip());
        assert!(b.admit(ip(), t0));
    }

    #[test]
    fn backoff_forgets_after_quiet_period() {
        let mut b = Backoff::default();
        let t0 = Instant::now();
        for _ in 0..10 {
            b.record_failure(ip(), t0, FailureKind::Protocol);
        }
        assert_eq!(BACKOFF_FORGET, Duration::from_secs(15 * 60));
        let later = t0 + BACKOFF_MAX + BACKOFF_FORGET;
        b.record_failure(ip(), later, FailureKind::Protocol);
        assert!(b.admit(ip(), later), "counted as a first, free failure");
    }

    #[test]
    fn backoff_global_budget_spaces_admissions() {
        let mut b = Backoff::default();
        let t0 = Instant::now();
        for n in 0..=GLOBAL_BUDGET {
            b.record_failure(nth_ip(n), t0, FailureKind::Credential);
        }
        let fresh = |n: usize| nth_ip(1000 + n);
        assert!(b.admit(fresh(0), t0));
        assert!(!b.admit(fresh(1), t0 + Duration::from_secs(1)));
        assert!(b.admit(fresh(1), t0 + GLOBAL_SPACING));
        // The window drains: admission is free again.
        let drained = t0 + GLOBAL_WINDOW + Duration::from_millis(1);
        assert!(b.admit(fresh(2), drained));
        assert!(b.admit(fresh(3), drained));
    }

    #[test]
    fn backoff_protocol_failures_skip_global_budget() {
        let mut b = Backoff::default();
        let t0 = Instant::now();
        for n in 0..=GLOBAL_BUDGET * 2 {
            b.record_failure(nth_ip(n), t0, FailureKind::Protocol);
        }
        assert!(b.admit(nth_ip(5000), t0));
        assert!(b.admit(nth_ip(5001), t0));
    }
}
