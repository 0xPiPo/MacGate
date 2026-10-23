use anyhow::{Context, Result};
use bytes::Bytes;
use ironrdp_pdu::codecs::rfx::RfxRectangle;
use ironrdp_server::{
    BitmapUpdate, DesktopSize, DisplayUpdate, GfxFrameUpdate, GfxRemoteFxUpdate, GfxUncompressedUpdate,
    PixelFormat as RdpPixelFormat, RdpServerDisplay, RdpServerDisplayUpdates, UncompressedRect,
    gfx::{GfxState, RfxQuality},
};
use macrdp_audio::AudioTap;
use macrdp_capture::{CaptureConfig, CapturePixelFormat, CapturedFrame, CgFallbackCapturer, FrameData, Rect, SafePixelBuffer, ScreenCapturer};
use macrdp_capture::virtual_display::{display_bounds, DisplayMode, VirtualDisplay};
use macrdp_input::LocalInputBlocker;
use macrdp_encode::{self, Quality, VideoEncoder};
use std::num::{NonZeroU16, NonZeroUsize};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::bitrate_controller::{BitrateController, FrameStats};
use crate::rfx_quality::{RfxQualityController, RfxSample};
use crate::handler::MouseCoordMapper;
use crate::perf_stats::SharedPerfStats;
use crate::resolution::{self, Debounce, TargetLayout, LAYOUT_DEBOUNCE};

/// Maximum tile size for bitmap updates
const TILE_SIZE: u16 = 64;

/// Convert a captured frame into tiled BitmapUpdate chunks
pub fn frame_to_bitmap_updates(frame: &CapturedFrame, tile_size: u16) -> Vec<BitmapUpdate> {
    let bgra = match frame.data.as_bgra_bytes() {
        Some(b) => b,
        None => return Vec::new(), // PixelBuffer frames don't go through bitmap path
    };
    let mut updates = Vec::new();
    let bpp: usize = 4;

    let cols = (frame.width as u16 + tile_size - 1) / tile_size;
    let rows = (frame.height as u16 + tile_size - 1) / tile_size;

    for row in 0..rows {
        for col in 0..cols {
            let x = col * tile_size;
            let y = row * tile_size;
            let w = (frame.width as u16 - x).min(tile_size);
            let h = (frame.height as u16 - y).min(tile_size);

            let Some(width) = NonZeroU16::new(w) else { continue };
            let Some(height) = NonZeroU16::new(h) else { continue };

            let mut tile_data = Vec::with_capacity(w as usize * h as usize * bpp);
            for dy in 0..h {
                let src_y = (y + dy) as usize;
                let src_x_start = x as usize * bpp;
                let src_x_end = src_x_start + w as usize * bpp;
                let row_start = src_y * frame.stride;
                if row_start + src_x_end <= bgra.len() {
                    tile_data.extend_from_slice(&bgra[row_start + src_x_start..row_start + src_x_end]);
                }
            }

            let stride = w as usize * bpp;
            let Some(stride) = NonZeroUsize::new(stride) else { continue };

            updates.push(BitmapUpdate {
                x,
                y,
                width,
                height,
                format: RdpPixelFormat::BgrA32,
                data: Bytes::from(tile_data),
                stride,
            });
        }
    }

    updates
}

/// How a captured frame reaches the client.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Route {
    /// GFX AVC420/AVC444 via the H.264 encoder.
    H264,
    /// GFX WireToSurface1 uncompressed dirty rects (client without AVC).
    Uncompressed,
    /// GFX expected or switching: send nothing (mixing bitmaps and GFX breaks clients).
    Hold,
    /// No GFX possible: legacy bitmap updates.
    Bitmap,
}

fn route(gfx_ready: bool, avc_ready: bool, has_encoder: bool, uncompressed: bool) -> Route {
    if avc_ready && has_encoder {
        Route::H264
    } else if gfx_ready && uncompressed {
        Route::Uncompressed
    } else if has_encoder || uncompressed {
        Route::Hold
    } else {
        Route::Bitmap
    }
}

/// GFX is confirmed but H.264 cannot be used: switch capture to BGRA uncompressed.
fn needs_uncompressed(gfx_ready: bool, avc_ready: bool, has_encoder: bool) -> bool {
    gfx_ready && !(avc_ready && has_encoder)
}

/// Most bytes of pixels one uncompressed GFX frame carries; the rest follows next frame,
/// so a full-screen change does not hold the session writer (and input) for long.
const MAX_UNCOMPRESSED_BYTES: usize = 1 << 20;
/// Merge two dirty rects when their bounding box wastes at most this many pixels.
const MERGE_SLACK_PX: u64 = 64 * 64;
/// More rects than this after merging collapse to their bounding box.
const MAX_DIRTY_RECTS: usize = 32;

fn area(r: &Rect) -> u64 {
    u64::from(r.width) * u64::from(r.height)
}

fn bounding_box(rects: &[Rect]) -> Rect {
    let x = rects.iter().map(|r| r.x).min().unwrap_or(0);
    let y = rects.iter().map(|r| r.y).min().unwrap_or(0);
    let right = rects.iter().map(|r| r.x + r.width).max().unwrap_or(0);
    let bottom = rects.iter().map(|r| r.y + r.height).max().unwrap_or(0);
    Rect { x, y, width: right - x, height: bottom - y }
}

/// Region to send for a `w`x`h` frame: the whole frame when `full`, else the dirty rects
/// clipped to the frame and coalesced (tiny neighbours merged, many rects collapsed).
fn plan_rects(dirty: &[Rect], full: bool, w: u32, h: u32) -> Vec<Rect> {
    if full {
        return vec![Rect { x: 0, y: 0, width: w, height: h }];
    }
    let mut rects: Vec<Rect> = dirty
        .iter()
        .filter(|r| r.x < w && r.y < h)
        .map(|r| Rect { x: r.x, y: r.y, width: r.width.min(w - r.x), height: r.height.min(h - r.y) })
        .filter(|r| r.width > 0 && r.height > 0)
        .collect();
    // Bounds the pairwise merge below.
    if rects.len() > MAX_DIRTY_RECTS * 4 {
        return vec![bounding_box(&rects)];
    }
    'merge: loop {
        for i in 0..rects.len() {
            for j in i + 1..rects.len() {
                let union = bounding_box(&[rects[i].clone(), rects[j].clone()]);
                if area(&union) <= area(&rects[i]) + area(&rects[j]) + MERGE_SLACK_PX {
                    rects[i] = union;
                    rects.swap_remove(j);
                    continue 'merge;
                }
            }
        }
        break;
    }
    if rects.len() > MAX_DIRTY_RECTS {
        return vec![bounding_box(&rects)];
    }
    rects
}

/// Split `rects` into what fits in `budget` bytes of BGRA (whole rows) and what carries over.
/// The first rect always sends at least one row, so a flush always progresses.
fn split_budget(rects: Vec<Rect>, budget: usize) -> (Vec<Rect>, Vec<Rect>) {
    let (mut send, mut carry) = (Vec::new(), Vec::new());
    let mut left = budget;
    for r in rects {
        let row = r.width as usize * 4;
        let mut rows = (left / row).min(r.height as usize) as u32;
        if rows == 0 && send.is_empty() {
            rows = 1;
        }
        if rows == 0 {
            carry.push(r);
            continue;
        }
        left = left.saturating_sub(rows as usize * row);
        if rows < r.height {
            carry.push(Rect { y: r.y + rows, height: r.height - rows, ..r.clone() });
        }
        send.push(Rect { height: rows, ..r });
    }
    (send, carry)
}

/// Copy each rect's rows out of a BGRA frame (`stride` bytes per row), tightly packed.
/// Rects that do not fit in `px` are skipped.
fn copy_rects(px: &[u8], stride: usize, rects: &[Rect]) -> Vec<UncompressedRect> {
    rects
        .iter()
        .filter_map(|r| {
            let row = r.width as usize * 4;
            let mut data = Vec::with_capacity(row * r.height as usize);
            for y in r.y..r.y + r.height {
                let start = y as usize * stride + r.x as usize * 4;
                data.extend_from_slice(px.get(start..start + row)?);
            }
            Some(UncompressedRect {
                x: r.x as u16,
                y: r.y as u16,
                width: r.width as u16,
                height: r.height as u16,
                pixel_data: Bytes::from(data),
            })
        })
        .collect()
}

