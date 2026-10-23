//! macOS screen capture via ScreenCaptureKit

pub mod ddc;
pub mod virtual_display;
mod windows;

use std::ffi::c_void;
use std::sync::Arc;

use anyhow::{Context, Result};
use bytes::Bytes;
use core_graphics::access::ScreenCaptureAccess;
use screencapturekit::cv::{CVPixelBuffer, CVPixelBufferLockFlags};
use screencapturekit::prelude::*;
use screencapturekit::stream::configuration::{AudioChannelCount, AudioSampleRate};
use tokio::sync::mpsc;

/// Check if Screen Recording permission is granted (no prompt)
pub fn check_screen_recording_permission() -> bool {
    ScreenCaptureAccess::default().preflight()
}

/// Request Screen Recording permission (triggers system dialog if not granted)
/// Returns true if already granted. Note: even after granting, the app
/// may need to be restarted for the permission to take effect.
pub fn request_screen_recording_permission() -> bool {
    ScreenCaptureAccess::default().request()
}

/// ScreenCaptureKit without Screen Recording access shows the system prompt: callers that must
/// not prompt (server start) get an error instead.
fn ensure_screen_access() -> Result<()> {
    if !check_screen_recording_permission() {
        anyhow::bail!("Screen Recording not granted");
    }
    Ok(())
}

/// Open System Settings to Privacy & Security page
pub fn open_screen_recording_settings() {
    let _ = std::process::Command::new("open")
        .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture")
        .spawn();
}

/// A rectangle region
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// Pixel format for screen capture output
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CapturePixelFormat {
    /// BGRA 32-bit (default, needed for OpenH264 and bitmap fallback path)
    Bgra,
    /// NV12 (420f full-range) — zero-copy to VideoToolbox, no color conversion needed
    Nv12,
    /// BGRA delivered as the retained CVPixelBuffer (no copy): readers copy only what they need
    BgraBuffer,
}

/// Frame pixel data — either raw BGRA bytes or a zero-copy CVPixelBuffer reference
#[derive(Debug)]
pub enum FrameData {
    /// BGRA raw bytes copied from CVPixelBuffer (existing behavior)
    Raw(Bytes),
    /// IOSurface-backed CVPixelBuffer — zero copy, passed directly to VideoToolbox
    PixelBuffer(SafePixelBuffer),
}

impl FrameData {
    /// Get raw BGRA bytes if this is a Raw frame. Returns None for PixelBuffer frames.
    pub fn as_bgra_bytes(&self) -> Option<&[u8]> {
        match self {
            FrameData::Raw(bytes) => Some(bytes),
            FrameData::PixelBuffer(_) => None,
        }
    }
}

/// Events from the screen capture pipeline.
#[derive(Debug)]
pub enum CaptureEvent {
    /// A complete frame with pixel data ready for encoding.
    Frame(CapturedFrame),
    /// Desktop is unchanged — no new pixel data.
    Idle,
}

/// A captured screen frame
#[derive(Debug)]
pub struct CapturedFrame {
    pub width: u32,
    pub height: u32,
    pub data: FrameData,
    /// Bytes per row (valid for FrameData::Raw only)
    pub stride: usize,
    pub timestamp_us: u64,
    /// Regions that changed since the last frame.
    /// Empty means info unavailable — treat as full frame change.
    pub dirty_rects: Vec<Rect>,
}

/// Raw audio frame from ScreenCaptureKit audio callback.
/// Data is interleaved Float32 PCM (L0,R0, L1,R1, ...).
#[derive(Clone)]
pub struct AudioFrame {
    pub data: Vec<f32>,
    pub sample_rate: u32,
    pub channels: u16,
    pub num_samples: usize,
    pub timestamp_ms: u64,
}

/// Configuration for screen capture
#[derive(Clone)]
pub struct CaptureConfig {
    pub width: u32,
    pub height: u32,
    pub frame_rate: u32,
    pub pixel_format: CapturePixelFormat,
    pub show_cursor: bool,
    /// Display to capture (CGDirectDisplayID); None = the first shareable display.
    pub display_id: Option<u32>,
}

/// Screen capturer using ScreenCaptureKit
pub struct ScreenCapturer {
    stream: SCStream,
    frame_rx: mpsc::Receiver<CaptureEvent>,
    /// Raised by the stream delegate when ScreenCaptureKit stops the stream.
    stop_rx: tokio::sync::watch::Receiver<bool>,
    /// Stream settings kept so a frame-rate change rebuilds the full configuration.
    width: u32,
    height: u32,
    pixel_format: CapturePixelFormat,
    show_cursor: bool,
    captures_audio: bool,
}

