//! Session display on CGVirtualDisplay (private CoreGraphics API). Every private selector lives here.
//!
//! Interface pinned from DeskPad's CGVirtualDisplayPrivate.h and Chromium's virtual_display_mac_util.mm.
//! Classes and selectors are checked at runtime, so a macOS without them yields an Err, never a crash.

use std::ffi::CStr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use core_graphics::display::{CGConfigureOption, CGDisplay};
use objc2::encode::{Encode, Encoding};
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject, Sel};
use objc2::{msg_send, sel};
use objc2_foundation::{NSArray, NSString};

/// How the session display relates to the Mac's own displays (config `display_mode`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DisplayMode {
    /// Local input blocked and only the client sees the desktop. With working DDC standby
    /// (`curtain_standby_ok`) it mirrors and the external monitors sleep; otherwise the session
    /// display is main, the app covers the physical displays and their windows move to the session.
    #[default]
    Curtain,
    /// Physical displays mirror the session display.
    Mirror,
    /// The session display is an extra desktop beside the physical ones, which stay as they are.
    Extend,
}

impl DisplayMode {
    /// Unknown values fall back to the private default.
    pub fn from_config(value: Option<&str>) -> Self {
        match value {
            None | Some("curtain") => Self::Curtain,
            Some("mirror") => Self::Mirror,
            Some("extend") => Self::Extend,
            Some(other) => {
                tracing::warn!(display_mode = ?other, "Unknown display_mode; using curtain");
                Self::Curtain
            }
        }
    }
}

/// Config `curtain_standby_ok`: the monitors honour DDC standby (else the curtain uses the shield).
static STANDBY_OK: AtomicBool = AtomicBool::new(false);
/// Config `curtain_standby_keepalive`: re-send standby during a curtain session.
static KEEPALIVE: AtomicBool = AtomicBool::new(false);

/// Config `curtain_standby_ok`; the next configured display uses it.
pub fn set_curtain_standby_ok(ok: bool) {
    STANDBY_OK.store(ok, Ordering::Relaxed);
}

/// Config `curtain_standby_keepalive`; the next configured display uses it.
pub fn set_curtain_standby_keepalive(on: bool) {
    KEEPALIVE.store(on, Ordering::Relaxed);
}

/// Mode size handed to CGVirtualDisplayMode plus the square max-pixel bound for the descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VirtualMode {
    pub mode_w: u32,
    pub mode_h: u32,
    pub max_px: u32,
}

/// With HiDPI the mode is in points (2x backing); max pixels covers the larger side.
pub fn virtual_mode(px_w: u32, px_h: u32, hidpi: bool) -> VirtualMode {
    let div = if hidpi { 2 } else { 1 };
    VirtualMode { mode_w: px_w / div, mode_h: px_h / div, max_px: px_w.max(px_h) }
}

/// Global bounds (x, y, w, h) in points of a display; None means the main display.
pub fn display_bounds(id: Option<u32>) -> (f64, f64, f64, f64) {
    let b = id.map_or_else(CGDisplay::main, CGDisplay::new).bounds();
    (b.origin.x, b.origin.y, b.size.width, b.size.height)
}

/// Online displays, asleep ones included (the active list drops a sleeping panel, which is the
/// usual state of a Mac used remotely).
pub fn online_displays() -> Vec<u32> {
    let mut ids = [0u32; 32];
    let mut n = 0u32;
    // SAFETY: CoreGraphics writes at most `max` ids into the buffer and reports the count.
    let err = unsafe { CGGetOnlineDisplayList(32, ids.as_mut_ptr(), &mut n) };
    if err != 0 {
        return Vec::new();
    }
    ids[..(n as usize).min(32)].to_vec()
}