/// RemoteFX tiles covering `plan` (rects inside the frame) of a BGRA frame, read straight from
/// the locked pixel buffer. None when the frame is not BGRA.
fn encode_remotefx(frame: &CapturedFrame, plan: &[Rect], quality: RfxQuality) -> Option<Result<GfxRemoteFxUpdate>> {
    let rects: Vec<RfxRectangle> = plan
        .iter()
        .map(|r| RfxRectangle { x: r.x as u16, y: r.y as u16, width: r.width as u16, height: r.height as u16 })
        .collect();
    let (w, h) = (frame.width as u16, frame.height as u16);
    let encode = |px: &[u8], stride: usize| {
        ironrdp_server::gfx::encode_gfx_progressive(px, stride, w, h, &rects, quality).map_err(|e| anyhow::anyhow!("{e}"))
    };
    match &frame.data {
        FrameData::PixelBuffer(buf) => buf.with_bgra(|px, stride, _, _| encode(px, stride)),
        FrameData::Raw(bytes) => Some(encode(bytes, frame.stride)),
    }
}

/// Display adapter that bridges ScreenCapturer to ironrdp-server
pub struct MacDisplay {
    width: u16,
    height: u16,
    /// Whether resolution is fixed by config (true) or follows client (false)
    fixed_resolution: bool,
    /// Configured bitrate; when set it wins over the size-derived base bitrate.
    bitrate_override: Option<u32>,
    /// Latest session layout; the running pipeline rebuilds when it changes.
    layout_tx: tokio::sync::watch::Sender<TargetLayout>,
    frame_rate: u32,
    quality: Quality,
    encoder_pref: macrdp_encode::EncoderPreference,
    /// Whether AVC444 mode is requested by config
    mode_444: bool,
    show_cursor: bool,
    base_bitrate: u32,
    gfx_state: Arc<Mutex<GfxState>>,
    coord_mapper: MouseCoordMapper,
    /// Audio sender while a client has RDPSND open (None = audio disabled in config).
    audio_tap: Option<AudioTap>,
    /// Shared performance statistics collector (None = disabled)
    perf_stats: Option<SharedPerfStats>,
    /// Closes when the previous session's pipeline thread has exited.
    prev_pipeline: Option<tokio::sync::oneshot::Receiver<()>>,
    /// Display mode for the next session (hot-updated from the settings).
    display_mode: Arc<Mutex<DisplayMode>>,
}

/// Longest a new session waits for the previous pipeline (an uncancellable rebuild is ~4 s).
const PREV_PIPELINE_WAIT: Duration = Duration::from_secs(10);

impl MacDisplay {
    pub fn new(
        width: u16, height: u16,
        fixed_resolution: bool,
        frame_rate: u32, quality: Quality,
        encoder_pref: macrdp_encode::EncoderPreference,
        mode_444: bool,
        show_cursor: bool,
        bitrate_override: Option<u32>,
        gfx_state: Arc<Mutex<GfxState>>,
        coord_mapper: MouseCoordMapper,
        audio_tap: Option<AudioTap>,
        perf_stats: Option<SharedPerfStats>,
    ) -> Self {
        let base_bitrate = base_bitrate(bitrate_override, width as u32, height as u32, frame_rate as f32, quality);
        tracing::info!(base_bitrate_mbps = base_bitrate as f64 / 1_000_000.0, "Base bitrate");
        let (layout_tx, _) = tokio::sync::watch::channel(TargetLayout { width, height, hidpi: false });
        Self {
            width, height,
            fixed_resolution,
            bitrate_override,
            layout_tx,
            frame_rate, quality, encoder_pref, mode_444, show_cursor, base_bitrate, gfx_state,
            coord_mapper,
            audio_tap,
            perf_stats,
            prev_pipeline: None,
            display_mode: Arc::default(),
        }
    }

    /// Share the display mode cell the settings update.
    pub fn with_display_mode(mut self, mode: Arc<Mutex<DisplayMode>>) -> Self {
        self.display_mode = mode;
        self
    }
}

/// Base bitrate for a size; a configured override always wins.
fn base_bitrate(override_bps: Option<u32>, w: u32, h: u32, fps: f32, quality: Quality) -> u32 {
    override_bps.unwrap_or_else(|| macrdp_encode::screen_bitrate(w, h, fps, quality))
}

impl MacDisplay {
    /// Adopt a validated size and publish it to the running pipeline.
    fn apply_layout(&mut self, target: TargetLayout, source: &str) {
        let (w, h) = resolution::clamp_desktop_size(target.width as u32, target.height as u32);
        let target = TargetLayout { width: w, height: h, ..target };
        if w != self.width || h != self.height {
            tracing::info!(
                old_w = self.width, old_h = self.height,
                new_w = w, new_h = h,
                hidpi = target.hidpi,
                source,
                "Applying resolution change"
            );
            self.width = w;
            self.height = h;
            self.base_bitrate = base_bitrate(self.bitrate_override, w as u32, h as u32, self.frame_rate as f32, self.quality);
        }
        self.layout_tx.send_replace(target);
    }

    /// Frame rate and cursor changed in the settings since the last session start.
    fn adopt_hot_config(&mut self) {
        let (show_cursor, fps) = {
            let mut gs = self.gfx_state.lock().unwrap();
            (gs.show_cursor.take(), gs.next_frame_rate.take())
        };
        if let Some(show) = show_cursor {
            self.show_cursor = show;
        }
        if let Some(fps) = fps {
            self.frame_rate = fps.clamp(1, 120);
            self.base_bitrate = base_bitrate(
                self.bitrate_override, self.width as u32, self.height as u32, self.frame_rate as f32, self.quality,
            );
        }
    }

    fn apply_resize(&mut self, width: u16, height: u16, source: &str) {
        let hidpi = self.layout_tx.borrow().hidpi;
        self.apply_layout(TargetLayout { width, height, hidpi }, source);
    }
}

#[async_trait::async_trait]
impl RdpServerDisplay for MacDisplay {
    async fn size(&mut self) -> DesktopSize {
        DesktopSize { width: self.width, height: self.height }
    }

    fn request_layout(&mut self, layout: ironrdp_server::DisplayControlMonitorLayout) {
        if self.fixed_resolution {
            tracing::info!("Ignoring monitor layout — resolution is fixed by config");
            return;
        }
        let Some(target) = resolution::layout_target(&layout) else {
            tracing::warn!("Monitor layout without monitors ignored");
            return;
        };
        self.apply_layout(target, "monitor-layout");
    }

    // The layout watch outlives sessions: HiDPI comes from this client, never the previous one.
    fn begin_session(&mut self, desktop_scale_factor: Option<u32>) {
        let hidpi = !self.fixed_resolution && desktop_scale_factor.is_some_and(|s| s > 100);
        self.layout_tx.send_if_modified(|l| core::mem::replace(&mut l.hidpi, hidpi) != hidpi);
    }

    fn request_resize(&mut self, width: u16, height: u16) {
        if self.fixed_resolution {
            tracing::debug!("Ignoring resize request — resolution is fixed by config");
            return;
        }
        self.apply_resize(width, height, "client-requested");
    }

    // Hot-config size for the next connection; fixed mode comes only from config at startup.
    fn set_size(&mut self, width: u16, height: u16) {
        self.apply_resize(width, height, "server-config");
    }

    async fn updates(&mut self) -> Result<Box<dyn RdpServerDisplayUpdates>> {
        // A replaced session's pipeline may still be rebuilding: its virtual display, display
        // configuration restore and mouse geometry must be gone before this session sets its own.
        if let Some(prev) = self.prev_pipeline.take() {
            if tokio::time::timeout(PREV_PIPELINE_WAIT, prev).await.is_err() {
                tracing::warn!("Previous display pipeline still running — starting the new one anyway");
            }
        }
        self.adopt_hot_config();
        let capture_config = CaptureConfig {
            width: self.width as u32,
            height: self.height as u32,
            frame_rate: self.frame_rate,
            // Chosen by the pipeline once it knows which encoder it got.
            pixel_format: CapturePixelFormat::Bgra,
            show_cursor: self.show_cursor,
            // The pipeline points it at the session's virtual display.
            display_id: None,
        };

        let bitrate_ctrl = BitrateController::new(self.base_bitrate, capture_config.frame_rate);

        let setup = PipelineSetup {
            capture_config,
            audio_rx: self.audio_tap.as_ref().map(|tap| tap.subscribe()),
            layout: *self.layout_tx.borrow(),
            layout_rx: self.layout_tx.subscribe(),
            frame_rate: self.frame_rate as f32,
            quality: self.quality,
            encoder_pref: self.encoder_pref,
            mode_444: self.mode_444,
            base_bitrate: self.base_bitrate,
            bitrate_override: self.bitrate_override,
            gfx_state: Arc::clone(&self.gfx_state),
            perf_stats: self.perf_stats.clone(),
            bitrate_ctrl,
            coord_mapper: self.coord_mapper.clone(),
            display_mode: *self.display_mode.lock().unwrap(),
        };

        // Capture + encode run on their own thread so encoding never stalls the session
        // runtime (input, acks, socket writes). Capacity 1: at most one finished update waits.
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let (init_tx, init_rx) = tokio::sync::oneshot::channel();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
        // Counted from here, so a session ending right away still waits for this thread's teardown.
        let live = crate::curtain::SCREEN.pipeline_started();
        std::thread::Builder::new()
            .name("macrdp-pipeline".into())
            .spawn(move || {
                // Dropped last, also on panic: the screen (input block, virtual display) is gone.
                let _live = live;
                run_pipeline_thread(setup, tx, init_tx, done_tx);
            })
            .context("failed to spawn display pipeline thread")?;
        self.prev_pipeline = Some(done_rx);
        init_rx.await.context("display pipeline thread exited during setup")??;

        Ok(Box::new(MacDisplayUpdates { rx }))
    }
}