/// Full SCStream configuration; every reconfiguration must go through here, because
/// update_configuration replaces all settings (size, format, cursor, audio).
fn stream_configuration(
    width: u32,
    height: u32,
    fps: u32,
    pixel_format: CapturePixelFormat,
    show_cursor: bool,
    captures_audio: bool,
) -> SCStreamConfiguration {
    let frame_interval = CMTime::new(1, fps as i32);
    SCStreamConfiguration::new()
        .with_width(width)
        .with_height(height)
        .with_scales_to_fit(true)
        // Letterbox when the display aspect differs from the session (mouse mapping assumes it).
        .with_preserves_aspect_ratio(true)
        .with_minimum_frame_interval(&frame_interval)
        .with_pixel_format(match pixel_format {
            CapturePixelFormat::Nv12 => PixelFormat::YCbCr_420f,
            CapturePixelFormat::Bgra | CapturePixelFormat::BgraBuffer => PixelFormat::BGRA,
        })
        .with_shows_cursor(show_cursor)
        .with_captures_audio(captures_audio)
        .with_sample_rate(AudioSampleRate::Rate48000)
        .with_channel_count(AudioChannelCount::Stereo)
        .with_excludes_current_process_audio(true)
}

struct VideoOutputHandler {
    frame_tx: mpsc::Sender<CaptureEvent>,
    pixel_format: CapturePixelFormat,
}

struct AudioOutputHandler {
    audio_tx: Arc<mpsc::Sender<AudioFrame>>,
}

impl SCStreamOutputTrait for VideoOutputHandler {
    fn did_output_sample_buffer(&self, sample: CMSampleBuffer, of_type: SCStreamOutputType) {
        if of_type != SCStreamOutputType::Screen {
            return;
        }

        let event = match self.pixel_format {
            CapturePixelFormat::Nv12 | CapturePixelFormat::BgraBuffer => extract_frame_nv12(&sample),
            CapturePixelFormat::Bgra => extract_frame(&sample),
        };
        let Some(event) = event else { return };

        // Non-blocking send — drop frame if channel is full
        let _ = self.frame_tx.try_send(event);
    }
}

impl SCStreamOutputTrait for AudioOutputHandler {
    fn did_output_sample_buffer(&self, sample: CMSampleBuffer, of_type: SCStreamOutputType) {
        if of_type != SCStreamOutputType::Audio {
            return;
        }

        let Some(audio_buffer_list) = sample.audio_buffer_list() else { return };

        let timestamp = sample.presentation_timestamp();
        let timestamp_ms = if timestamp.timescale > 0 {
            ((timestamp.value as u128 * 1000) / timestamp.timescale as u128) as u64
        } else {
            0
        };

        // Get sample rate and channel count from format description
        let (sample_rate, channels) = if let Some(fmt) = sample.format_description() {
            let sr = fmt.audio_sample_rate().unwrap_or(48000.0) as u32;
            let ch = fmt.audio_channel_count().unwrap_or(2) as u16;
            (sr, ch)
        } else {
            (48000u32, 2u16)
        };

        let num_buffers = audio_buffer_list.num_buffers();

        // SCK delivers stereo as a single interleaved buffer (num_buffers == 1, number_channels == 2)
        // or as separate per-channel buffers (num_buffers == 2, number_channels == 1 each).
        let interleaved_data = if num_buffers <= 1 {
            if let Some(buf) = audio_buffer_list.get(0) {
                let raw = buf.data();
                let samples: &[f32] = unsafe {
                    std::slice::from_raw_parts(raw.as_ptr() as *const f32, raw.len() / 4)
                };
                samples.to_vec()
            } else {
                return;
            }
        } else {
            // Non-interleaved: one buffer per channel — interleave manually
            let channel_bufs: Vec<&[f32]> = (0..num_buffers)
                .filter_map(|i| audio_buffer_list.get(i))
                .map(|buf| {
                    let raw = buf.data();
                    unsafe {
                        std::slice::from_raw_parts(raw.as_ptr() as *const f32, raw.len() / 4)
                    }
                })
                .collect();

            if channel_bufs.is_empty() {
                return;
            }
            let n = channel_bufs[0].len();
            let mut out = Vec::with_capacity(n * channel_bufs.len());
            for i in 0..n {
                for ch in &channel_bufs {
                    out.push(ch[i]);
                }
            }
            out
        };

        let num_samples = if channels > 0 {
            interleaved_data.len() / channels as usize
        } else {
            interleaved_data.len()
        };

        let frame = AudioFrame {
            data: interleaved_data,
            sample_rate,
            channels,
            num_samples,
            timestamp_ms,
        };
        let _ = self.audio_tx.try_send(frame);
    }
}