/// Backing pixel size of a display's current mode.
pub fn pixel_size(id: u32) -> Option<(u64, u64)> {
    CGDisplay::new(id).display_mode().map(|m| (m.pixel_width(), m.pixel_height()))
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CGSize {
    width: f64,
    height: f64,
}

// SAFETY: matches CoreGraphics' CGSize layout and encoding on 64-bit.
unsafe impl Encode for CGSize {
    const ENCODING: Encoding = Encoding::Struct("CGSize", &[f64::ENCODING, f64::ENCODING]);
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CGPoint {
    x: f64,
    y: f64,
}

// SAFETY: matches CoreGraphics' CGPoint layout and encoding on 64-bit.
unsafe impl Encode for CGPoint {
    const ENCODING: Encoding = Encoding::Struct("CGPoint", &[f64::ENCODING, f64::ENCODING]);
}

/// Origin for an extending display: right of the rightmost display, top-aligned with the main one.
fn beside(bounds: &[(f64, f64, f64, f64)]) -> (i32, i32) {
    let right = bounds.iter().map(|&(x, _, w, _)| x + w).fold(0.0, f64::max);
    (right as i32, 0)
}

extern "C" {
    fn CGGetOnlineDisplayList(max: u32, displays: *mut u32, count: *mut u32) -> i32;
    // dispatch_queue_t is an ObjC object; typed as one so objc2's encoding check sees `@`.
    fn dispatch_get_global_queue(identifier: isize, flags: usize) -> *mut AnyObject;
}

/// A selector and the argument encodings our call passes.
type Sig<'a> = (Sel, &'a [Encoding]);

/// Class by name, only if every selector exists with the argument types we pass: a missing
/// selector would abort on an ObjC exception, a changed signature would be undefined behaviour.
fn class(name: &CStr, sigs: &[Sig<'_>]) -> Result<&'static AnyClass> {
    let cls = AnyClass::get(name).ok_or_else(|| anyhow!("{name:?} not available"))?;
    for (sel, args) in sigs {
        let m = cls.instance_method(*sel).ok_or_else(|| anyhow!("{name:?} lacks {sel:?}"))?;
        let actual: Vec<String> = (2..m.arguments_count())
            .map(|i| m.argument_type(i).map(|t| t.to_string_lossy().into_owned()).unwrap_or_default())
            .collect();
        let same = actual.len() == args.len() && args.iter().zip(&actual).all(|(e, a)| e.equivalent_to_str(a));
        if !same {
            bail!("{name:?} {sel:?} takes {actual:?}, expected {args:?}");
        }
    }
    Ok(cls)
}

/// WindowServer refuses a vendor/product/serial identity for minutes after a display with it
/// was destroyed (measured: reuse returns nil, a fresh serial succeeds), so each display gets a new one.
fn next_serial() -> u32 {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as u32);
    secs.wrapping_add(NEXT.fetch_add(1, Ordering::Relaxed))
}

fn alloc(cls: &AnyClass) -> Allocated<AnyObject> {
    // SAFETY: alloc on a class returns an uninitialised instance of it.
    unsafe { msg_send![cls, alloc] }
}

fn new_object(cls: &AnyClass) -> Result<Retained<AnyObject>> {
    // SAFETY: plain NSObject init.
    let obj: Option<Retained<AnyObject>> = unsafe { msg_send![alloc(cls), init] };
    obj.context("init returned nil")
}

/// A virtual display owned by this process. Dropping it restores the display configuration
/// and destroys the display.
///
/// There is deliberately no in-place resize: on macOS 27 a second applySettings left the
/// display alive after release and even after process exit (4 of 4 live runs; 0 of 12
/// displays without one leaked). A size change drops the display and creates a new one.
pub struct VirtualDisplay {
    display: Retained<AnyObject>,
    id: u32,
    /// Backing pixel size of the single mode.
    px: (u32, u32),
    /// Curtain with `curtain_standby_keepalive`: re-sends standby while this display lives.
    keepalive: Option<Keepalive>,
}

impl VirtualDisplay {
    /// Create a display at `px_w` x `px_h` backing pixels (HiDPI: half that in points).
    pub fn create(px_w: u32, px_h: u32, hidpi: bool) -> Result<Self> {
        objc2::rc::autoreleasepool(|_| Self::create_inner(px_w, px_h, hidpi))
    }

    fn create_inner(px_w: u32, px_h: u32, hidpi: bool) -> Result<Self> {
        const OBJ: &[Encoding] = &[Encoding::Object];
        const U32: &[Encoding] = &[u32::ENCODING];
        const PT: &[Encoding] = &[CGPoint::ENCODING];
        let desc_cls = class(c"CGVirtualDisplayDescriptor", &[
            (sel!(setQueue:), OBJ), (sel!(setName:), OBJ),
            (sel!(setMaxPixelsWide:), U32), (sel!(setMaxPixelsHigh:), U32),
            (sel!(setSizeInMillimeters:), &[CGSize::ENCODING]),
            (sel!(setSerialNum:), U32), (sel!(setProductID:), U32), (sel!(setVendorID:), U32),
            (sel!(setWhitePoint:), PT), (sel!(setRedPrimary:), PT),
            (sel!(setGreenPrimary:), PT), (sel!(setBluePrimary:), PT),
        ])?;
        let display_cls = class(c"CGVirtualDisplay", &[
            (sel!(initWithDescriptor:), OBJ), (sel!(applySettings:), OBJ), (sel!(displayID), &[]),
        ])?;
        // Fail before creating anything if settings cannot be applied.
        settings_classes()?;

        let mode = virtual_mode(px_w, px_h, hidpi);
        // ~110 ppi at the logical size: macOS picks UI scale from physical size.
        let mm = |pt: u32| f64::from(pt) / 110.0 * 25.4;
        let size = CGSize { width: mm(mode.mode_w), height: mm(mode.mode_h) };
        let name = NSString::from_str("macrdp");
        let desc = new_object(desc_cls)?;
        // SAFETY: selectors verified above; argument types follow the private header.
        let display: Option<Retained<AnyObject>> = unsafe {
            let queue = dispatch_get_global_queue(0, 0);
            let _: () = msg_send![&*desc, setQueue: queue];
            let _: () = msg_send![&*desc, setName: &*name];
            let _: () = msg_send![&*desc, setMaxPixelsWide: mode.max_px];
            let _: () = msg_send![&*desc, setMaxPixelsHigh: mode.max_px];
            let _: () = msg_send![&*desc, setSizeInMillimeters: size];
            let _: () = msg_send![&*desc, setSerialNum: next_serial()];
            let _: () = msg_send![&*desc, setProductID: 0x0001u32];
            let _: () = msg_send![&*desc, setVendorID: 0x6d72u32];
            // sRGB / D65 chromaticities (as Chromium sets them).
            let _: () = msg_send![&*desc, setWhitePoint: CGPoint { x: 0.3125, y: 0.3291 }];
            let _: () = msg_send![&*desc, setRedPrimary: CGPoint { x: 0.6797, y: 0.3203 }];
            let _: () = msg_send![&*desc, setGreenPrimary: CGPoint { x: 0.2559, y: 0.6983 }];
            let _: () = msg_send![&*desc, setBluePrimary: CGPoint { x: 0.1494, y: 0.0557 }];
            msg_send![alloc(display_cls), initWithDescriptor: &*desc]
        };
        let display = display.context("CGVirtualDisplay init returned nil")?;
        // SAFETY: selector verified above.
        let id: u32 = unsafe { msg_send![&*display, displayID] };
        if id == 0 {
            bail!("CGVirtualDisplay has no display id");
        }
        let vd = Self { display, id, px: (px_w, px_h), keepalive: None };
        registry().live.push(id);
        vd.apply(px_w, px_h, hidpi)?;
        tracing::info!(id, px_w, px_h, hidpi, "Virtual display created");
        Ok(vd)
    }

    pub fn id(&self) -> u32 {
        self.id
    }

    /// Apply the single mode. Called once per display (see the type docs).
    fn apply(&self, px_w: u32, px_h: u32, hidpi: bool) -> Result<()> {
        let (settings_cls, mode_cls) = settings_classes()?;
        let mode = virtual_mode(px_w, px_h, hidpi);
        let settings = new_object(settings_cls)?;
        // SAFETY: selectors verified in settings_classes; types follow the private header.
        let ok: bool = unsafe {
            let m: Option<Retained<AnyObject>> = msg_send![
                alloc(mode_cls),
                initWithWidth: mode.mode_w,
                height: mode.mode_h,
                refreshRate: 60.0f64
            ];
            let m = m.context("CGVirtualDisplayMode init returned nil")?;
            let modes = NSArray::from_retained_slice(&[m]);
            let _: () = msg_send![&*settings, setHiDPI: u32::from(hidpi)];
            let _: () = msg_send![&*settings, setModes: &*modes];
            msg_send![&*self.display, applySettings: &*settings]
        };
        if !ok {
            bail!("CGVirtualDisplay applySettings rejected {px_w}x{px_h} hidpi={hidpi}");
        }
        Ok(())
    }

    /// Wait briefly for the mode to land; call after [`Self::mirror_physical`].
    ///
    /// The mode is only reported once the display is online. With the console asleep, or
    /// right after the previous display's configuration was restored, WindowServer leaves a
    /// new display offline until a display configuration brings it up: our mirroring. Waiting
    /// before mirroring therefore stalled every reconnect for the whole bound (2 s, then
    /// `actual=None`), while capture ran at the right size 0.1 s after mirroring. Capture does
    /// not depend on this (it waits for the display in shareable content and scales to size).
    pub fn wait_for_mode(&self) {
        let (w, h) = self.px;
        let started = Instant::now();
        if poll_until(MODE_WAIT, || pixel_size(self.id) == Some((w.into(), h.into()))).is_some() {
            tracing::debug!(id = self.id, waited_ms = started.elapsed().as_millis() as u64, "Virtual display mode applied");
        } else {
            tracing::info!(id = self.id, px_w = w, px_h = h, actual = ?pixel_size(self.id), "Virtual display mode not reported yet — continuing");
        }
    }

    /// Apply `mode` to the Mac's displays. Every mode completes an app-only configuration, which
    /// macOS reverts when the process exits (CGCompleteDisplayConfiguration docs), crash included.
    /// This display then owns the configuration: an older one's drop no longer restores it.
    pub fn configure(&mut self, mode: DisplayMode) -> Result<()> {
        let standby_ok = STANDBY_OK.load(Ordering::Relaxed);
        // A just-created display is not online yet: its first configuration fails with CG 1001
        // (live, about one connect in two), which used to fall back to a letterboxed capture.
        let result = retry_for(ARRANGE_WAIT, Duration::from_millis(100), || match layout(mode, standby_ok) {
            Layout::Mirror => self.mirror_physical(),
            Layout::Extend => self.extend(),
            Layout::Shield => self.shield(),
        });
        let plan = monitor_plan(mode, standby_ok, KEEPALIVE.load(Ordering::Relaxed), result.is_ok());
        let mut reg = registry();
        if let Some(asleep) = standby_change(reg.standby, plan.sleep) {
            reg.standby = asleep;
            drop(reg);
            crate::ddc::set_external_power(!asleep);
        }
        self.keepalive = plan.keepalive.then(|| Keepalive::start(KEEPALIVE_EVERY, crate::ddc::resend_standby));
        result
    }

    /// Curtain without working standby: this display becomes main at (0, 0), the app covers the
    /// physical displays with black, and every window on them moves here (put back on release).
    fn shield(&mut self) -> Result<()> {
        let mut reg = registry();
        let others = reg.mirror_targets(&online_displays());
        arrange(&CGDisplay::new(self.id), &others, (0, 0))?;
        self.settle((0, 0), &others);
        reg.config_owner = Some(self.id);
        tracing::info!(id = self.id, shield = ?others, "Session display is main; the app covers the physical displays (curtain)");
        notify_shield(reg.set_shield(others.clone()));
        // Accessibility calls can be slow: not under the lock.
        drop(reg);
        let physical: Vec<_> = others.iter().map(|&d| (d, display_bounds(Some(d)))).collect();
        let moved = crate::windows::move_into(display_bounds(Some(self.id)), &physical);
        registry().moved.extend(moved);
        Ok(())
    }

    /// Place this display right of the others; they keep their arrangement and content.
    fn extend(&mut self) -> Result<()> {
        let mut reg = registry();
        let others = reg.mirror_targets(&online_displays());
        let bounds: Vec<_> = others.iter().map(|&d| display_bounds(Some(d))).collect();
        let origin = beside(&bounds);
        arrange(&CGDisplay::new(self.id), &others, origin)?;
        tracing::info!(id = self.id, x = origin.0, y = origin.1, "Virtual display extends the desktop");
        self.settle(origin, &others);
        reg.config_owner = Some(self.id);
        notify_shield(reg.set_shield(Vec::new()));
        Ok(())
    }

    /// Wait up to [`MODE_WAIT`] for this display to reach `origin` outside any mirror set, then
    /// log the arrangement every display ended up in (the mouse mapping reads these bounds).
    fn settle(&self, origin: (i32, i32), others: &[u32]) {
        let me = CGDisplay::new(self.id);
        let done = || {
            let (x, y, _, _) = display_bounds(Some(self.id));
            settled((x, y), me.is_in_mirror_set(), origin)
        };
        let waited = poll_until(MODE_WAIT, done);
        tracing::info!(
            id = self.id,
            settled = waited.is_some(),
            waited_ms = waited.map(|d| d.as_millis() as u64),
            bounds = ?display_bounds(Some(self.id)),
            main = CGDisplay::main().id,
            "Virtual display arrangement"
        );
        for &d in std::iter::once(&self.id).chain(others) {
            let cg = CGDisplay::new(d);
            tracing::info!(
                display = d,
                in_mirror_set = cg.is_in_mirror_set(),
                mirrors = cg.mirrors_display(),
                bounds = ?display_bounds(Some(d)),
                "Display arrangement"
            );
        }
    }

    /// Mirror every other online display (not our virtual ones) to this one and put it at the
    /// global origin. App-only: macOS reverts it if the process dies. Headless is fine.
    fn mirror_physical(&mut self) -> Result<()> {
        let mut reg = registry();
        let others = reg.mirror_targets(&online_displays());
        if others.is_empty() {
            return Ok(());
        }
        let me = CGDisplay::new(self.id);
        let cfg = me.begin_configuration().map_err(|e| anyhow!("CGBeginDisplayConfiguration failed: {e}"))?;
        let configured = others
            .iter()
            .try_for_each(|&d| CGDisplay::new(d).configure_display_mirror_of_display(&cfg, &me))
            .and_then(|()| me.configure_display_origin(&cfg, 0, 0));
        if let Err(e) = configured {
            let _ = me.cancel_configuration(&cfg);
            bail!("display mirror configuration failed: {e}");
        }
        me.complete_configuration(&cfg, CGConfigureOption::ConfigureForAppOnly)
            .map_err(|e| anyhow!("CGCompleteDisplayConfiguration failed: {e}"))?;
        tracing::info!(id = self.id, mirrored = ?others, "Physical displays mirror the virtual display");
        reg.config_owner = Some(self.id);
        notify_shield(reg.set_shield(Vec::new()));
        Ok(())
    }
}

/// One app-only configuration: neither `me` nor any of `others` mirrors a display, and `me`
/// sits at `origin` ((0, 0) makes it main). A new virtual display can land in a mirror set
/// (live: extend and curtain looked mirrored, `me` reported origin 0 after asking for x=836).
fn arrange(me: &CGDisplay, others: &[u32], (x, y): (i32, i32)) -> Result<()> {
    let cfg = me.begin_configuration().map_err(|e| anyhow!("CGBeginDisplayConfiguration failed: {e}"))?;
    let configured = std::iter::once(me.id)
        .chain(others.iter().copied())
        .try_for_each(|d| CGDisplay::new(d).configure_display_mirror_of_display(&cfg, &CGDisplay::null_display()))
        .and_then(|()| me.configure_display_origin(&cfg, x, y));
    if let Err(e) = configured {
        let _ = me.cancel_configuration(&cfg);
        bail!("display arrangement configuration failed: {e}");
    }
    me.complete_configuration(&cfg, CGConfigureOption::ConfigureForAppOnly)
        .map_err(|e| anyhow!("CGCompleteDisplayConfiguration failed: {e}"))
}

/// A display is where we put it once it has the wanted origin and mirrors nothing.
fn settled((x, y): (f64, f64), in_mirror_set: bool, (want_x, want_y): (i32, i32)) -> bool {
    !in_mirror_set && x == f64::from(want_x) && y == f64::from(want_y)
}

/// Receives the physical displays the app must cover whenever that set changes (empty: none).
pub type ShieldHook = Box<dyn Fn(&[u32]) + Send + Sync>;

static SHIELD_HOOK: Mutex<Option<ShieldHook>> = Mutex::new(None);

/// Install (or remove) the receiver of shield changes; the server forwards them to the app.
pub fn set_shield_hook(hook: Option<ShieldHook>) {
    *SHIELD_HOOK.lock().unwrap_or_else(PoisonError::into_inner) = hook;
}

fn notify_shield(changed: Option<Vec<u32>>) {
    let Some(displays) = changed else { return };
    if let Some(hook) = SHIELD_HOOK.lock().unwrap_or_else(PoisonError::into_inner).as_ref() {
        hook(&displays);
    }
}

/// Longest time a new display may take to accept its arrangement.
const ARRANGE_WAIT: Duration = Duration::from_secs(2);

/// Run `f` until it succeeds or `bound` has passed, `every` apart; returns the last result.
fn retry_for<T>(bound: Duration, every: Duration, mut f: impl FnMut() -> Result<T>) -> Result<T> {
    let started = Instant::now();
    loop {
        match f() {
            Err(e) if started.elapsed() < bound => {
                tracing::debug!(error = format!("{e:#}"), "Display arrangement not accepted yet; retrying");
                std::thread::sleep(every);
            }
            done => return done,
        }
    }
}

/// Longest wait for a mirrored display's mode.
const MODE_WAIT: Duration = Duration::from_millis(500);

/// Poll `cond` every 10 ms until it holds (returns the time waited) or `bound` passes.
fn poll_until(bound: Duration, mut cond: impl FnMut() -> bool) -> Option<Duration> {
    let started = Instant::now();
    loop {
        if cond() {
            return Some(started.elapsed());
        }
        if started.elapsed() >= bound {
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn settings_classes() -> Result<(&'static AnyClass, &'static AnyClass)> {
    Ok((
        class(c"CGVirtualDisplaySettings", &[
            (sel!(setHiDPI:), &[u32::ENCODING]), (sel!(setModes:), &[Encoding::Object]),
        ])?,
        class(c"CGVirtualDisplayMode", &[
            (sel!(initWithWidth:height:refreshRate:), &[u32::ENCODING, u32::ENCODING, f64::ENCODING]),
        ])?,
    ))
}

/// This process's virtual displays. CGRestorePermanentDisplayConfiguration is process-wide,
/// so only the display whose configuration is in effect may call it.
#[derive(Default)]
struct Registry {
    live: Vec<u32>,
    config_owner: Option<u32>,
    /// Physical displays the app covers for the curtain in effect.
    shield: Vec<u32>,
    /// External monitors are in DDC standby for a curtain session.
    standby: bool,
    /// Windows the shield curtain moved into the session display; the owner puts them back.
    moved: Vec<crate::windows::Moved>,
}

static REGISTRY: Mutex<Registry> = Mutex::new(Registry {
    live: Vec::new(),
    config_owner: None,
    shield: Vec::new(),
    standby: false,
    moved: Vec::new(),
});

/// How a mode arranges the displays.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Layout {
    /// Physical displays mirror the session display.
    Mirror,
    /// The session display sits right of the others.
    Extend,
    /// The session display is main; the app covers the physical displays.
    Shield,
}

/// Curtain mirrors only when DDC standby was shown to work: mirrored, the shield would black
/// out the session too.
fn layout(mode: DisplayMode, standby_ok: bool) -> Layout {
    match mode {
        DisplayMode::Curtain if standby_ok => Layout::Mirror,
        DisplayMode::Curtain => Layout::Shield,
        DisplayMode::Mirror => Layout::Mirror,
        DisplayMode::Extend => Layout::Extend,
    }
}

/// What a configured display does with the external monitors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MonitorPlan {
    /// DDC standby.
    sleep: bool,
    /// Re-send standby every [`KEEPALIVE_EVERY`].
    keepalive: bool,
}

/// Monitors sleep only under a working curtain whose standby was tested; a failed
/// configuration never leaves them asleep. The keepalive needs both.
fn monitor_plan(mode: DisplayMode, standby_ok: bool, keepalive: bool, configured: bool) -> MonitorPlan {
    let sleep = mode == DisplayMode::Curtain && standby_ok && configured;
    MonitorPlan { sleep, keepalive: sleep && keepalive }
}

/// The new standby state when it must change.
fn standby_change(asleep: bool, want: bool) -> Option<bool> {
    (want != asleep).then_some(want)
}

/// Standby re-send interval: the monitor's power button wakes it for at most this long.
const KEEPALIVE_EVERY: Duration = Duration::from_secs(5);

/// Calls `send` every interval on its own thread until dropped; the drop waits for the thread,
/// so nothing is sent afterwards.
struct Keepalive {
    stop: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Keepalive {
    fn start(every: Duration, send: impl Fn() + Send + 'static) -> Self {
        let (stop, stopped) = mpsc::channel::<()>();
        let thread = std::thread::Builder::new()
            .name("macrdp-standby-keepalive".into())
            .spawn(move || {
                while stopped.recv_timeout(every) == Err(RecvTimeoutError::Timeout) {
                    send();
                }
            })
            .map_err(|e| tracing::warn!("Cannot start the standby keepalive: {e}"))
            .ok();
        Self { stop: Some(stop), thread }
    }
}

impl Drop for Keepalive {
    fn drop(&mut self) {
        // Closing the channel wakes the thread at once.
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn registry() -> MutexGuard<'static, Registry> {
    // Plain data: a panic elsewhere cannot leave it inconsistent.
    REGISTRY.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Registry {
    /// Displays to mirror onto a new one: every online display that is not one of ours.
    fn mirror_targets(&self, online: &[u32]) -> Vec<u32> {
        online.iter().copied().filter(|d| !self.live.contains(d)).collect()
    }

    /// Replace the shield list; returns the new one when it changed (the app must hear of it).
    fn set_shield(&mut self, displays: Vec<u32>) -> Option<Vec<u32>> {
        (self.shield != displays).then(|| {
            self.shield = displays.clone();
            displays
        })
    }

    /// Forget `id`. When its configuration is the one in effect it must restore: returns the
    /// shield list it leaves behind (the app uncovers those displays).
    fn release(&mut self, id: u32) -> Option<Vec<u32>> {
        self.live.retain(|&d| d != id);
        if self.config_owner != Some(id) {
            return None;
        }
        self.config_owner = None;
        Some(std::mem::take(&mut self.shield))
    }
}

impl Drop for VirtualDisplay {
    fn drop(&mut self) {
        // Stopped first, so no standby follows the wake below.
        drop(self.keepalive.take());
        let released = registry().release(self.id);
        if let Some(shield) = released {
            let moved = std::mem::take(&mut registry().moved);
            if std::mem::take(&mut registry().standby) {
                crate::ddc::set_external_power(true);
            }
            if !shield.is_empty() {
                notify_shield(Some(Vec::new()));
            }
            // SAFETY: plain CoreGraphics call; undoes this process's app-only configuration
            // (mirroring, placement).
            unsafe { core_graphics::display::CGRestorePermanentDisplayConfiguration() };
            tracing::info!(id = self.id, "Display configuration restored");
            if !moved.is_empty() {
                // The windows still sit on this display: back to theirs once the physical
                // arrangement is in effect again, before this display goes.
                poll_until(MODE_WAIT, || CGDisplay::main().id != self.id);
                crate::windows::restore(&moved);
            }
        }
        tracing::info!(id = self.id, "Virtual display destroyed");
        // Releasing `display` (field drop, after this) removes the display.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arrangement_is_retried_until_the_new_display_accepts_it() {
        let mut calls = 0;
        let r = retry_for(Duration::from_secs(2), Duration::ZERO, || {
            calls += 1;
            if calls < 3 { Err(anyhow!("display arrangement configuration failed: 1001")) } else { Ok(calls) }
        });
        assert_eq!(r.unwrap(), 3);
        let r: Result<()> = retry_for(Duration::ZERO, Duration::ZERO, || Err(anyhow!("still failing")));
        assert!(r.is_err(), "gives up after the bound");
    }

    #[test]
    fn mirror_never_targets_our_own_displays() {
        let mut r = Registry::default();
        r.live = vec![10, 11];
        assert_eq!(r.mirror_targets(&[1, 10, 2, 11]), vec![1, 2]);
    }

    #[test]
    fn only_the_display_whose_configuration_is_in_effect_restores() {
        let mut r = Registry::default();
        r.live = vec![10, 11];
        r.config_owner = Some(11);
        assert!(r.release(10).is_none(), "an older display must not undo the newer one's configuration");
        assert!(r.release(11).is_some());
        assert!(r.live.is_empty());
        assert!(r.release(11).is_none());
    }

    #[test]
    fn the_owner_hands_back_the_shield_list_on_release() {
        let mut r = Registry::default();
        r.live = vec![10];
        r.config_owner = Some(10);
        r.shield = vec![2, 3];
        assert_eq!(r.release(10), Some(vec![2, 3]));
        assert!(r.shield.is_empty());
    }

    #[test]
    fn curtain_mirrors_only_with_working_standby_else_uses_the_shield() {
        assert_eq!(layout(DisplayMode::Curtain, true), Layout::Mirror);
        assert_eq!(layout(DisplayMode::Curtain, false), Layout::Shield);
        assert_eq!(layout(DisplayMode::Mirror, false), Layout::Mirror);
        assert_eq!(layout(DisplayMode::Mirror, true), Layout::Mirror);
        assert_eq!(layout(DisplayMode::Extend, false), Layout::Extend);
        assert_eq!(layout(DisplayMode::Extend, true), Layout::Extend);
    }

    #[test]
    fn monitor_plan_decision_table() {
        use DisplayMode::{Curtain, Extend, Mirror};
        let plan = |sleep, keepalive| MonitorPlan { sleep, keepalive };
        // (mode, standby_ok, keepalive, configured) -> plan
        let table = [
            ((Curtain, true, false, true), plan(true, false)),
            ((Curtain, true, true, true), plan(true, true)),
            ((Curtain, true, true, false), plan(false, false)), // a failed curtain never hides the screen
            ((Curtain, true, false, false), plan(false, false)),
            ((Curtain, false, false, true), plan(false, false)), // fallback: shield, no DDC
            ((Curtain, false, true, true), plan(false, false)), // keepalive needs working standby
            ((Mirror, true, true, true), plan(false, false)),
            ((Mirror, false, false, true), plan(false, false)),
            ((Extend, true, true, true), plan(false, false)),
            ((Extend, false, false, true), plan(false, false)),
        ];
        for ((mode, ok, keepalive, configured), want) in table {
            assert_eq!(monitor_plan(mode, ok, keepalive, configured), want, "{mode:?} ok={ok} keepalive={keepalive} configured={configured}");
        }
    }

    #[test]
    fn standby_changes_only_when_the_wanted_state_differs() {
        assert_eq!(standby_change(false, true), Some(true));
        assert_eq!(standby_change(true, true), None, "a rebuild keeps them asleep");
        assert_eq!(standby_change(true, false), Some(false));
        assert_eq!(standby_change(false, false), None);
    }

    #[test]
    fn keepalive_resends_until_dropped_and_stops_at_once() {
        use std::sync::atomic::AtomicUsize;
        use std::sync::Arc;
        let sent = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&sent);
        let keepalive = Keepalive::start(Duration::from_millis(10), move || {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        assert!(poll_until(Duration::from_secs(2), || sent.load(Ordering::SeqCst) >= 3).is_some());
        drop(keepalive);
        let after = sent.load(Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(sent.load(Ordering::SeqCst), after, "no re-send after the display is gone");

        // A long interval does not delay the drop.
        let t0 = Instant::now();
        drop(Keepalive::start(Duration::from_secs(60), || {}));
        assert!(t0.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn a_shield_change_is_reported_once() {
        let mut r = Registry::default();
        assert_eq!(r.set_shield(vec![1, 2]), Some(vec![1, 2]));
        assert_eq!(r.set_shield(vec![1, 2]), None, "a rebuild with the same displays tells nobody");
        assert_eq!(r.set_shield(Vec::new()), Some(Vec::new()));
        assert_eq!(r.set_shield(Vec::new()), None);
    }

    #[test]
    fn arrangement_settles_at_the_wanted_origin_outside_any_mirror_set() {
        assert!(settled((836.0, 0.0), false, (836, 0)));
        assert!(!settled((0.0, 0.0), false, (836, 0)), "still at the origin macOS gave it");
        assert!(!settled((836.0, 0.0), true, (836, 0)), "in a mirror set it shows another display");
    }

    #[test]
    fn display_mode_defaults_to_curtain() {
        assert_eq!(DisplayMode::from_config(None), DisplayMode::Curtain);
        assert_eq!(DisplayMode::from_config(Some("curtain")), DisplayMode::Curtain);
        assert_eq!(DisplayMode::from_config(Some("mirror")), DisplayMode::Mirror);
        assert_eq!(DisplayMode::from_config(Some("extend")), DisplayMode::Extend);
        // Unknown values keep the private default rather than showing the session locally.
        assert_eq!(DisplayMode::from_config(Some("Mirror ")), DisplayMode::Curtain);
    }

    #[test]
    fn extend_places_the_display_right_of_the_others() {
        assert_eq!(beside(&[]), (0, 0));
        assert_eq!(beside(&[(0.0, 0.0, 1512.0, 982.0)]), (1512, 0));
        assert_eq!(beside(&[(-1920.0, 0.0, 1920.0, 1080.0), (0.0, 0.0, 1512.0, 982.0)]), (1512, 0));
    }

    #[test]
    fn poll_until_returns_as_soon_as_the_condition_holds() {
        let mut calls = 0;
        let waited = poll_until(Duration::from_secs(5), || {
            calls += 1;
            calls == 3
        });
        assert!(waited.is_some_and(|d| d < Duration::from_secs(1)));
        assert_eq!(calls, 3);
    }

    #[test]
    fn poll_until_gives_up_at_the_bound() {
        let t0 = Instant::now();
        assert_eq!(poll_until(Duration::from_millis(50), || false), None);
        assert!(t0.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn virtual_mode_hidpi_halves_into_points() {
        assert_eq!(virtual_mode(2388, 1668, true), VirtualMode { mode_w: 1194, mode_h: 834, max_px: 2388 });
        assert_eq!(virtual_mode(2388, 1668, false), VirtualMode { mode_w: 2388, mode_h: 1668, max_px: 2388 });
    }

    /// Creates and destroys a real virtual display. Never mirrors. Afterwards confirm from a
    /// fresh process that no "macrdp" display is online (system_profiler SPDisplaysDataType).
    #[test]
    #[ignore = "creates a virtual display on the live WindowServer"]
    fn virtual_display_live_create_destroy() {
        let vd = VirtualDisplay::create(1280, 800, true).expect("create");
        let id = vd.id();
        // Without mirroring the mode lands only if WindowServer brings the display online.
        assert!(poll_until(Duration::from_secs(2), || pixel_size(id) == Some((1280, 800))).is_some());
        // WindowServer brings the display online only while the console is awake; CG keeps
        // answering mode queries for removed ids, so online membership is the only removal signal.
        let online = online_displays().contains(&id);
        println!("virtual display {id}: online={online} bounds={:?}", display_bounds(Some(id)));
        if online {
            let (_, _, w, h) = display_bounds(Some(id));
            assert_eq!((w, h), (640.0, 400.0));
        }

        // SAFETY: NSObject retainCount; ours must be the only reference so drop destroys it.
        let rc: usize = unsafe { msg_send![&*vd.display, retainCount] };
        assert_eq!(rc, 1);
        drop(vd);
        let deadline = Instant::now() + Duration::from_secs(2);
        while online && online_displays().contains(&id) {
            assert!(Instant::now() < deadline, "virtual display still online after drop");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}