/// Everything the pipeline thread needs to build its capturer and encoder.
struct PipelineSetup {
    capture_config: CaptureConfig,
    audio_rx: Option<AudioRx>,
    /// Size at start; equals the capture size.
    layout: TargetLayout,
    layout_rx: tokio::sync::watch::Receiver<TargetLayout>,
    frame_rate: f32,
    quality: Quality,
    encoder_pref: macrdp_encode::EncoderPreference,
    mode_444: bool,
    base_bitrate: u32,
    bitrate_override: Option<u32>,
    gfx_state: Arc<Mutex<GfxState>>,
    perf_stats: Option<SharedPerfStats>,
    bitrate_ctrl: BitrateController,
    coord_mapper: MouseCoordMapper,
    display_mode: DisplayMode,
}

/// `done` closes once capture and encoding have stopped (the next session may start). A replaced
/// curtain session then keeps its screen until the next session's display took over.
fn run_pipeline_thread(
    setup: PipelineSetup,
    tx: tokio::sync::mpsc::Sender<DisplayUpdate>,
    init_tx: tokio::sync::oneshot::Sender<Result<()>>,
    done: tokio::sync::oneshot::Sender<()>,
) {
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            let _ = init_tx.send(Err(e.into()));
            return;
        }
    };
    let screen = rt.block_on(async move {
        let mut pipeline = match Pipeline::start(setup).await {
            Ok(p) => p,
            Err(e) => {
                let _ = init_tx.send(Err(e));
                return None;
            }
        };
        if init_tx.send(Ok(())).is_ok() {
            pipeline.run(tx).await;
        }
        Some(pipeline.into_screen())
    });
    if let Some((mode, input_block, virtual_display)) = screen {
        let held = crate::curtain::SCREEN.park_if_replaced(mode == DisplayMode::Curtain);
        drop(done);
        if held {
            tracing::info!("Session replaced: the curtain stays up for the new connection");
            crate::curtain::SCREEN.wait_released(crate::curtain::HOLD_LIMIT);
        }
        // Local input first, then the displays (restored before the virtual display goes).
        drop(input_block);
        drop(virtual_display);
    }
    tracing::debug!("Display pipeline thread stopped");
}

/// Session-side end of the pipeline: only receives finished updates.
struct MacDisplayUpdates {
    rx: tokio::sync::mpsc::Receiver<DisplayUpdate>,
}

#[async_trait::async_trait]
impl RdpServerDisplayUpdates for MacDisplayUpdates {
    async fn next_update(&mut self) -> Result<Option<DisplayUpdate>> {
        // Cancel-safe; None (pipeline ended) ends the session.
        Ok(self.rx.recv().await)
    }
}

/// Cached frame data for idle IDR re-encoding (refcounted, no per-frame copy).
enum CachedFrame {
    Bgra { data: Bytes, width: u32, height: u32, stride: usize },
    PixelBuffer(SafePixelBuffer, u32, u32), // buffer, width, height
}

/// IDR keepalive interval during idle scenes (seconds).
const IDLE_IDR_INTERVAL: Duration = Duration::from_secs(2);

/// Capture + encode loop, owned by the `macrdp-pipeline` thread.
struct Pipeline {
    capturer: ScreenCapturer,
    capture_config: CaptureConfig,
    encoder: Option<Box<dyn VideoEncoder>>,
    gfx_state: Arc<Mutex<GfxState>>,
    mode_444: bool,
    display_frame_count: u64,
    /// Consecutive idle events from SCK (desktop unchanged).
    idle_frame_count: u32,
    /// When the last IDR keyframe was sent (natural or keepalive).
    last_idr_time: Instant,
    /// Cached last frame for idle IDR re-encoding.
    last_frame_cache: Option<CachedFrame>,
    /// Adaptive bitrate/fps controller.
    bitrate_ctrl: BitrateController,
    /// Last fps value applied to the capturer (to avoid redundant updates).
    last_applied_fps: u32,
    /// Captures replaced by a newer one while the ack gate was closed.
    coalesced: u32,
    /// Auto RemoteFX quality; None while the config fixes the level.
    rfx_ctrl: Option<RfxQualityController>,
    /// Shared performance statistics collector (None = disabled)
    perf_stats: Option<SharedPerfStats>,
    /// Session layout the capturer and encoder are built for.
    current: TargetLayout,
    /// Client layout changes; taken by run().
    layout_rx: Option<tokio::sync::watch::Receiver<TargetLayout>>,
    /// Audio sender changes (client opened or closed RDPSND); taken by run().
    audio_rx: Option<AudioRx>,
    /// Where captured audio goes; capture runs with audio only while this is set.
    audio_tx: Option<tokio::sync::mpsc::Sender<macrdp_capture::AudioFrame>>,
    /// Restarting the stream to switch audio failed once: keep the current stream.
    audio_switch_failed: bool,
    frame_rate: f32,
    quality: Quality,
    encoder_pref: macrdp_encode::EncoderPreference,
    bitrate_override: Option<u32>,
    coord_mapper: MouseCoordMapper,
    /// Virtual display creation failed for the current layout; retried on the next one.
    virtual_failed: bool,
    /// GFX without usable AVC: BGRA pixel-buffer capture, RemoteFX (or uncompressed) dirty
    /// rects, no H.264 encoder.
    uncompressed: bool,
    /// Uncompressed path: region changed since it was last sent (`dirty_full` = whole frame).
    dirty: Vec<Rect>,
    dirty_full: bool,
    /// Uncompressed path: newest frame while `dirty` still holds rects over the frame budget.
    carry_frame: Option<CapturedFrame>,
    /// RemoteFX encoding failed once: this session sends uncompressed rects instead.
    rfx_failed: bool,
    display_mode: DisplayMode,
    /// Curtain: local input blocked for the session. Dropped before the display, so local input
    /// comes back before the physical displays do.
    input_block: Option<LocalInputBlocker>,
    /// Session display. Declared last: fields drop in order, so the capturer stops before the
    /// display configuration is restored (session end, error or panic unwind).
    virtual_display: Option<VirtualDisplay>,
}

/// A new virtual display sized for `target`; None means the main display (letterboxed).
/// A failure sets `failed`, so recovery loops do not retry until the next layout.
fn prepare_virtual(failed: &mut bool, target: TargetLayout, mode: DisplayMode) -> Option<VirtualDisplay> {
    let vd = if *failed { None } else { create_virtual(failed, target, mode) };
    // This display now owns the configuration (or there is none): a replaced session's kept
    // curtain may go.
    crate::curtain::SCREEN.release();
    vd
}

fn create_virtual(failed: &mut bool, target: TargetLayout, mode: DisplayMode) -> Option<VirtualDisplay> {
    let (w, h) = (u32::from(target.width), u32::from(target.height));
    crate::lifecycle::wake_displays();
    // Always a new display (no in-place resize, see VirtualDisplay). It is configured while the
    // old one still exists; the old one's drop then leaves the configuration alone.
    match VirtualDisplay::create(w, h, target.hidpi).and_then(|mut d| d.configure(mode).map(|()| d)) {
        Ok(d) => {
            // Mirroring brings an offline display up, so its mode is awaited only now.
            d.wait_for_mode();
            Some(d)
        }
        Err(e) => {
            tracing::warn!(error = format!("{e:#}"), "Virtual display unavailable — capturing the physical display letterboxed");
            *failed = true;
            None
        }
    }
}

/// Curtain input block; without Accessibility the session continues with local input live.
fn block_local_input() -> Option<LocalInputBlocker> {
    LocalInputBlocker::install()
        .map_err(|e| tracing::warn!(error = format!("{e:#}"), "Cannot block local input for the curtain; local keyboard and mouse stay live"))
        .ok()
}