fn extract_frame(sample: &CMSampleBuffer) -> Option<CaptureEvent> {
    use screencapturekit::cm::SCFrameStatus;

    // Idle means desktop is unchanged — propagate as CaptureEvent::Idle.
    // Blank/Suspended/Stopped are non-displayable states — discard silently.
    match sample.frame_status() {
        Some(SCFrameStatus::Idle) => {
            return Some(CaptureEvent::Idle);
        }
        Some(SCFrameStatus::Blank)
        | Some(SCFrameStatus::Suspended)
        | Some(SCFrameStatus::Stopped) => {
            return None;
        }
        _ => {}
    }

    let pixel_buffer: CVPixelBuffer = sample.image_buffer()?;

    let guard = pixel_buffer.lock(CVPixelBufferLockFlags::READ_ONLY).ok()?;

    let width = guard.width() as u32;
    let height = guard.height() as u32;
    let stride = guard.bytes_per_row();
    let pixels = guard.as_slice();

    if width == 0 || height == 0 || pixels.is_empty() {
        return None;
    }

    // Extract dirty rects from the sample buffer
    // Extract dirty rects — screencapturekit's CGRect has x/y/width/height fields
    let dirty_rects = sample
        .dirty_rects()
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.width > 0.0 && r.height > 0.0)
        .map(|r| Rect {
            x: r.x.max(0.0) as u32,
            y: r.y.max(0.0) as u32,
            width: r.width as u32,
            height: r.height as u32,
        })
        .collect::<Vec<_>>();

    let data = Bytes::copy_from_slice(pixels);

    let t = sample.presentation_timestamp();
    let timestamp_us = if t.timescale > 0 {
        ((t.value as u128 * 1_000_000) / t.timescale as u128) as u64
    } else {
        0
    };

    Some(CaptureEvent::Frame(CapturedFrame {
        width,
        height,
        data: FrameData::Raw(data),
        stride,
        timestamp_us,
        dirty_rects,
    }))
}

/// Extract a frame in NV12 or BgraBuffer mode — zero-copy CVPixelBuffer wrapped as SafePixelBuffer.
/// The pixel buffer is retained and passed through the channel without locking or copying.
fn extract_frame_nv12(sample: &CMSampleBuffer) -> Option<CaptureEvent> {
    use screencapturekit::cm::SCFrameStatus;

    // Idle means desktop is unchanged — propagate as CaptureEvent::Idle.
    // Blank/Suspended/Stopped are non-displayable states — discard silently.
    match sample.frame_status() {
        Some(SCFrameStatus::Idle) => {
            return Some(CaptureEvent::Idle);
        }
        Some(SCFrameStatus::Blank)
        | Some(SCFrameStatus::Suspended)
        | Some(SCFrameStatus::Stopped) => {
            return None;
        }
        _ => {}
    }

    let pixel_buffer: CVPixelBuffer = sample.image_buffer()?;

    // width()/height() read from the CVPixelBuffer header — no lock required
    let width = pixel_buffer.width() as u32;
    let height = pixel_buffer.height() as u32;

    if width == 0 || height == 0 {
        return None;
    }

    // Extract dirty rects (same logic as the BGRA path)
    let dirty_rects = sample
        .dirty_rects()
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.width > 0.0 && r.height > 0.0)
        .map(|r| Rect {
            x: r.x.max(0.0) as u32,
            y: r.y.max(0.0) as u32,
            width: r.width as u32,
            height: r.height as u32,
        })
        .collect::<Vec<_>>();

    let t = sample.presentation_timestamp();
    let timestamp_us = if t.timescale > 0 {
        ((t.value as u128 * 1_000_000) / t.timescale as u128) as u64
    } else {
        0
    };

    // Zero-copy: retain the CVPixelBuffer and wrap it as SafePixelBuffer
    let safe_buf = unsafe { SafePixelBuffer::from_raw(pixel_buffer.as_ptr()) };

    Some(CaptureEvent::Frame(CapturedFrame {
        width,
        height,
        data: FrameData::PixelBuffer(safe_buf),
        stride: 0, // Not applicable for NV12 PixelBuffer mode
        timestamp_us,
        dirty_rects,
    }))
}

