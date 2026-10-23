//! Video encoding abstraction (H.264 via VideoToolbox / OpenH264)

mod openh264_enc;
pub mod yuv444_split;
#[cfg(target_os = "macos")]
pub mod color_convert;
#[cfg(target_os = "macos")]
mod videotoolbox;
#[cfg(target_os = "macos")]

use anyhow::Result;
use bytes::Bytes;

pub use openh264_enc::OpenH264Encoder;
#[cfg(target_os = "macos")]
pub use videotoolbox::VtEncoder;

/// Encoded frame output
pub struct EncodedFrame {
    /// H.264 NAL units (Annex B format)
    pub data: Bytes,
    /// Whether this is a key frame (IDR)
    pub is_keyframe: bool,
    pub width: u32,
    pub height: u32,
}

/// AVC444 dual-stream encoded result
pub struct Avc444EncodedFrame {
    /// Stream1: Main YUV420 H.264 (luma + downsampled chroma)
    pub main_view: EncodedFrame,
    /// Stream2: Auxiliary YUV420 H.264 (chroma compensation)
    pub aux_view: EncodedFrame,
}

/// Quality preset
#[derive(Debug, Clone, Copy)]
pub enum Quality {
    LowLatency,
    Balanced,
    HighQuality,
}

/// Video encoder trait
pub trait VideoEncoder: Send {
    /// AVC420 encode (existing)
    fn encode_bgra(&mut self, data: &[u8], width: u32, height: u32, stride: usize) -> Result<EncodedFrame>;

    /// AVC444 dual-stream encode.
    /// Internally performs BGRA -> YUV444 -> B-area split -> dual session encode.
    fn encode_bgra_444(&mut self, data: &[u8], width: u32, height: u32, stride: usize) -> Result<Avc444EncodedFrame>;

    /// Zero-copy encode from CVPixelBuffer (NV12). Default returns error (unsupported).
    /// Only VtEncoder overrides this.
    fn encode_pixel_buffer(&mut self, _ptr: *mut std::ffi::c_void, _force_keyframe: bool) -> Result<EncodedFrame> {
        anyhow::bail!("encode_pixel_buffer not supported by this encoder")
    }

    fn set_bitrate(&mut self, bitrate_bps: u32);
    fn force_keyframe(&mut self);

    /// Whether this encoder supports AVC444 dual-stream encoding
    fn supports_444(&self) -> bool;

    /// Whether `encode_pixel_buffer` works (capture can then deliver NV12 zero-copy).
    fn accepts_pixel_buffer(&self) -> bool { false }
}

/// Align a dimension up to the nearest multiple of 16 (H.264 macroblock size)
pub fn align16(v: u32) -> u32 {
    (v + 15) & !15
}

/// Bitrate for screen content: pixels x fps x bits-per-pixel, floored at 1 Mbps
/// (1080p60 HighQuality is about 14.9 Mbps).
pub fn screen_bitrate(width: u32, height: u32, fps: f32, quality: Quality) -> u32 {
    let bpp = match quality {
        Quality::LowLatency => 0.06,
        Quality::Balanced => 0.09,
        Quality::HighQuality => 0.12,
    };
    let bits = width as f64 * height as f64 * fps as f64 * bpp;
    (bits.round() as u32).max(1_000_000)
}

/// Encoder preference
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EncoderPreference {
    /// OpenH264 CPU encoder — full P-frame support, higher latency (~40ms)
    Software,
    /// VideoToolbox GPU encoder — low latency (~6ms), supports P-frames (default;
    /// falls back to OpenH264 when VideoToolbox is unavailable)
    Hardware,
}

impl EncoderPreference {
    pub fn from_str_opt(s: Option<&str>) -> Self {
        match s.map(|s| s.to_lowercase()).as_deref() {
            Some("software") | Some("cpu") | Some("openh264") | Some("oh264") => Self::Software,
            // hardware / gpu / videotoolbox / vt / auto / unset
            _ => Self::Hardware,
        }
    }
}

/// Create an H.264 encoder based on preference.
/// When `mode_444` is true, the encoder will initialize dual sessions for AVC444 support.
pub fn create_encoder(width: u32, height: u32, fps: f32, quality: Quality, preference: EncoderPreference, mode_444: bool, bitrate: u32) -> Result<Box<dyn VideoEncoder>> {
    let enc_w = align16(width);
    let enc_h = align16(height);

    // Hardware: VideoToolbox GPU encoder
    #[cfg(target_os = "macos")]
    if preference == EncoderPreference::Hardware {
        match VtEncoder::new(enc_w, enc_h, fps, bitrate, mode_444) {
            Ok(encoder) => {
                tracing::info!(enc_w, enc_h, mode_444, "Using VideoToolbox hardware encoder (GPU)");
                return Ok(Box::new(encoder));
            }
            Err(e) => {
                tracing::warn!("VideoToolbox unavailable: {e}, falling back to OpenH264");
            }
        }
    }

    // Software (or VideoToolbox failed): OpenH264 CPU encoder (full P-frame support)
    tracing::info!(enc_w, enc_h, mode_444, "Using OpenH264 software encoder (CPU)");
    let encoder = OpenH264Encoder::new(enc_w, enc_h, fps, bitrate, mode_444)?;
    Ok(Box::new(encoder))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: u32, b: u32) -> bool {
        a.abs_diff(b) <= 2
    }

    #[test]
    fn bitrate_1080p60_high_quality_is_about_15_mbps() {
        let b = screen_bitrate(1920, 1080, 60.0, Quality::HighQuality);
        assert!((10_000_000..=15_000_000).contains(&b), "{b}");
    }

    #[test]
    fn bitrate_scales_with_fps_and_pixels() {
        let base = screen_bitrate(1920, 1080, 60.0, Quality::HighQuality);
        assert!(close(screen_bitrate(1920, 1080, 120.0, Quality::HighQuality), base * 2));
        assert!(close(screen_bitrate(3840, 2160, 60.0, Quality::HighQuality), base * 4));
    }

    #[test]
    fn bitrate_orders_by_quality() {
        let b = |q| screen_bitrate(1920, 1080, 60.0, q);
        assert!(b(Quality::LowLatency) < b(Quality::Balanced));
        assert!(b(Quality::Balanced) < b(Quality::HighQuality));
    }

    #[test]
    fn bitrate_has_floor() {
        assert!(screen_bitrate(320, 240, 1.0, Quality::LowLatency) >= 1_000_000);
    }

    #[test]
    fn default_preference_is_hardware() {
        assert_eq!(EncoderPreference::from_str_opt(None), EncoderPreference::Hardware);
        assert_eq!(EncoderPreference::from_str_opt(Some("auto")), EncoderPreference::Hardware);
        assert_eq!(EncoderPreference::from_str_opt(Some("software")), EncoderPreference::Software);
    }
}