/// Capture for `target`: on a new virtual display when possible, else on the main display.
/// Returns the display to keep; the caller drops any old one only after swapping capturers,
/// so the old capture and display survive a failure here.
async fn open_capture(
    failed: &mut bool,
    target: TargetLayout,
    mode: DisplayMode,
    mut config: CaptureConfig,
    audio_tx: Option<tokio::sync::mpsc::Sender<macrdp_capture::AudioFrame>>,
) -> Result<(ScreenCapturer, CaptureConfig, Option<VirtualDisplay>)> {
    if let Some(vd) = prepare_virtual(failed, target, mode) {
        config.display_id = Some(vd.id());
        match ScreenCapturer::new(config.clone(), audio_tx.clone()).await {
            Ok(c) => return Ok((c, config, Some(vd))),
            Err(e) => {
                tracing::warn!(error = format!("{e:#}"), "Virtual display capture failed — capturing the physical display letterboxed");
                *failed = true;
            }
        }
    }
    config.display_id = None;
    Ok((ScreenCapturer::new(config.clone(), audio_tx).await?, config, None))
}

/// H.264 encoder for a size plus the capture pixel format it takes.
fn encoder_and_format(
    w: u32, h: u32, fps: f32, quality: Quality,
    pref: macrdp_encode::EncoderPreference, mode_444: bool, bitrate: u32,
) -> (Option<Box<dyn VideoEncoder>>, CapturePixelFormat) {
    let encoder = macrdp_encode::create_encoder(w, h, fps, quality, pref, mode_444, bitrate).ok();
    // Zero-copy NV12 only when the encoder we actually got takes pixel buffers
    // (a VideoToolbox failure falls back to OpenH264, which needs BGRA).
    let format = if encoder.as_ref().is_some_and(|e| e.accepts_pixel_buffer()) && !mode_444 {
        CapturePixelFormat::Nv12
    } else {
        CapturePixelFormat::Bgra
    };
    (encoder, format)
}

/// Audio sender as published by the session's RDPSND handler.
type AudioRx = tokio::sync::watch::Receiver<Option<tokio::sync::mpsc::Sender<macrdp_capture::AudioFrame>>>;

/// Next value from a watch; None once its sender is gone, then pends forever.
async fn next_change<T: Clone>(rx: &mut Option<tokio::sync::watch::Receiver<T>>) -> Option<T> {
    let Some(r) = rx.as_mut() else { return std::future::pending().await };
    if r.changed().await.is_ok() {
        return Some(r.borrow_and_update().clone());
    }
    *rx = None;
    None
}

impl Pipeline {
    async fn start(s: PipelineSetup) -> Result<Self> {
        let (w, h) = (s.layout.width as u32, s.layout.height as u32);
        let (encoder, pixel_format) = encoder_and_format(
            w, h, s.frame_rate, s.quality, s.encoder_pref, s.mode_444, s.base_bitrate,
        );
        if encoder.is_some() {
            tracing::info!("H.264 encoder available — will use GFX path when client supports it");
        }

        let mut capture_config = s.capture_config;
        capture_config.pixel_format = pixel_format;
        let mut audio_rx = s.audio_rx;
        let audio_tx = audio_rx.as_mut().and_then(|r| r.borrow_and_update().clone());
        // Before the display: no local input slips in while the curtain comes up.
        let input_block = (s.display_mode == DisplayMode::Curtain).then(block_local_input).flatten();
        let mut virtual_failed = false;
        let (capturer, capture_config, virtual_display) = open_capture(
            &mut virtual_failed, s.layout, s.display_mode, capture_config, audio_tx.clone(),
        ).await?;
        s.coord_mapper.set_geometry(s.layout.width, s.layout.height, display_bounds(capture_config.display_id));

        let last_applied_fps = capture_config.frame_rate;
        Ok(Self {
            capturer,
            capture_config,
            encoder,
            gfx_state: s.gfx_state,
            mode_444: s.mode_444,
            display_frame_count: 0,
            idle_frame_count: 0,
            last_idr_time: Instant::now(),
            last_frame_cache: None,
            bitrate_ctrl: s.bitrate_ctrl,
            last_applied_fps,
            coalesced: 0,
            rfx_ctrl: None,
            perf_stats: s.perf_stats,
            current: s.layout,
            layout_rx: Some(s.layout_rx),
            audio_rx,
            audio_tx,
            audio_switch_failed: false,
            frame_rate: s.frame_rate,
            quality: s.quality,
            encoder_pref: s.encoder_pref,
            bitrate_override: s.bitrate_override,
            coord_mapper: s.coord_mapper,
            virtual_failed,
            uncompressed: false,
            dirty: Vec::new(),
            dirty_full: true,
            carry_frame: None,
            rfx_failed: false,
            display_mode: s.display_mode,
            input_block,
            virtual_display,
        })
    }

    /// What outlives capture and encoding at the pipeline's end: the mode, the input block and
    /// the session display. Everything else stops here.
    fn into_screen(mut self) -> (DisplayMode, Option<LocalInputBlocker>, Option<VirtualDisplay>) {
        (self.display_mode, self.input_block.take(), self.virtual_display.take())
    }

    /// Rebuild for a new layout. GFX clients get ResetGraphics from the next frame's new
    /// size; the bitmap path needs a deactivation-reactivation (returned update).
    async fn apply_layout(&mut self, target: TargetLayout) -> Option<DisplayUpdate> {
        if self.encoder.is_none() && !self.uncompressed {
            self.current = target;
            return Some(DisplayUpdate::Resize(DesktopSize { width: target.width, height: target.height }));
        }
        match self.rebuild(target).await {
            Ok(()) => tracing::info!(
                width = target.width, height = target.height, hidpi = target.hidpi,
                "Display pipeline rebuilt for new layout"
            ),
            Err(e) => tracing::warn!(error = format!("{e:#}"), "Layout rebuild failed — keeping the current size"),
        }
        None
    }

    async fn rebuild(&mut self, target: TargetLayout) -> Result<()> {
        let (w, h) = (target.width as u32, target.height as u32);
        let base = base_bitrate(self.bitrate_override, w, h, self.frame_rate, self.quality);
        let (encoder, pixel_format) = if self.uncompressed {
            (None, CapturePixelFormat::BgraBuffer)
        } else {
            let (encoder, format) = encoder_and_format(
                w, h, self.frame_rate, self.quality, self.encoder_pref, self.mode_444, base,
            );
            (Some(encoder.context("no H.264 encoder at the new size")?), format)
        };
        let mut config = self.capture_config.clone();
        config.width = w;
        config.height = h;
        config.pixel_format = pixel_format;
        // The new display and stream start before the old ones drop, so a failure keeps both.
        self.virtual_failed = false;
        let (capturer, config, virtual_display) = open_capture(
            &mut self.virtual_failed, target, self.display_mode, config, self.audio_tx.clone(),
        ).await?;
        // Old capture stops first, then its display goes.
        self.capturer = capturer;
        self.virtual_display = virtual_display;

        self.bitrate_ctrl = BitrateController::new(base, config.frame_rate);
        self.last_applied_fps = config.frame_rate;
        self.capture_config = config;
        self.encoder = encoder;
        self.last_frame_cache = None;
        self.reset_dirty();
        self.idle_frame_count = 0;
        self.coalesced = 0;
        self.current = target;
        self.coord_mapper.set_geometry(target.width, target.height, display_bounds(self.capture_config.display_id));
        self.gfx_state.lock().unwrap().request_idr();
        Ok(())
    }

    /// Produce updates until the session drops its receiver.
    async fn run(&mut self, tx: tokio::sync::mpsc::Sender<DisplayUpdate>) {
        let mut layout_rx = self.layout_rx.take();
        let mut audio_rx = self.audio_rx.take();
        let mut pending = Debounce::new(LAYOUT_DEBOUNCE);
        let mut first_sent = false;
        loop {
            // Wait until the session has taken the previous update before gating and encoding
            // the next one. A GFX frame counts toward the gate from hand-off (frame_handed_off),
            // since the slot frees on receive, before the session counts it in pending_acks.
            let Ok(permit) = tx.reserve().await else { return };
            // Caps arrive while capture may be idle; wake to pick the GFX path (once, until switched).
            let caps = Arc::clone(&self.gfx_state.lock().unwrap().caps_notify);
            let next = loop {
                let awaiting_caps = !self.uncompressed;
                let due = pending.due();
                let wake = tokio::time::Instant::from_std(due.unwrap_or_else(Instant::now));
                tokio::select! {
                    _ = tx.closed() => return,
                    target = next_change(&mut layout_rx) => {
                        if let Some(target) = target {
                            pending.push(target, Instant::now());
                        }
                    }
                    // next_update restarts the stream with or without audio.
                    Some(tx) = next_change(&mut audio_rx) => self.audio_tx = tx,
                    _ = tokio::time::sleep_until(wake), if due.is_some() => {
                        let Some(target) = pending.take_if_due(Instant::now()) else { continue };
                        // The rebuild cannot be cancelled; skip it, and stop after it, once the
                        // session is gone so the next session's pipeline is not held up.
                        if target != self.current && !tx.is_closed() {
                            let update = self.apply_layout(target).await;
                            if tx.is_closed() {
                                return;
                            }
                            if let Some(update) = update {
                                break Ok(Some(update));
                            }
                        }
                    }
                    _ = caps.notified(), if awaiting_caps => {}
                    next = self.next_update() => break next,
                }
            };
            match next {
                Ok(Some(update)) => {
                    permit.send(update);
                    if !std::mem::replace(&mut first_sent, true) {
                        // The client sees the screen now: a locked one gets its password typed.
                        crate::unlock::first_frame_sent();
                    }
                }
                Ok(None) => {}
                Err(e) => tracing::warn!(error = format!("{e:#}"), "display pipeline update failed"),
            }
        }
    }