/// Detect the main display's native scale factor (1 for non-Retina, 2 for Retina).
pub fn detect_display_scale() -> Result<u32> {
    use core_graphics::display::CGDisplay;
    let main = CGDisplay::main();
    let physical_w = main.pixels_wide() as u32;
    ensure_screen_access()?;
    let content = SCShareableContent::get()
        .context("Failed to get shareable content")?;
    let display = content.displays().into_iter().next()
        .context("No display found")?;
    let logical_w = display.width();
    let scale = if logical_w > 0 { physical_w / logical_w } else { 1 };
    Ok(scale.max(1))
}

/// Query the main display's resolution (from ScreenCaptureKit, used for capture sizing)
pub fn detect_display_size() -> Result<(u32, u32)> {
    ensure_screen_access()?;
    let content = SCShareableContent::get()
        .context("Failed to get shareable content")?;
    let display = content
        .displays()
        .into_iter()
        .next()
        .context("No display found")?;
    Ok((display.width() as u32, display.height() as u32))
}

/// Query the main display's logical bounds from CoreGraphics.
/// This is the coordinate system CGEvent uses, and MUST be used for mouse mapping.
/// May differ from SCDisplay dimensions on non-standard scaling modes.
pub fn detect_cg_display_size() -> Result<(u32, u32)> {
    use core_graphics::display::CGDisplay;
    let main = CGDisplay::main();
    let bounds = main.bounds();
    let w = bounds.size.width as u32;
    let h = bounds.size.height as u32;
    if w == 0 || h == 0 {
        anyhow::bail!("CGDisplay returned zero bounds");
    }
    Ok((w, h))
}

/// The display to capture. A just-created virtual display reaches shareable content with a
/// delay, so a requested id is retried for ~2 s.
async fn find_display(id: Option<u32>) -> Result<SCDisplay> {
    for _ in 0..20 {
        // SCShareableContent::get() is synchronous, run in blocking task
        let content = tokio::task::spawn_blocking(SCShareableContent::get)
            .await?
            .context("Failed to get shareable content (Screen Recording permission needed)")?;
        let mut displays = content.displays().into_iter();
        let Some(id) = id else {
            return displays.next().context("No display found");
        };
        if let Some(d) = displays.find(|d| d.display_id() == id) {
            return Ok(d);
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    Err(DisplayMissing(id.unwrap_or_default()).into())
}

/// Shareable content is readable but the requested display is not in it (it went away).
#[derive(Debug)]
pub struct DisplayMissing(pub u32);

impl std::fmt::Display for DisplayMissing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "display {} not in shareable content", self.0)
    }
}

impl std::error::Error for DisplayMissing {}

impl ScreenCapturer {
    /// Create a new screen capturer for `config.display_id` (or the first display).
    ///
    /// If `audio_tx` is `Some`, audio capture is enabled and frames are sent
    /// to the provided channel as `AudioFrame` (interleaved Float32 PCM, 48 kHz stereo).
    pub async fn new(
        config: CaptureConfig,
        audio_tx: Option<mpsc::Sender<AudioFrame>>,
    ) -> Result<Self> {
        let display = find_display(config.display_id).await?;

        let actual_width = if config.width == 0 {
            display.width() as u32
        } else {
            config.width
        };
        let actual_height = if config.height == 0 {
            display.height() as u32
        } else {
            config.height
        };

        let filter = SCContentFilter::create()
            .with_display(&display)
            .with_excluding_windows(&[])
            .build();

        let captures_audio = audio_tx.is_some();
        let stream_config = stream_configuration(
            actual_width,
            actual_height,
            config.frame_rate,
            config.pixel_format,
            config.show_cursor,
            captures_audio,
        );

        // Channel for capture events: buffer 2 entries to allow for jitter
        let (frame_tx, frame_rx): (mpsc::Sender<CaptureEvent>, mpsc::Receiver<CaptureEvent>) =
            mpsc::channel(2);

        let video_handler = VideoOutputHandler {
            frame_tx,
            pixel_format: config.pixel_format,
        };

        // Sleep/wake, display reconfiguration and user switch stop the stream through the delegate.
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let on_stop = screencapturekit::stream::ErrorHandler::new(move |e| {
            tracing::warn!(error = ?e, "SCStream stopped");
            stop_tx.send_replace(true);
        });
        let mut stream = SCStream::new_with_delegate(&filter, &stream_config, on_stop);
        stream.add_output_handler(video_handler, SCStreamOutputType::Screen);

        // Register audio handler only if a sender was provided
        if let Some(tx) = audio_tx {
            let audio_handler = AudioOutputHandler {
                audio_tx: Arc::new(tx),
            };
            stream.add_output_handler(audio_handler, SCStreamOutputType::Audio);
        }

        stream.start_capture().context("Failed to start capture")?;

        tracing::info!(
            width = actual_width,
            height = actual_height,
            fps = config.frame_rate,
            pixel_format = ?config.pixel_format,
            audio = captures_audio,
            "Screen capture started"
        );

        Ok(Self {
            stream,
            frame_rx,
            stop_rx,
            width: actual_width,
            height: actual_height,
            pixel_format: config.pixel_format,
            show_cursor: config.show_cursor,
            captures_audio,
        })
    }

    /// Receive the next capture event (async, cancellation safe); None once the stream stopped.
    pub async fn next_frame(&mut self) -> Option<CaptureEvent> {
        recv_or_stop(&mut self.frame_rx, &mut self.stop_rx).await
    }

    /// Try to get a buffered capture event without waiting. Returns None if no event ready.
    pub fn try_next_frame(&mut self) -> Option<CaptureEvent> {
        self.frame_rx.try_recv().ok()
    }

    /// Whether this stream captures audio.
    pub fn captures_audio(&self) -> bool {
        self.captures_audio
    }

    /// Update capture frame rate at runtime, keeping size, pixel format, cursor and audio.
    /// Note: SCStream::update_configuration() blocks until completion via internal semaphore.
    pub fn set_frame_rate(&self, fps: u32) -> anyhow::Result<()> {
        let config = stream_configuration(
            self.width,
            self.height,
            fps,
            self.pixel_format,
            self.show_cursor,
            self.captures_audio,
        );
        self.stream
            .update_configuration(&config)
            .map_err(|e| anyhow::anyhow!("Failed to update SCStream frame rate: {:?}", e))?;
        tracing::info!(fps, "Capture frame rate updated");
        Ok(())
    }
}

impl Drop for ScreenCapturer {
    fn drop(&mut self) {
        // SCStream's own drop only releases it (screencapturekit 1.5.4) and frees the handler
        // context; stop first so a replaced stream neither keeps running nor calls into freed
        // memory. Blocks until ScreenCaptureKit confirms; an already stopped stream errors at once.
        if let Err(e) = self.stream.stop_capture() {
            tracing::debug!(error = ?e, "SCStream stop on drop");
        }
    }
}

/// Fallback capturer using CGDisplayCreateImage (CoreGraphics).
/// Works during lock screen because it captures at the display level,
/// below the window server / ScreenCaptureKit layer.
pub struct CgFallbackCapturer {
    display_id: u32,
    width: u32,
    height: u32,
    frame_interval: std::time::Duration,
}

impl CgFallbackCapturer {
    /// Create a fallback capturer for the main display
    pub fn new(config: &CaptureConfig) -> Self {
        let display_id = config.display_id.unwrap_or_else(|| core_graphics::display::CGDisplay::main().id);
        let fps = if config.frame_rate > 0 { config.frame_rate } else { 30 };
        Self {
            display_id,
            width: config.width,
            height: config.height,
            frame_interval: std::time::Duration::from_micros(1_000_000 / fps as u64),
        }
    }

    /// Capture a single frame using CGDisplayCreateImage
    pub fn capture_frame(&self) -> Option<CapturedFrame> {
        let display = core_graphics::display::CGDisplay::new(self.display_id);
        let image = display.image()?;

        let w = image.width() as u32;
        let h = image.height() as u32;
        let bpr = image.bytes_per_row();
        let data = image.data();
        let raw = data.bytes().to_vec();

        Some(CapturedFrame {
            width: if self.width > 0 { self.width } else { w },
            height: if self.height > 0 { self.height } else { h },
            data: FrameData::Raw(Bytes::from(raw)),
            stride: bpr,
            timestamp_us: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_micros() as u64,
            dirty_rects: vec![],
        })
    }