    /// Next update to send, or None when this round produced nothing.
    async fn next_update(&mut self) -> Result<Option<DisplayUpdate>> {
        if let Err(e) = self.enter_uncompressed_if_needed().await {
            // Bounded retry rate: the next round tries again.
            tokio::time::sleep(Duration::from_secs(1)).await;
            return Err(e);
        }
        self.switch_audio_if_needed().await;
        // Rects left over from the frame budget go out before waiting for a new capture.
        if let Some(frame) = self.carry_frame.take() {
            let frame = self.latest(frame);
            let frame = self.wait_for_gate(frame).await;
            return self.encode_and_send(frame);
        }
        // Drain stale frames — always use the latest available frame.
        // If SCK capturer stops (e.g. screen locked), fall back to CGDisplayCreateImage
        // which works at the display level (including lock screen).
        let frame = loop {
            // Calculate timeout for idle IDR keepalive.
            // If we have a cached frame and are in idle state, wait at most until
            // the next IDR keepalive is due instead of blocking forever.
            let idle_deadline = if self.idle_frame_count > 0 && self.last_frame_cache.is_some() {
                let elapsed = self.last_idr_time.elapsed();
                if elapsed >= IDLE_IDR_INTERVAL {
                    // IDR keepalive is overdue — send it immediately
                    if let Some(update) = self.send_idle_idr() {
                        return Ok(Some(update));
                    }
                    // If send_idle_idr returned None (no encoder/gfx), just continue waiting
                    None
                } else {
                    Some(IDLE_IDR_INTERVAL - elapsed)
                }
            } else {
                None
            };

            let event = if let Some(timeout) = idle_deadline {
                match tokio::time::timeout(timeout, self.capturer.next_frame()).await {
                    Ok(e) => e,
                    Err(_elapsed) => {
                        // Timeout — time to send an idle IDR keepalive
                        if let Some(update) = self.send_idle_idr() {
                            return Ok(Some(update));
                        }
                        // No encoder/GFX available — just continue waiting
                        continue;
                    }
                }
            } else {
                self.capturer.next_frame().await
            };

            let Some(event) = event else {
                // SCK stopped — fall back to CoreGraphics capture (works on lock screen)
                if let Some(cg_frame) = self.cg_fallback().await {
                    self.note_dirty(&cg_frame);
                    return self.encode_and_send(cg_frame);
                }
                continue;
            };

            // Handle idle events (desktop unchanged)
            let frame = match event {
                macrdp_capture::CaptureEvent::Frame(f) => {
                    // Desktop became active — reset idle counter
                    if self.idle_frame_count > 0 {
                        tracing::debug!(
                            idle_frames = self.idle_frame_count,
                            "Desktop active again — forcing keyframe for clean recovery"
                        );
                        if let Some(enc) = &mut self.encoder {
                            enc.force_keyframe();
                        }

                        // Reset BitrateController and restore initial bitrate/fps
                        self.bitrate_ctrl.on_idle_recovery();
                        if let Some(enc) = &mut self.encoder {
                            enc.set_bitrate(self.bitrate_ctrl.current_bitrate());
                        }
                        let target_fps = self.bitrate_ctrl.target_fps();
                        if self.last_applied_fps != target_fps {
                            let _ = self.capturer.set_frame_rate(target_fps);
                            self.last_applied_fps = target_fps;
                        }
                    }
                    self.idle_frame_count = 0;
                    self.note_dirty(&f);
                    f
                }
                macrdp_capture::CaptureEvent::Idle => {
                    self.idle_frame_count += 1;
                    continue;
                }
            };

            // If newer frames are already buffered, use the newest: queuing adds latency.
            break self.latest(frame);
        };

        let frame = self.wait_for_gate(frame).await;
        self.encode_and_send(frame)
    }

    /// SCStream stopped: serve CoreGraphics frames until a new SCStream starts.
    /// Returns a frame to send, or None once the SCStream is back.
    async fn cg_fallback(&mut self) -> Option<CapturedFrame> {
        self.idle_frame_count = 0;
        tracing::warn!("SCStream stopped — switching to CoreGraphics fallback (lock screen?)");
        let fallback = CgFallbackCapturer::new(&self.capture_config);
        loop {
            // Same display (the session's virtual display stays the target) and audio kept.
            let mut recovered = ScreenCapturer::new(self.capture_config.clone(), self.audio_tx.clone())
                .await
                .map(|c| (c, self.capture_config.clone()));
            if recovered.as_ref().is_err_and(|e| e.is::<macrdp_capture::DisplayMissing>()) {
                tracing::warn!("Capture display vanished — recreating the session display");
                self.virtual_display = None;
                recovered = open_capture(
                    &mut self.virtual_failed, self.current, self.display_mode, self.capture_config.clone(),
                    self.audio_tx.clone(),
                ).await.map(|(c, config, vd)| {
                    self.virtual_display = vd;
                    (c, config)
                });
            }
            match recovered {
                Ok((new_capturer, config)) => {
                    tracing::info!("SCStream recovered — switching back from CoreGraphics");
                    self.capturer = new_capturer;
                    self.capture_config = config;
                    self.coord_mapper.set_geometry(
                        self.current.width, self.current.height, display_bounds(self.capture_config.display_id),
                    );
                    self.gfx_state.lock().unwrap().request_idr();
                    return None;
                }
                Err(_) => {
                    if self.gate_open() {
                        if let Some(cg_frame) = fallback.capture_frame() {
                            return Some(cg_frame);
                        }
                    }
                    tokio::time::sleep(fallback.frame_interval()).await;
                }
            }
        }
    }

    /// GFX ack gate; always open on the bitmap path.
    fn gate_open(&self) -> bool {
        let mut st = self.gfx_state.lock().unwrap();
        let gfx = matches!(
            route(st.is_ready(), st.avc_ready(), self.encoder.is_some(), self.uncompressed),
            Route::H264 | Route::Uncompressed
        );
        !gfx || st.can_send(Instant::now())
    }

    /// Hold `held` until the ack gate opens. Newer captures replace it before encoding,
    /// so dropped frames never break the H.264 reference chain.
    async fn wait_for_gate(&mut self, mut held: CapturedFrame) -> CapturedFrame {
        let notify = Arc::clone(&self.gfx_state.lock().unwrap().ack_notify);
        let mut capture_open = true;
        while !self.gate_open() {
            tokio::select! {
                _ = notify.notified() => {}
                ev = self.capturer.next_frame(), if capture_open => match ev {
                    Some(macrdp_capture::CaptureEvent::Frame(f)) => {
                        self.note_dirty(&f);
                        held = f;
                        self.coalesced += 1;
                    }
                    Some(macrdp_capture::CaptureEvent::Idle) => {}
                    None => capture_open = false,
                },
                // can_send reopens after ACK_STALL without progress
                _ = tokio::time::sleep(ironrdp_server::gfx::ACK_STALL) => {}
            }
        }
        held
    }