    /// Frame interval for pacing
    pub fn frame_interval(&self) -> std::time::Duration {
        self.frame_interval
    }
}

// ---------------------------------------------------------------------------
// CoreVideo FFI for CVPixelBuffer retain/release and plane access
// ---------------------------------------------------------------------------

#[link(name = "CoreVideo", kind = "framework")]
extern "C" {
    fn CVPixelBufferRetain(pixel_buffer: *mut c_void) -> *mut c_void;
    fn CVPixelBufferRelease(pixel_buffer: *mut c_void);
    fn CVPixelBufferLockBaseAddress(pixel_buffer: *mut c_void, flags: u64) -> i32;
    fn CVPixelBufferUnlockBaseAddress(pixel_buffer: *mut c_void, flags: u64) -> i32;
    fn CVPixelBufferGetBaseAddressOfPlane(pixel_buffer: *mut c_void, plane: usize) -> *mut u8;
    fn CVPixelBufferGetBytesPerRowOfPlane(pixel_buffer: *mut c_void, plane: usize) -> usize;
    fn CVPixelBufferGetHeightOfPlane(pixel_buffer: *mut c_void, plane: usize) -> usize;
    fn CVPixelBufferGetPixelFormatType(pixel_buffer: *mut c_void) -> u32;
    fn CVPixelBufferGetBaseAddress(pixel_buffer: *mut c_void) -> *mut u8;
    fn CVPixelBufferGetBytesPerRow(pixel_buffer: *mut c_void) -> usize;
    fn CVPixelBufferGetWidth(pixel_buffer: *mut c_void) -> usize;
    fn CVPixelBufferGetHeight(pixel_buffer: *mut c_void) -> usize;
}

/// kCVPixelFormatType_32BGRA ('BGRA')
const K_CV_PIXEL_FORMAT_32BGRA: u32 = u32::from_be_bytes(*b"BGRA");

/// kCVPixelBufferLock_ReadOnly
const CV_PIXEL_BUFFER_LOCK_READ_ONLY: u64 = 0x0000_0001;

// ---------------------------------------------------------------------------
// NV12PlaneData — extracted Y and UV plane data from an NV12 pixel buffer
// ---------------------------------------------------------------------------

/// Holds copied plane data from an NV12 CVPixelBuffer.
/// Used for the OpenH264 software encoding fallback path.
pub struct NV12PlaneData {
    /// Y (luma) plane data, one byte per pixel, row-major
    pub y_data: Vec<u8>,
    /// Y plane stride (bytes per row, may include padding)
    pub y_stride: usize,
    /// UV (chroma) plane data, interleaved U/V, half resolution
    pub uv_data: Vec<u8>,
    /// UV plane stride (bytes per row, may include padding)
    pub uv_stride: usize,
    /// Width of the Y plane in pixels
    pub width: usize,
    /// Height of the Y plane in pixels
    pub height: usize,
}

// ---------------------------------------------------------------------------
// SafePixelBuffer — RAII wrapper around a retained CVPixelBufferRef
// ---------------------------------------------------------------------------

/// A safe RAII wrapper around a `CVPixelBufferRef` that manages the
/// retain/release lifecycle. Intended for zero-copy frame passing to
/// VideoToolbox (hardware encoder) while also supporting a locked-read
/// path for OpenH264 (software encoder fallback).
///
/// # Safety
///
/// The inner pointer must originate from a valid `CVPixelBufferRef`.
/// `Send` is implemented because IOSurface-backed pixel buffers are safe
/// to transfer across threads. `Sync` is deliberately NOT implemented
/// because `CVPixelBufferLockBaseAddress` / `UnlockBaseAddress` are not
/// safe for concurrent access from multiple threads.
pub struct SafePixelBuffer {
    ptr: *mut c_void,
}

// SAFETY: IOSurface-backed CVPixelBuffers can be sent across threads.
// We do NOT implement Sync — lock/unlock is not thread-safe for
// concurrent access.
unsafe impl Send for SafePixelBuffer {}

impl SafePixelBuffer {
    /// Create a `SafePixelBuffer` by retaining the given `CVPixelBufferRef`.
    ///
    /// # Safety
    ///
    /// `ptr` must be a valid, non-null `CVPixelBufferRef`.
    pub unsafe fn from_raw(ptr: *mut c_void) -> Self {
        debug_assert!(!ptr.is_null(), "CVPixelBufferRef must not be null");
        CVPixelBufferRetain(ptr);
        Self { ptr }
    }

    /// Return the raw `CVPixelBufferRef` pointer (e.g. for passing to
    /// VideoToolbox's `VTCompressionSessionEncodeFrame`).
    pub fn as_ptr(&self) -> *mut c_void {
        self.ptr
    }

    /// Create a new `SafePixelBuffer` that shares the same underlying
    /// `CVPixelBuffer`, incrementing its retain count.
    pub fn clone_ref(&self) -> Self {
        unsafe {
            CVPixelBufferRetain(self.ptr);
        }
        Self { ptr: self.ptr }
    }

    /// Lock the pixel buffer, copy NV12 plane data out, and unlock.
    ///
    /// This is the software-encoding path: we lock the buffer read-only,
    /// memcpy the Y and UV planes into owned `Vec<u8>`s, then unlock.
    /// The lock is held for the shortest possible duration.
    ///
    /// Returns `None` if the lock fails or plane pointers are null.
    pub fn lock_and_read_nv12(&self) -> Option<NV12PlaneData> {
        unsafe {
            // Lock for read-only access
            let status = CVPixelBufferLockBaseAddress(self.ptr, CV_PIXEL_BUFFER_LOCK_READ_ONLY);
            if status != 0 {
                tracing::warn!(status, "CVPixelBufferLockBaseAddress failed");
                return None;
            }

            let result = self.read_nv12_planes();

            // Always unlock, even if plane read failed
            CVPixelBufferUnlockBaseAddress(self.ptr, CV_PIXEL_BUFFER_LOCK_READ_ONLY);

            result
        }
    }

    /// Lock a 32BGRA buffer read-only and pass `(pixels, bytes_per_row, width, height)` to `f`,
    /// so callers copy only the rows they need. None for other formats or a failed lock.
    pub fn with_bgra<R>(&self, f: impl FnOnce(&[u8], usize, usize, usize) -> R) -> Option<R> {
        unsafe {
            if CVPixelBufferGetPixelFormatType(self.ptr) != K_CV_PIXEL_FORMAT_32BGRA {
                return None;
            }
            if CVPixelBufferLockBaseAddress(self.ptr, CV_PIXEL_BUFFER_LOCK_READ_ONLY) != 0 {
                return None;
            }
            let base = CVPixelBufferGetBaseAddress(self.ptr);
            let stride = CVPixelBufferGetBytesPerRow(self.ptr);
            let (w, h) = (CVPixelBufferGetWidth(self.ptr), CVPixelBufferGetHeight(self.ptr));
            let result = (!base.is_null()).then(|| f(std::slice::from_raw_parts(base, stride * h), stride, w, h));
            CVPixelBufferUnlockBaseAddress(self.ptr, CV_PIXEL_BUFFER_LOCK_READ_ONLY);
            result
        }
    }

    /// Read Y and UV planes while the buffer is locked.
    /// Caller must ensure the buffer is locked before calling.
    unsafe fn read_nv12_planes(&self) -> Option<NV12PlaneData> {
        // Plane 0 = Y (luma)
        let y_ptr = CVPixelBufferGetBaseAddressOfPlane(self.ptr, 0);
        let y_stride = CVPixelBufferGetBytesPerRowOfPlane(self.ptr, 0);
        let y_height = CVPixelBufferGetHeightOfPlane(self.ptr, 0);

        // Plane 1 = UV (chroma, interleaved)
        let uv_ptr = CVPixelBufferGetBaseAddressOfPlane(self.ptr, 1);
        let uv_stride = CVPixelBufferGetBytesPerRowOfPlane(self.ptr, 1);
        let uv_height = CVPixelBufferGetHeightOfPlane(self.ptr, 1);

        if y_ptr.is_null() || uv_ptr.is_null() {
            tracing::warn!("NV12 plane base address is null");
            return None;
        }

        let y_len = y_stride * y_height;
        let uv_len = uv_stride * uv_height;

        let y_data = std::slice::from_raw_parts(y_ptr, y_len).to_vec();
        let uv_data = std::slice::from_raw_parts(uv_ptr, uv_len).to_vec();

        // Width is derived from plane 0 stride and pixel format.
        // For NV12 Y plane, each pixel is one byte, but stride may include
        // padding. We use the plane height directly and report stride so
        // callers can handle padding.
        Some(NV12PlaneData {
            y_data,
            y_stride,
            uv_data,
            uv_stride,
            width: y_stride, // conservative: callers should clamp to actual width
            height: y_height,
        })
    }
}