    /// Re-encode the cached last frame as an IDR keyframe (keepalive during idle).
    /// Returns `None` if no cached frame, GFX is not ready or the ack gate is closed.
    fn send_idle_idr(&mut self) -> Option<DisplayUpdate> {
        let cache = self.last_frame_cache.as_ref()?;
        let encoder = self.encoder.as_mut()?;

        {
            let mut state = self.gfx_state.lock().unwrap();
            if !state.avc_ready() || !state.can_send(Instant::now()) {
                return None;
            }
            // This frame is an IDR, so it satisfies any pending request.
            state.take_idr_request();
        }

        let t0 = Instant::now();
        encoder.force_keyframe();
        let result = match cache {
            CachedFrame::Bgra { data, width, height, stride } => {
                encoder.encode_bgra(data, *width, *height, *stride)
            }
            CachedFrame::PixelBuffer(buf, _w, _h) => {
                encoder.encode_pixel_buffer(buf.as_ptr(), true)
            }
        };

        match result {
            Ok(encoded) if !encoded.data.is_empty() => {
                let encode_ms = t0.elapsed().as_secs_f64() * 1000.0;
                tracing::debug!(
                    h264_bytes = encoded.data.len(),
                    encode_ms = format!("{:.1}", encode_ms),
                    idle_frames = self.idle_frame_count,
                    "Sent idle IDR keepalive"
                );
                self.last_idr_time = Instant::now();
                self.gfx_state.lock().unwrap().frame_handed_off();
                let (vis_w, vis_h) = match cache {
                    CachedFrame::Bgra { width, height, .. } => (*width as u16, *height as u16),
                    CachedFrame::PixelBuffer(_, w, h) => (*w as u16, *h as u16),
                };
                Some(DisplayUpdate::GfxFrame(GfxFrameUpdate {
                    h264_data: encoded.data,
                    width: vis_w,
                    height: vis_h,
                    enc_width: encoded.width as u16,
                    enc_height: encoded.height as u16,
                    is_keyframe: true,
                    h264_aux: None,
                }))
            }
            Ok(_) => {
                tracing::warn!("Idle IDR encode returned empty data");
                encoder.force_keyframe();
                None
            }
            Err(e) => {
                tracing::warn!("Idle IDR encode failed: {e}");
                encoder.force_keyframe();
                None
            }
        }
    }

    /// Evaluate the BitrateController window and apply any adaptive bitrate/fps changes.
    fn apply_adaptive_decision(&mut self) {
        if !self.bitrate_ctrl.should_evaluate() {
            return;
        }

        // RTT is for the stats display only; the controller reacts to congestion.
        if let Some(ps) = &self.perf_stats {
            let rtt = self.gfx_state.lock().unwrap().rtt_ewma_ms;
            if rtt > 0.0 {
                ps.lock().unwrap().record_rtt(rtt);
            }
        }

        let decision = self.bitrate_ctrl.evaluate();

        if let Some(ref mut enc) = self.encoder {
            enc.set_bitrate(decision.bitrate_bps);
        }

        if decision.fps != self.last_applied_fps {
            if let Err(e) = self.capturer.set_frame_rate(decision.fps) {
                tracing::warn!("Failed to update frame rate: {e}");
            } else {
                self.last_applied_fps = decision.fps;
                tracing::info!(
                    fps = decision.fps,
                    bitrate_mbps = decision.bitrate_bps as f64 / 1_000_000.0,
                    "Adaptive: fps/bitrate updated"
                );
            }
        }
    }

    /// Encode `frame` synchronously and return the update to send right away.
    fn encode_and_send(&mut self, frame: CapturedFrame) -> Result<Option<DisplayUpdate>> {
        // Check GFX state and AVC444 negotiation
        let (route, use_444, idr) = {
            let mut state = self.gfx_state.lock().unwrap();
            let route = route(state.is_ready(), state.avc_ready(), self.encoder.is_some(), self.uncompressed);
            let use_444 = self.mode_444
                && state.avc444_supported
                && state.avc444_enabled;
            let idr = route == Route::H264 && state.take_idr_request();
            (route, use_444, idr)
        };

        match route {
            Route::H264 => return Ok(self.encode_gfx(&frame, use_444, idr)),
            Route::Uncompressed => return Ok(self.send_uncompressed(frame)),
            // Never send bitmaps while GFX is expected: mixing bitmap and GFX causes
            // 0xd06 DECOMPRESSION_FAILED on reconnect.
            Route::Hold => return Ok(None),
            Route::Bitmap => {}
        }

        // Bitmap path (only when GFX is not available at all)
        // Requires BGRA raw bytes — PixelBuffer frames should not reach here
        let bgra_bitmap = match &frame.data {
            FrameData::Raw(bytes) => bytes,
            FrameData::PixelBuffer(_) => {
                tracing::warn!("PixelBuffer frame in bitmap path — should not happen");
                return Ok(None);
            }
        };

        if !frame.dirty_rects.is_empty() {
            // Find bounding box of all dirty rects to send a single update
            let mut min_x = frame.width;
            let mut min_y = frame.height;
            let mut max_x = 0u32;
            let mut max_y = 0u32;

            for r in &frame.dirty_rects {
                min_x = min_x.min(r.x);
                min_y = min_y.min(r.y);
                max_x = max_x.max(r.x + r.width);
                max_y = max_y.max(r.y + r.height);
            }

            // Clamp to frame bounds
            max_x = max_x.min(frame.width);
            max_y = max_y.min(frame.height);

            if max_x > min_x && max_y > min_y {
                let w = max_x - min_x;
                let h = max_y - min_y;
                let Some(width) = NonZeroU16::new(w as u16) else { return Ok(None) };
                let Some(height) = NonZeroU16::new(h as u16) else { return Ok(None) };

                // Extract only the dirty region from the full frame buffer
                let bpp = 4usize;
                let dirty_stride = w as usize * bpp;
                let mut dirty_data = Vec::with_capacity(dirty_stride * h as usize);
                for row in min_y..max_y {
                    let src_offset = row as usize * frame.stride + min_x as usize * bpp;
                    let src_end = src_offset + dirty_stride;
                    if src_end <= bgra_bitmap.len() {
                        dirty_data.extend_from_slice(&bgra_bitmap[src_offset..src_end]);
                    }
                }

                let Some(stride) = NonZeroUsize::new(dirty_stride) else { return Ok(None) };

                let update = BitmapUpdate {
                    x: min_x as u16,
                    y: min_y as u16,
                    width,
                    height,
                    format: RdpPixelFormat::BgrA32,
                    data: Bytes::from(dirty_data),
                    stride,
                };

                return Ok(Some(DisplayUpdate::Bitmap(update)));
            }
        }

        // No dirty rects available — send full frame (first frame or fallback)
        let Some(width) = NonZeroU16::new(frame.width as u16) else { return Ok(None) };
        let Some(height) = NonZeroU16::new(frame.height as u16) else { return Ok(None) };
        let Some(stride) = NonZeroUsize::new(frame.stride) else { return Ok(None) };

        let update = BitmapUpdate {
            x: 0,
            y: 0,
            width,
            height,
            format: RdpPixelFormat::BgrA32,
            data: bgra_bitmap.clone(),
            stride,
        };

        Ok(Some(DisplayUpdate::Bitmap(update)))
    }

    /// Client confirmed GFX without usable AVC: capture BGRA pixel buffers on the same display
    /// and release the H.264 encoder (no VideoToolbox session for a client that cannot decode it).
    async fn enter_uncompressed_if_needed(&mut self) -> Result<()> {
        if self.uncompressed {
            return Ok(());
        }
        let needed = {
            let st = self.gfx_state.lock().unwrap();
            needs_uncompressed(st.is_ready(), st.avc_ready(), self.encoder.is_some())
        };
        if !needed {
            return Ok(());
        }
        let mut config = self.capture_config.clone();
        config.pixel_format = CapturePixelFormat::BgraBuffer;
        let capturer = ScreenCapturer::new(config.clone(), self.audio_tx.clone())
            .await
            .context("BGRA capture for uncompressed GFX")?;
        // New stream runs before the old one stops (dropped here).
        self.capturer = capturer;
        self.capture_config = config;
        self.encoder = None;
        self.last_frame_cache = None;
        self.uncompressed = true;
        self.reset_dirty();
        tracing::info!(
            width = self.capture_config.width, height = self.capture_config.height,
            "GFX client without AVC: sending RemoteFX Progressive dirty tiles, H.264 encoder released"
        );
        Ok(())
    }

    /// Restart the stream on the same display when the client opened or closed RDPSND, so
    /// ScreenCaptureKit captures (and wakes for) audio only while a client listens.
    async fn switch_audio_if_needed(&mut self) {
        let wanted = self.audio_tx.is_some();
        if self.audio_switch_failed || self.capturer.captures_audio() == wanted {
            return;
        }
        match ScreenCapturer::new(self.capture_config.clone(), self.audio_tx.clone()).await {
            // The new stream runs before the old one stops (dropped here).
            Ok(capturer) => {
                self.capturer = capturer;
                tracing::info!(audio = wanted, "Capture restarted for the client's audio channel");
            }
            Err(e) => {
                tracing::warn!(error = format!("{e:#}"), audio = wanted, "Capture restart for audio failed — keeping the current stream");
                self.audio_switch_failed = true;
            }
        }
    }

    /// Uncompressed path: forget pending rects and send the whole next frame.
    fn reset_dirty(&mut self) {
        self.dirty.clear();
        self.dirty_full = true;
        self.carry_frame = None;
    }