impl Drop for SafePixelBuffer {
    fn drop(&mut self) {
        // SAFETY: ptr was retained in `from_raw`, so we must release it.
        unsafe {
            CVPixelBufferRelease(self.ptr);
        }
    }
}

impl std::fmt::Debug for SafePixelBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SafePixelBuffer")
            .field("ptr", &self.ptr)
            .finish()
    }
}

/// Next capture event, or None once the stream reported a stop. The video handler keeps the
/// frame sender alive after SCK stops a stream, so the channel alone never ends.
async fn recv_or_stop(
    rx: &mut mpsc::Receiver<CaptureEvent>,
    stopped: &mut tokio::sync::watch::Receiver<bool>,
) -> Option<CaptureEvent> {
    tokio::select! {
        biased;
        // wait_for sees a stop raised earlier too; a dropped sender means no stop can come.
        raised = async { stopped.wait_for(|s| *s).await.is_ok() } => {
            if raised { None } else { rx.recv().await }
        }
        ev = rx.recv() => ev,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stop_signal_ends_next_frame_while_sender_alive() {
        let (tx, mut rx) = mpsc::channel(2);
        let (stop_tx, mut stop_rx) = tokio::sync::watch::channel(false);
        tx.try_send(CaptureEvent::Idle).unwrap();
        assert!(matches!(recv_or_stop(&mut rx, &mut stop_rx).await, Some(CaptureEvent::Idle)));

        let waiter = tokio::spawn(async move {
            let ev = recv_or_stop(&mut rx, &mut stop_rx).await;
            (ev.is_none(), rx, stop_rx)
        });
        tokio::task::yield_now().await;
        stop_tx.send_replace(true);
        let (ended, mut rx, mut stop_rx) = waiter.await.unwrap();
        assert!(ended);
        // Sticky: later calls end too, even with frames queued and the sender alive.
        tx.try_send(CaptureEvent::Idle).unwrap();
        assert!(recv_or_stop(&mut rx, &mut stop_rx).await.is_none());
    }

    #[tokio::test]
    async fn stop_raised_before_waiting_is_not_lost() {
        let (_tx, mut rx) = mpsc::channel::<CaptureEvent>(2);
        let (stop_tx, mut stop_rx) = tokio::sync::watch::channel(false);
        stop_tx.send_replace(true);
        let ev = tokio::time::timeout(std::time::Duration::from_secs(1), recv_or_stop(&mut rx, &mut stop_rx)).await;
        assert!(matches!(ev, Ok(None)));
    }

    #[link(name = "CoreVideo", kind = "framework")]
    extern "C" {
        fn CVPixelBufferCreate(
            allocator: *const c_void, width: usize, height: usize, format: u32,
            attributes: *const c_void, out: *mut *mut c_void,
        ) -> i32;
    }

    /// An in-memory pixel buffer (no capture involved), released when the wrapper drops.
    fn pixel_buffer(w: usize, h: usize, format: u32) -> SafePixelBuffer {
        let mut raw = std::ptr::null_mut();
        assert_eq!(unsafe { CVPixelBufferCreate(std::ptr::null(), w, h, format, std::ptr::null(), &mut raw) }, 0);
        let buf = unsafe { SafePixelBuffer::from_raw(raw) };
        unsafe { CVPixelBufferRelease(raw) }; // from_raw retained; drop the create reference
        buf
    }

    #[test]
    fn with_bgra_reads_locked_rows() {
        let buf = pixel_buffer(5, 3, K_CV_PIXEL_FORMAT_32BGRA);
        let got = buf.with_bgra(|px, stride, w, h| (px.len(), stride, w, h)).expect("BGRA buffer");
        let (len, stride, w, h) = got;
        assert_eq!((w, h), (5, 3));
        assert!(stride >= 5 * 4);
        assert_eq!(len, stride * 3);
    }

    #[test]
    fn with_bgra_rejects_nv12() {
        let buf = pixel_buffer(16, 16, u32::from_be_bytes(*b"420f"));
        assert!(buf.with_bgra(|_, _, _, _| ()).is_none());
    }
}