    /// Record a received capture's changed region (uncompressed path only).
    fn note_dirty(&mut self, f: &CapturedFrame) {
        if !self.uncompressed {
            return;
        }
        if f.dirty_rects.is_empty() {
            self.dirty_full = true;
        } else {
            self.dirty.extend(f.dirty_rects.iter().cloned());
            if self.dirty.len() > MAX_DIRTY_RECTS * 4 {
                self.dirty = vec![bounding_box(&self.dirty)];
            }
        }
    }

    /// `frame`, or the newest capture already buffered behind it.
    fn latest(&mut self, mut frame: CapturedFrame) -> CapturedFrame {
        while let Some(ev) = self.capturer.try_next_frame() {
            if let macrdp_capture::CaptureEvent::Frame(newer) = ev {
                self.note_dirty(&newer);
                frame = newer;
            }
        }
        frame
    }

    /// In auto mode, steps `st.rfx_quality` with the connection before a RemoteFX frame.
    fn auto_rfx_quality(&mut self, st: &mut GfxState) {
        let coalesced = std::mem::take(&mut self.coalesced);
        if !st.rfx_auto {
            self.rfx_ctrl = None;
            return;
        }
        let ctrl = self.rfx_ctrl.get_or_insert_with(RfxQualityController::new);
        let sample = RfxSample { backlog: st.backlog(), coalesced, rtt_ms: st.rtt_ewma_ms, rtt_min_ms: st.rtt_min_ms() };
        if let Some(c) = ctrl.observe(sample, Instant::now()) {
            tracing::info!("RFX quality auto: {} → {} ({})", c.from.as_str(), c.to.as_str(), c.reason);
        }
        st.rfx_quality = ctrl.level();
    }

    /// Dirty rects of `frame` as one GFX RemoteFX update. If RemoteFX fails: one uncompressed
    /// update of at most MAX_UNCOMPRESSED_BYTES of pixels; the remainder is kept with the frame
    /// and sent next round.
    fn send_uncompressed(&mut self, frame: CapturedFrame) -> Option<DisplayUpdate> {
        let (w, h) = (frame.width as u16, frame.height as u16);
        let (full, quality) = {
            let gfx = Arc::clone(&self.gfx_state);
            let mut st = gfx.lock().unwrap();
            self.auto_rfx_quality(&mut st);
            // A new or resized surface starts black: paint all of it.
            (self.dirty_full || !st.surface_created || st.width != w || st.height != h, st.rfx_quality)
        };
        let plan = plan_rects(&self.dirty, full, frame.width, frame.height);
        if !self.rfx_failed && !plan.is_empty() {
            let t0 = Instant::now();
            match encode_remotefx(&frame, &plan, quality) {
                Some(Ok(update)) => {
                    // RemoteFX is small enough to send the whole region at once.
                    self.dirty.clear();
                    self.dirty_full = false;
                    self.carry_frame = None;
                    let encode_ms = t0.elapsed().as_secs_f64() * 1000.0;
                    tracing::debug!(
                        tiles = update.tiles, bytes = update.data.len(), full,
                        encode_ms = format!("{encode_ms:.1}"), "Display: sending GFX RemoteFX Progressive"
                    );
                    let mut st = self.gfx_state.lock().unwrap();
                    st.last_encode_ms = encode_ms;
                    st.last_frame_bytes = update.data.len() as u32;
                    st.frame_rate = self.last_applied_fps;
                    st.frame_handed_off();
                    return Some(DisplayUpdate::GfxRemoteFx(update));
                }
                Some(Err(e)) => {
                    tracing::warn!("RemoteFX encode failed, sending uncompressed rects from now on: {e:#}");
                    self.rfx_failed = true;
                }
                None => {} // not BGRA: the uncompressed path below reports it
            }
        }
        let (send, carry) = split_budget(plan, MAX_UNCOMPRESSED_BYTES);
        let rects = match &frame.data {
            FrameData::PixelBuffer(buf) => buf.with_bgra(|px, stride, _, _| copy_rects(px, stride, &send)),
            FrameData::Raw(bytes) => Some(copy_rects(bytes, frame.stride, &send)),
        };
        let Some(rects) = rects else {
            tracing::warn!("Uncompressed GFX: frame is not BGRA — skipped");
            return None;
        };
        self.dirty = carry;
        self.dirty_full = false;
        self.carry_frame = (!self.dirty.is_empty()).then_some(frame);
        if rects.is_empty() {
            return None;
        }
        let bytes: usize = rects.iter().map(|r| r.pixel_data.len()).sum();
        tracing::debug!(rects = rects.len(), bytes, full, carried = self.dirty.len(), "Display: sending GFX uncompressed");
        {
            let mut st = self.gfx_state.lock().unwrap();
            st.last_frame_bytes = bytes as u32;
            st.frame_rate = self.last_applied_fps;
            st.frame_handed_off();
        }
        Some(DisplayUpdate::GfxUncompressed(GfxUncompressedUpdate { rects, width: w, height: h }))
    }

    /// H.264 encode for the GFX channel: zero-copy NV12, AVC444 (falls back to AVC420), or AVC420.
    fn encode_gfx(&mut self, frame: &CapturedFrame, use_444: bool, idr: bool) -> Option<DisplayUpdate> {
        let encoder = self.encoder.as_mut()?;
        if idr {
            encoder.force_keyframe();
        }
        self.display_frame_count += 1;
        let t0 = Instant::now();

        let encoded = match &frame.data {
            FrameData::PixelBuffer(buf) => encoder.encode_pixel_buffer(buf.as_ptr(), false).map(|e| (e, None)),
            FrameData::Raw(bgra) => {
                let avc444 = if use_444 && encoder.supports_444() {
                    match encoder.encode_bgra_444(bgra, frame.width, frame.height, frame.stride) {
                        Ok(e) => Some(Ok((e.main_view, Some(e.aux_view.data)))),
                        Err(e) => {
                            tracing::warn!(display_frame = self.display_frame_count, "AVC444 encode failed: {e}, falling back to AVC420");
                            // The failed call may have consumed a pending IDR or left a frame the client never gets.
                            encoder.force_keyframe();
                            None
                        }
                    }
                } else {
                    None
                };
                avc444.unwrap_or_else(|| {
                    encoder.encode_bgra(bgra, frame.width, frame.height, frame.stride).map(|e| (e, None))
                })
            }
        };

        let (main, aux) = match encoded {
            Ok((main, aux)) if !main.data.is_empty() => (main, aux),
            // A dropped frame may still be the encoder's reference (or a consumed IDR request):
            // the next frame must be an IDR or the client decodes a broken P-chain until the GOP ends.
            Ok(_) => {
                tracing::warn!(display_frame = self.display_frame_count, "H.264 encode returned EMPTY data — frame dropped!");
                encoder.force_keyframe();
                return None;
            }
            Err(e) => {
                tracing::warn!(display_frame = self.display_frame_count, "H.264 encode failed: {e}");
                encoder.force_keyframe();
                return None;
            }
        };

        let encode_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let total_bytes = (main.data.len() + aux.as_ref().map_or(0, |a| a.len())) as u32;
        let is_keyframe = main.is_keyframe;
        tracing::debug!(
            display_frame = self.display_frame_count,
            h264_bytes = total_bytes,
            avc444 = aux.is_some(),
            is_keyframe,
            coalesced = self.coalesced,
            encode_ms = format!("{:.1}", encode_ms),
            "Display: sending GFX frame"
        );
        let pending_acks = {
            let mut st = self.gfx_state.lock().unwrap();
            st.last_encode_ms = encode_ms;
            st.last_frame_bytes = total_bytes;
            // Sizes the ack gate to the path: frames in flight at the minimum RTT are not backlog.
            st.frame_rate = self.last_applied_fps;
            let backlog = st.backlog();
            // Returned below and sent at once; gated as pending until the session counts it.
            st.frame_handed_off();
            backlog
        };
        self.bitrate_ctrl.record_frame(FrameStats {
            pending_acks,
            coalesced: std::mem::take(&mut self.coalesced),
            frame_bytes: total_bytes,
            is_keyframe,
        });
        if let Some(ps) = &self.perf_stats {
            ps.lock().unwrap().record_frame(encode_ms, total_bytes, is_keyframe);
        }
        self.apply_adaptive_decision();

        // Cache frame for idle IDR re-encoding (refcount clone, no copy)
        self.last_frame_cache = Some(match &frame.data {
            FrameData::PixelBuffer(buf) => CachedFrame::PixelBuffer(buf.clone_ref(), frame.width, frame.height),
            FrameData::Raw(bytes) => CachedFrame::Bgra {
                data: bytes.clone(), width: frame.width, height: frame.height, stride: frame.stride,
            },
        });
        if is_keyframe {
            self.last_idr_time = Instant::now();
        }

        Some(DisplayUpdate::GfxFrame(GfxFrameUpdate {
            h264_data: main.data,
            width: frame.width as u16,
            height: frame.height as u16,
            enc_width: main.width as u16,
            enc_height: main.height as u16,
            is_keyframe,
            h264_aux: aux,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_frame_to_bitmap_updates() {
        let frame = CapturedFrame {
            width: 100,
            height: 50,
            data: FrameData::Raw(Bytes::from(vec![0u8; 100 * 50 * 4])),
            stride: 400,
            timestamp_us: 0,
            dirty_rects: vec![],
        };

        let updates = frame_to_bitmap_updates(&frame, 64);
        assert_eq!(updates.len(), 2);
        assert_eq!(updates[0].x, 0);
        assert_eq!(updates[0].width.get(), 64);
        assert_eq!(updates[1].x, 64);
        assert_eq!(updates[1].width.get(), 36);
    }

    fn display() -> MacDisplay {
        MacDisplay::new(
            1920, 1080, false, 60, Quality::Balanced, macrdp_encode::EncoderPreference::Software,
            false, false, None, Arc::new(Mutex::new(GfxState::new(1920, 1080, false))),
            MouseCoordMapper::new(1920, 1080, 1920, 1080), None, None,
        )
    }

    fn hidpi(d: &MacDisplay) -> bool {
        d.layout_tx.borrow().hidpi
    }

    #[test]
    fn new_session_does_not_inherit_hidpi() {
        let mut d = display();
        let layout = ironrdp_server::DisplayControlMonitorLayout::new_single_primary_monitor(2560, 1600, Some(200), None)
            .expect("valid layout");
        d.request_layout(layout);
        assert!(hidpi(&d));

        d.begin_session(None);
        d.request_resize(1920, 1080);
        assert!(!hidpi(&d));
    }

    #[test]
    fn session_hidpi_follows_client_scale_factor() {
        let mut d = display();
        d.begin_session(Some(200));
        assert!(hidpi(&d));
        d.request_resize(2560, 1600);
        assert!(hidpi(&d));
        d.begin_session(Some(100));
        assert!(!hidpi(&d));
    }

    #[test]
    fn hot_frame_rate_and_cursor_apply_to_the_next_session() {
        let mut d = display();
        d.adopt_hot_config();
        assert_eq!((d.frame_rate, d.show_cursor), (60, false));
        let base60 = d.base_bitrate;
        {
            let mut gs = d.gfx_state.lock().unwrap();
            gs.show_cursor = Some(true);
            gs.next_frame_rate = Some(15);
        }
        d.adopt_hot_config();
        assert_eq!((d.frame_rate, d.show_cursor), (15, true));
        assert!(d.base_bitrate < base60, "bitrate follows the frame rate");
        // Taken once: a later session keeps the adopted values.
        d.adopt_hot_config();
        assert_eq!((d.frame_rate, d.show_cursor), (15, true));
    }

    fn r(x: u32, y: u32, width: u32, height: u32) -> Rect {
        Rect { x, y, width, height }
    }

    #[test]
    fn route_picks_h264_only_for_avc_clients() {
        // (gfx_ready, avc_ready, has_encoder, uncompressed)
        assert_eq!(route(true, true, true, false), Route::H264);
        assert_eq!(route(true, false, false, true), Route::Uncompressed);
        // GFX up but not switched yet (frame may still be NV12): hold, never bitmaps.
        assert_eq!(route(true, false, true, false), Route::Hold);
        assert_eq!(route(false, false, true, false), Route::Hold);
        assert_eq!(route(false, false, false, true), Route::Hold);
        assert_eq!(route(false, false, false, false), Route::Bitmap);
    }

    #[test]
    fn switch_to_uncompressed_when_gfx_has_no_usable_avc() {
        // (gfx_ready, avc_ready, has_encoder)
        assert!(needs_uncompressed(true, false, true), "no-AVC client");
        assert!(needs_uncompressed(true, true, false), "AVC client but no encoder");
        assert!(!needs_uncompressed(true, true, true));
        assert!(!needs_uncompressed(false, false, true), "caps not confirmed yet");
    }

    #[test]
    fn plan_full_frame_when_full_or_no_rects() {
        assert_eq!(plan_rects(&[], true, 816, 1544), vec![r(0, 0, 816, 1544)]);
        assert_eq!(plan_rects(&[r(1, 1, 2, 2)], true, 816, 1544), vec![r(0, 0, 816, 1544)]);
        assert!(plan_rects(&[], false, 816, 1544).is_empty());
    }

    #[test]
    fn plan_clips_and_coalesces_tiny_rects() {
        // Adjacent tiny rects merge; a far one stays separate; out-of-frame parts are clipped.
        let got = plan_rects(&[r(10, 10, 4, 4), r(14, 10, 4, 4), r(500, 1500, 400, 100)], false, 816, 1544);
        assert_eq!(got, vec![r(10, 10, 8, 4), r(500, 1500, 316, 44)]);
        // Fully outside rects vanish.
        assert!(plan_rects(&[r(900, 0, 10, 10)], false, 816, 1544).is_empty());
    }

    #[test]
    fn plan_collapses_many_rects_to_bounding_box() {
        let many: Vec<Rect> = (0..200).map(|i| r((i % 20) * 40, (i / 20) * 150, 1, 1)).collect();
        assert_eq!(plan_rects(&many, false, 816, 1544), vec![r(0, 0, 761, 1351)]);
    }

    #[test]
    fn budget_splits_by_rows_and_always_progresses() {
        let row = 816 * 4;
        let (send, carry) = split_budget(vec![r(0, 0, 816, 1544)], row * 300 + 7);
        assert_eq!(send, vec![r(0, 0, 816, 300)]);
        assert_eq!(carry, vec![r(0, 300, 816, 1244)]);
        // A budget smaller than one row still sends one row.
        let (send, carry) = split_budget(vec![r(0, 0, 816, 2), r(0, 10, 1, 1)], 10);
        assert_eq!(send, vec![r(0, 0, 816, 1)]);
        assert_eq!(carry, vec![r(0, 1, 816, 1), r(0, 10, 1, 1)]);
        // Everything fits: nothing carried.
        let (send, carry) = split_budget(vec![r(1, 1, 2, 2), r(5, 5, 1, 1)], 1 << 20);
        assert_eq!(send.len(), 2);
        assert!(carry.is_empty());
    }

    #[test]
    fn copy_rects_reads_strided_rows_tightly_packed() {
        // 4x3 frame, stride 20 (4 bytes padding), pixel (x,y) = [x, y, 0, 0xFF].
        let stride = 20;
        let mut px = vec![0u8; stride * 3];
        for y in 0..3 {
            for x in 0..4 {
                px[y * stride + x * 4..y * stride + x * 4 + 4].copy_from_slice(&[x as u8, y as u8, 0, 0xFF]);
            }
        }
        let out = copy_rects(&px, stride, &[r(1, 1, 2, 2)]);
        assert_eq!(out.len(), 1);
        assert_eq!((out[0].x, out[0].y, out[0].width, out[0].height), (1, 1, 2, 2));
        assert_eq!(&out[0].pixel_data[..], &[1, 1, 0, 0xFF, 2, 1, 0, 0xFF, 1, 2, 0, 0xFF, 2, 2, 0, 0xFF]);
        // A rect past the buffer is skipped, not a panic.
        assert!(copy_rects(&px, stride, &[r(0, 2, 4, 5)]).is_empty());
    }

    #[test]
    fn remotefx_encodes_dirty_tiles_of_raw_bgra_frames() {
        let frame = |len: usize| CapturedFrame {
            width: 200,
            height: 130,
            data: FrameData::Raw(Bytes::from(vec![0x40u8; len])),
            stride: 800,
            timestamp_us: 0,
            dirty_rects: vec![],
        };
        let update = encode_remotefx(&frame(800 * 130), &[r(70, 10, 10, 10), r(0, 100, 5, 5)], RfxQuality::Medium)
            .expect("BGRA frame")
            .expect("encoded");
        assert_eq!(update.tiles, 2);
        assert_eq!((update.width, update.height), (200, 130));
        // A buffer shorter than the frame is an error (falls back), never a panic.
        assert!(encode_remotefx(&frame(800 * 10), &[r(0, 0, 200, 130)], RfxQuality::Medium).expect("BGRA frame").is_err());
    }

    #[test]
    fn test_frame_to_bitmap_updates_exact_tile() {
        let frame = CapturedFrame {
            width: 128,
            height: 64,
            data: FrameData::Raw(Bytes::from(vec![0u8; 128 * 64 * 4])),
            stride: 512,
            timestamp_us: 0,
            dirty_rects: vec![],
        };

        let updates = frame_to_bitmap_updates(&frame, 64);
        assert_eq!(updates.len(), 2);
    }
}
