//! RDPGFX (Graphics Pipeline) DVC handler

use core::time::Duration;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use ironrdp_core::{encode_vec, impl_as_any, Encode, WriteCursor, EncodeResult};
use ironrdp_dvc::{DvcEncode, DvcProcessor, DvcServerProcessor};
use ironrdp_pdu::geometry::InclusiveRectangle;
use ironrdp_pdu::rdp::vc::dvc::gfx::{
    Avc420BitmapStream, Avc444BitmapStream, CapabilitiesConfirmPdu, CapabilitiesV10Flags,
    CapabilitiesV103Flags, CapabilitiesV104Flags, CapabilitiesV107Flags, CapabilitiesV81Flags,
    CapabilitySet, ClientPdu, Codec1Type, CreateSurfacePdu, EndFramePdu, Encoding,
    MapSurfaceToOutputPdu, PixelFormat as GfxPixelFormat, QuantQuality, QueueDepth, ResetGraphicsPdu,
    ServerPdu, StartFramePdu, Timestamp, WireToSurface1Pdu, Codec2Type, WireToSurface2Pdu,
};
use ironrdp_pdu::gcc::{Monitor, MonitorFlags};
use ironrdp_pdu::{decode, PduResult};

type DvcMessage = ironrdp_dvc::DvcMessage;
use tracing::{debug, info, warn};

use crate::display::{GfxFrameUpdate, GfxRemoteFxUpdate, GfxUncompressedUpdate};
pub use crate::encoder::rfx::{encode_gfx_progressive, RfxQuality};

/// GFX channel name as defined by RDP spec
pub const GFX_CHANNEL_NAME: &str = "Microsoft::Windows::RDS::Graphics";

/// Wrapper to make raw PDU bytes usable as DvcMessage
pub struct RawGfxPdu(pub Vec<u8>);

impl Encode for RawGfxPdu {
    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        dst.write_slice(&self.0);
        Ok(())
    }

    fn name(&self) -> &'static str {
        "GfxServerPdu"
    }

    fn size(&self) -> usize {
        self.0.len()
    }
}

impl DvcEncode for RawGfxPdu {}

// SAFETY: RawGfxPdu only contains Vec<u8> which is Send
unsafe impl Send for RawGfxPdu {}

/// Wrap raw GFX PDU bytes in RDP_SEGMENTED_DATA (ZGFX) format.
/// MS-RDPEGFX Section 2.2.5: ALL GFX PDUs must be ZGFX-wrapped before DVC transport.
/// Using uncompressed mode (descriptor 0xE0/0xE1, compression type 0x04).
fn wrap_zgfx(data: &[u8]) -> Vec<u8> {
    const SINGLE: u8 = 0xE0;
    const MULTIPART: u8 = 0xE1;
    const UNCOMPRESSED: u8 = 0x04;
    // Max data per segment = 65534 bytes. The segmentSize field includes the
    // 1-byte compression type, so segmentSize = data_len + 1 ≤ 65535 (0xFFFF).
    const MAX_SEG_DATA: usize = 65534;

    if data.len() <= MAX_SEG_DATA {
        // Single segment: descriptor(1) + compression_type(1) + data
        let mut out = Vec::with_capacity(2 + data.len());
        out.push(SINGLE);
        out.push(UNCOMPRESSED);
        out.extend_from_slice(data);
        out
    } else {
        // Multipart: descriptor(1) + seg_count(2) + uncompressed_size(4) + segments
        let seg_count = data.len().div_ceil(MAX_SEG_DATA);
        let mut out = Vec::with_capacity(7 + data.len() + seg_count * 5);
        out.push(MULTIPART);
        out.extend_from_slice(&(seg_count as u16).to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        for chunk in data.chunks(MAX_SEG_DATA) {
            // segmentSize = compression_type(1) + chunk data
            let seg_size = (chunk.len() + 1) as u32;
            out.extend_from_slice(&seg_size.to_le_bytes());
            out.push(UNCOMPRESSED);
            out.extend_from_slice(chunk);
        }
        out
    }
}

/// Capability versions `ironrdp-pdu` 0.7 can decode (its `CapabilityVersion`).
const KNOWN_CAPS_VERSIONS: [u32; 12] = [
    0x8_0004, 0x8_0105, 0xa_0002, 0xa_0100, 0xa_0200, 0xa_0301, 0xa_0400, 0xa_0502, 0xa_0600, 0xa_0601,
    0xa_0701, 0xa_0702,
];

/// Decodes a client GFX PDU. ironrdp-pdu rejects a whole CapsAdvertise when one set has a version
/// it does not know; MS-RDPEGFX says the server ignores such sets, so retry with them removed.
fn decode_client_pdu(data: &[u8]) -> ironrdp_core::DecodeResult<ClientPdu> {
    let err = match decode::<ClientPdu>(data) {
        Ok(pdu) => return Ok(pdu),
        Err(e) => e,
    };
    const CAPS_ADVERTISE: u16 = 0x0012;
    if data.len() < 10 || u16::from_le_bytes([data[0], data[1]]) != CAPS_ADVERTISE {
        return Err(err);
    }
    let count = u16::from_le_bytes([data[8], data[9]]);
    let (mut kept, mut sets, mut offset) = (0u16, Vec::new(), 10usize);
    for _ in 0..count {
        let Some(head) = data.get(offset..offset + 8) else { return Err(err) };
        let version = u32::from_le_bytes([head[0], head[1], head[2], head[3]]);
        let len = u32::from_le_bytes([head[4], head[5], head[6], head[7]]) as usize;
        let Some(set) = data.get(offset..offset + 8 + len) else { return Err(err) };
        if KNOWN_CAPS_VERSIONS.contains(&version) {
            sets.extend_from_slice(set);
            kept += 1;
        } else {
            info!(version = format_args!("{version:#x}"), data = ?&set[8..], "GFX: skipping unknown capability set");
        }
        offset += 8 + len;
    }
    let mut pdu = Vec::with_capacity(10 + sets.len());
    pdu.extend_from_slice(&data[0..4]);
    pdu.extend_from_slice(&((10 + sets.len()) as u32).to_le_bytes());
    pdu.extend_from_slice(&kept.to_le_bytes());
    pdu.extend_from_slice(&sets);
    decode(&pdu).map_err(|_| err)
}

/// Unwrap ZGFX segmented data from client. Returns None if not ZGFX-wrapped.
fn unwrap_zgfx(data: &[u8]) -> Option<Vec<u8>> {
    if data.is_empty() {
        return None;
    }
    match data[0] {
        0xE0 => {
            // Single segment: skip descriptor(1) + compression_type(1)
            if data.len() > 2 && data[1] == 0x04 {
                Some(data[2..].to_vec())
            } else {
                None
            }
        }
        0xE1 => {
            // Multipart: descriptor(1) + segment_count(2) + uncompressed_size(4)
            if data.len() < 7 {
                return None;
            }
            let seg_count = u16::from_le_bytes([data[1], data[2]]) as usize;
            let mut offset = 7;
            let mut result = Vec::new();
            for _ in 0..seg_count {
                if offset + 4 > data.len() {
                    break;
                }
                let seg_size = u32::from_le_bytes([
                    data[offset], data[offset + 1], data[offset + 2], data[offset + 3],
                ]) as usize;
                offset += 4;
                if offset < data.len() && data[offset] == 0x04 {
                    // Uncompressed: skip compression_type byte
                    offset += 1;
                    let end = (offset + seg_size - 1).min(data.len());
                    result.extend_from_slice(&data[offset..end]);
                    offset = end;
                } else {
                    break;
                }
            }
            Some(result)
        }
        _ => None, // Not ZGFX-wrapped
    }
}

/// Outcome of a CapabilitiesAdvertise: the set to confirm and the AVC support it implies.
#[derive(Debug)]
pub struct NegotiatedCaps {
    pub confirmed: CapabilitySet,
    pub avc420: bool,
    pub avc444: bool,
}

/// Version order of the sets ironrdp-pdu decodes (Unknown never wins).
fn cap_rank(cap: &CapabilitySet) -> u8 {
    match cap {
        CapabilitySet::V8 { .. } => 1,
        CapabilitySet::V8_1 { .. } => 2,
        CapabilitySet::V10 { .. } => 3,
        CapabilitySet::V10_1 => 4,
        CapabilitySet::V10_2 { .. } => 5,
        CapabilitySet::V10_3 { .. } => 6,
        CapabilitySet::V10_4 { .. } => 7,
        CapabilitySet::V10_5 { .. } => 8,
        CapabilitySet::V10_6 { .. } => 9,
        CapabilitySet::V10_6Err { .. } => 10,
        CapabilitySet::V10_7 { .. } => 11,
        CapabilitySet::Unknown(_) => 0,
    }
}

/// Picks the capability set to confirm. AVC sets win; without AVC, the highest version advertised.
pub fn negotiate_caps(cap_sets: &[CapabilitySet]) -> NegotiatedCaps {
    let (mut avc420, mut avc444) = (false, false);
    let mut best_cap = None;

    for cap in cap_sets {
        match cap {
            CapabilitySet::V10_7 { flags } if !flags.contains(CapabilitiesV107Flags::AVC_DISABLED) => {
                avc420 = true;
                avc444 = true;
                best_cap = Some(cap.clone());
                break;
            }
            CapabilitySet::V10_6 { flags } | CapabilitySet::V10_6Err { flags }
                if !flags.contains(CapabilitiesV104Flags::AVC_DISABLED) =>
            {
                avc420 = true;
                avc444 = true;
                best_cap = Some(cap.clone());
            }
            CapabilitySet::V10_5 { flags } | CapabilitySet::V10_4 { flags }
                if !flags.contains(CapabilitiesV104Flags::AVC_DISABLED) =>
            {
                avc420 = true;
                avc444 = true;
                if best_cap.is_none() { best_cap = Some(cap.clone()); }
            }
            CapabilitySet::V10_3 { flags }
                if !flags.contains(CapabilitiesV103Flags::AVC_DISABLED) =>
            {
                avc420 = true;
                avc444 = true;
                if best_cap.is_none() { best_cap = Some(cap.clone()); }
            }
            CapabilitySet::V10_2 { flags } | CapabilitySet::V10 { flags }
                if !flags.contains(CapabilitiesV10Flags::AVC_DISABLED) =>
            {
                avc420 = true;
                avc444 = true;
                if best_cap.is_none() { best_cap = Some(cap.clone()); }
            }
            CapabilitySet::V10_1 => {
                // V10_1 has no AVC_DISABLED flag — always supports AVC
                avc420 = true;
                avc444 = true;
                if best_cap.is_none() { best_cap = Some(cap.clone()); }
            }
            CapabilitySet::V8_1 { flags }
                if flags.contains(CapabilitiesV81Flags::AVC420_ENABLED) =>
            {
                avc420 = true;
                // V8.1 only supports AVC420, not AVC444
                if best_cap.is_none() { best_cap = Some(cap.clone()); }
            }
            _ => {}
        }
    }

    let confirmed = best_cap
        .or_else(|| cap_sets.iter().filter(|c| cap_rank(c) > 0).max_by_key(|c| cap_rank(c)).cloned())
        .unwrap_or(CapabilitySet::V8 {
            flags: ironrdp_pdu::rdp::vc::dvc::gfx::CapabilitiesV8Flags::empty(),
        });
    NegotiatedCaps { confirmed, avc420, avc444 }
}

fn make_dvc_message(pdu: &ServerPdu) -> PduResult<DvcMessage> {
    let data = encode_vec(pdu).map_err(|e| ironrdp_pdu::custom_err!("GfxEncode", e))?;
    let wrapped = wrap_zgfx(&data);
    Ok(Box::new(RawGfxPdu(wrapped)))
}

/// Frames that may be in flight unacknowledged, beyond the path's baseline in-flight
/// (see `GfxState::baseline_in_flight`), before the encoder waits.
pub const MAX_PENDING_ACKS: u32 = 2;

/// Upper bound on the RTT-derived part of the gate (500 ms at 60 fps).
const MAX_BASELINE_IN_FLIGHT: u32 = 30;

/// With frames pending and no send or ack for this long, the gate reopens
/// (and the next frame is an IDR) so a client that stops acking never freezes video.
pub const ACK_STALL: Duration = Duration::from_secs(1);

/// Shared state between GfxHandler and the server
#[derive(Debug)]
pub struct GfxState {
    pub channel_id: Option<u32>,
    pub surface_created: bool,
    pub caps_confirmed: bool,
    pub frame_id: u32,
    pub avc420_supported: bool,
    /// Whether the client supports AVC444 (V10+ with AVC not disabled)
    pub avc444_supported: bool,
    /// Whether AVC444 is enabled by server config (chroma_mode = "avc444")
    pub avc444_enabled: bool,
    pub confirmed_cap: Option<CapabilitySet>,
    pub width: u16,
    pub height: u16,
    /// Frames sent but not yet acknowledged
    pub pending_acks: u32,
    /// Frames the encode pipeline handed over that `next_frame_id` has not counted yet: the
    /// channel slot frees on receive, before the session counts the frame.
    in_transit: u32,
    /// Last acknowledged frame ID
    pub last_ack_frame: u32,
    /// Network quality estimate (0.0 = congested, 1.0 = excellent)
    pub network_quality: f32,
    /// Send timestamps for RTT calculation (frame_id → Instant)
    pub frame_send_times: std::collections::HashMap<u32, Instant>,
    /// Exponentially weighted moving average RTT in ms
    pub rtt_ewma_ms: f64,
    /// Lowest RTT seen this session in ms (0 = none yet): the uncongested path delay.
    rtt_min_ms: f64,
    /// Target frame rate of the encode pipeline (0 = unknown), sizes the ack gate.
    pub frame_rate: u32,
    /// Last frame encode time in ms
    pub last_encode_ms: f64,
    /// Last frame size in bytes
    pub last_frame_bytes: u32,
    /// Total bytes sent
    pub total_bytes_sent: u64,
    /// Time of first frame sent
    pub start_time: Option<Instant>,
    /// Client peer IP address (set after TCP connection is established)
    pub peer_addr: Option<std::net::IpAddr>,
    /// Client GCC clientName of the active session (untrusted, client-chosen).
    pub client_name: Option<String>,
    /// Instantaneous bitrate tracking (per-frame Mbps samples since last log)
    bitrate_samples: Vec<f64>,
    bitrate_max: f64,
    bitrate_min: f64,
    last_frame_time: Option<Instant>,
    /// Hot-updatable: target bitrate override (set via ConfigUpdate)
    pub target_bitrate: Option<u32>,
    /// Hot-updatable: show cursor flag (set via ConfigUpdate), taken by the next session
    pub show_cursor: Option<bool>,
    /// Hot-updatable: frame rate (set via ConfigUpdate), taken by the next session
    pub next_frame_rate: Option<u32>,
    /// Hot-updatable: resolution (set via ConfigUpdate)
    pub resolution: Option<String>,
    /// Hot-updatable: encoder preference (set via ConfigUpdate)
    pub encoder_pref: Option<String>,
    /// Hot-updatable: chroma mode (set via ConfigUpdate)
    pub chroma_mode: Option<String>,
    /// Hot-updatable: RemoteFX quality, read for every RemoteFX frame (in auto mode, the
    /// level the encode pipeline's controller chose last).
    pub rfx_quality: RfxQuality,
    /// `rfx_quality` follows the connection (config "auto", the default).
    pub rfx_auto: bool,
    /// Client sent QueueDepth::Suspend: it does not ack, so do not gate on acks.
    acks_suspended: bool,
    /// Next encoded frame must be an IDR (caps confirm, refresh request, stall recovery).
    idr_requested: bool,
    last_ack_time: Option<Instant>,
    /// Signalled on every frame ack so the encode pipeline can re-check `can_send`.
    pub ack_notify: Arc<tokio::sync::Notify>,
    /// Signalled when capabilities are confirmed, so an idle pipeline picks its GFX path.
    pub caps_notify: Arc<tokio::sync::Notify>,
    /// RemoteFX Progressive codec context of the current surface; None until its first
    /// message (which carries SYNC and CONTEXT), cleared when a surface is created.
    progressive_context: Option<u32>,
    next_codec_context: u32,
}

impl GfxState {
    pub fn new(width: u16, height: u16, avc444_enabled: bool) -> Self {
        Self {
            channel_id: None,
            surface_created: false,
            caps_confirmed: false,
            frame_id: 0,
            avc420_supported: false,
            avc444_supported: false,
            avc444_enabled,
            confirmed_cap: None,
            width,
            height,
            pending_acks: 0,
            in_transit: 0,
            last_ack_frame: 0,
            network_quality: 1.0,
            frame_send_times: std::collections::HashMap::new(),
            rtt_ewma_ms: 0.0,
            rtt_min_ms: 0.0,
            frame_rate: 0,
            last_encode_ms: 0.0,
            last_frame_bytes: 0,
            total_bytes_sent: 0,
            start_time: None,
            peer_addr: None,
            client_name: None,
            bitrate_samples: Vec::new(),
            bitrate_max: 0.0,
            bitrate_min: f64::MAX,
            last_frame_time: None,
            target_bitrate: None,
            show_cursor: None,
            next_frame_rate: None,
            resolution: None,
            encoder_pref: None,
            chroma_mode: None,
            rfx_quality: RfxQuality::default(),
            rfx_auto: true,
            acks_suspended: false,
            idr_requested: false,
            last_ack_time: None,
            ack_notify: Arc::new(tokio::sync::Notify::new()),
            caps_notify: Arc::new(tokio::sync::Notify::new()),
            progressive_context: None,
            next_codec_context: 0,
        }
    }

    pub fn next_frame_id(&mut self) -> u32 {
        self.frame_id += 1;
        self.in_transit = self.in_transit.saturating_sub(1);
        // A suspended client never acks these frames; counting them would close the gate on resume.
        if !self.acks_suspended {
            self.pending_acks += 1;
        }
        self.total_bytes_sent += self.last_frame_bytes as u64;
        if self.start_time.is_none() {
            self.start_time = Some(Instant::now());
        }
        // Track instantaneous bitrate per frame
        if let Some(prev) = self.last_frame_time {
            let dt = prev.elapsed().as_secs_f64().max(0.0001);
            let mbps = self.last_frame_bytes as f64 * 8.0 / dt / 1_000_000.0;
            self.bitrate_samples.push(mbps);
            if mbps > self.bitrate_max { self.bitrate_max = mbps; }
            if mbps < self.bitrate_min { self.bitrate_min = mbps; }
        }
        self.last_frame_time = Some(Instant::now());
        self.frame_send_times.insert(self.frame_id, Instant::now());
        // Limit map size to prevent unbounded growth
        if self.frame_send_times.len() > 120 {
            let cutoff = self.frame_id.saturating_sub(120);
            self.frame_send_times.retain(|&id, _| id > cutoff);
        }
        self.frame_id
    }

    /// Update network quality and RTT based on frame acknowledgment
    pub fn ack_frame(&mut self, ack_frame_id: u32, queue_depth: QueueDepth, now: Instant) {
        self.last_ack_frame = ack_frame_id;
        self.pending_acks = self.pending_acks.saturating_sub(1);
        self.acks_suspended = queue_depth == QueueDepth::Suspend;
        if self.acks_suspended {
            self.pending_acks = 0;
        }
        self.last_ack_time = Some(now);
        // Notify stores a permit when nobody waits, so the wakeup is never lost.
        self.ack_notify.notify_one();

        // Calculate RTT from send timestamp
        if let Some(send_time) = self.frame_send_times.remove(&ack_frame_id) {
            let rtt_ms = now.saturating_duration_since(send_time).as_secs_f64() * 1000.0;
            if self.rtt_min_ms == 0.0 || rtt_ms < self.rtt_min_ms {
                self.rtt_min_ms = rtt_ms;
            }
            // EWMA with alpha=0.2 for smooth averaging
            if self.rtt_ewma_ms == 0.0 {
                self.rtt_ewma_ms = rtt_ms;
            } else {
                self.rtt_ewma_ms = self.rtt_ewma_ms * 0.8 + rtt_ms * 0.2;
            }
        }

        // Network quality based on RTT and ack backlog
        self.network_quality = if self.rtt_ewma_ms < 15.0 && self.pending_acks <= 2 {
            1.0 // Excellent: max bitrate
        } else if self.rtt_ewma_ms < 30.0 && self.pending_acks <= 5 {
            0.8 // Good
        } else if self.rtt_ewma_ms < 60.0 && self.pending_acks <= 10 {
            0.5 // Fair
        } else if self.rtt_ewma_ms < 100.0 {
            0.3 // Poor
        } else {
            0.15 // Congested
        };
    }

    /// Config `rfx_quality`: "low", "medium" or "high" fix the level; anything else (unset,
    /// "auto") follows the connection, starting at medium.
    pub fn set_rfx_quality(&mut self, value: Option<&str>) {
        self.rfx_auto = !matches!(value, Some("low" | "medium" | "high"));
        self.rfx_quality = RfxQuality::from_config(value);
    }

    /// Lowest RTT seen this session in ms (0 = none yet).
    pub fn rtt_min_ms(&self) -> f64 {
        self.rtt_min_ms
    }

    /// Frames in flight on an uncongested path: minimum RTT times the frame rate (bounded).
    pub fn baseline_in_flight(&self) -> u32 {
        // Truncation keeps LAN paths (under one frame interval) at the plain MAX_PENDING_ACKS gate.
        let frames = self.rtt_min_ms * f64::from(self.frame_rate) / 1000.0;
        (frames as u32).min(MAX_BASELINE_IN_FLIGHT)
    }

    /// Unacked frames beyond the path's baseline in-flight: queueing, i.e. congestion.
    pub fn backlog(&self) -> u32 {
        (self.pending_acks + self.in_transit).saturating_sub(self.baseline_in_flight())
    }

    /// The encode pipeline handed a GFX frame to the session (counted by `next_frame_id`).
    pub fn frame_handed_off(&mut self) {
        self.in_transit += 1;
    }

    /// Get recommended bitrate based on network quality and base bitrate
    pub fn adaptive_bitrate(&self, base_bitrate: u32) -> u32 {
        (base_bitrate as f32 * self.network_quality) as u32
    }

    /// Ack gate: may another frame be encoded and sent at `now`?
    /// Reopens after ACK_STALL without progress (last send or ack) and requests an IDR.
    pub fn can_send(&mut self, now: Instant) -> bool {
        if self.acks_suspended || self.backlog() <= MAX_PENDING_ACKS {
            return true;
        }
        let progress = match (self.last_frame_time, self.last_ack_time) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        let stalled = progress.is_none_or(|p| now.saturating_duration_since(p) > ACK_STALL);
        if !stalled {
            return false;
        }
        warn!(pending = self.pending_acks, "GFX acks stalled; reopening gate with IDR");
        self.pending_acks = 0;
        // Also clears a frame the session dropped uncounted (GFX closed in between).
        self.in_transit = 0;
        self.frame_send_times.clear();
        self.idr_requested = true;
        true
    }

    pub fn request_idr(&mut self) {
        self.idr_requested = true;
    }

    /// Returns true once per IDR request (requests coalesce).
    pub fn take_idr_request(&mut self) -> bool {
        core::mem::take(&mut self.idr_requested)
    }

    /// GFX channel open and capabilities confirmed: surface commands may be sent.
    pub fn is_ready(&self) -> bool {
        self.channel_id.is_some() && self.caps_confirmed
    }

    /// GFX ready and the client decodes AVC420 (H.264) surface commands.
    pub fn avc_ready(&self) -> bool {
        self.is_ready() && self.avc420_supported
    }
}

/// GFX DVC processor
pub struct GfxHandler {
    pub state: Arc<Mutex<GfxState>>,
}

impl_as_any!(GfxHandler);

impl GfxHandler {
    pub fn new(state: Arc<Mutex<GfxState>>) -> Self {
        Self { state }
    }

    /// Create a single ZGFX-wrapped buffer containing all GFX PDUs for an H.264 frame.
    /// Per MS-RDPEGFX Section 2.2.5: "The server SHOULD combine multiple RDPGFX commands
    /// into a single RDP_SEGMENTED_DATA structure."
    /// First call also includes surface setup PDUs (ResetGraphics + CreateSurface + MapSurfaceToOutput).
    pub fn create_frame_pdu(state: &mut GfxState, frame: &GfxFrameUpdate) -> Vec<u8> {
        // Concatenate all raw GFX PDUs, then ZGFX-wrap once
        let mut raw_pdus = Vec::new();

        let enc_w = frame.enc_width;
        let enc_h = frame.enc_height;

        // Detect resolution change (e.g. client resize or HiDPI mismatch) — rebuild surface
        if state.surface_created
            && (state.width != frame.width || state.height != frame.height)
        {
            info!(
                old_w = state.width, old_h = state.height,
                new_w = frame.width, new_h = frame.height,
                "GFX resolution changed, rebuilding surface"
            );
            state.width = frame.width;
            state.height = frame.height;
            state.surface_created = false;
        }

        // First frame: surface setup (CapConfirm already sent by DVC handler)
        if !state.surface_created {
            if let Ok(data) = encode_vec(&ServerPdu::ResetGraphics(ResetGraphicsPdu {
                width: state.width as u32,
                height: state.height as u32,
                monitors: vec![Monitor {
                    left: 0,
                    top: 0,
                    right: state.width as i32 - 1,
                    bottom: state.height as i32 - 1,
                    flags: MonitorFlags::PRIMARY,
                }],
            })) {
                raw_pdus.extend_from_slice(&data);
            }

            if let Ok(data) = encode_vec(&ServerPdu::CreateSurface(CreateSurfacePdu {
                surface_id: 0,
                width: state.width,
                height: state.height,
                pixel_format: GfxPixelFormat::XRgb,
            })) {
                raw_pdus.extend_from_slice(&data);
            }

            if let Ok(data) = encode_vec(&ServerPdu::MapSurfaceToOutput(MapSurfaceToOutputPdu {
                surface_id: 0,
                output_origin_x: 0,
                output_origin_y: 0,
            })) {
                raw_pdus.extend_from_slice(&data);
            }

            state.surface_created = true;
            state.progressive_context = None;
            info!("GFX surface created: {}x{} (enc: {}x{})", state.width, state.height, enc_w, enc_h);
        }

        let frame_id = state.next_frame_id();

        // StartFrame
        if let Ok(data) = encode_vec(&ServerPdu::StartFrame(StartFramePdu {
            timestamp: Timestamp {
                milliseconds: 0,
                seconds: 0,
                minutes: 0,
                hours: 0,
            },
            frame_id,
        })) {
            raw_pdus.extend_from_slice(&data);
        }

        let make_rect = || InclusiveRectangle {
            left: 0,
            top: 0,
            right: frame.width,   // RDPGFX_RECT16 exclusive bound = visible crop
            bottom: frame.height, // RDPGFX_RECT16 exclusive bound = visible crop
        };

        let make_dest_rect = || InclusiveRectangle {
            left: 0,
            top: 0,
            right: frame.width,   // RDPGFX_RECT16 exclusive bound = match surface/desktop
            bottom: frame.height, // RDPGFX_RECT16 exclusive bound = match surface/desktop
        };

        let make_qq = || QuantQuality {
            quantization_parameter: 22,
            progressive: false,
            quality: 100,
        };

        // Choose AVC444 or AVC420 path based on available data and negotiated caps
        let use_avc444 = frame.h264_aux.is_some()
            && state.avc444_supported
            && state.avc444_enabled;

        if use_avc444 {
            // AVC444 path: WireToSurface1 + Avc444BitmapStream
            let aux_data = frame.h264_aux.as_ref().unwrap();
            let avc444_stream = Avc444BitmapStream {
                encoding: Encoding::LUMA_AND_CHROMA,
                stream1: Avc420BitmapStream {
                    rectangles: vec![make_rect()],
                    quant_qual_vals: vec![make_qq()],
                    data: &frame.h264_data,
                },
                stream2: Some(Avc420BitmapStream {
                    rectangles: vec![make_rect()],
                    quant_qual_vals: vec![make_qq()],
                    data: aux_data,
                }),
            };

            if let Ok(avc444_data) = encode_vec(&avc444_stream) {
                if let Ok(data) = encode_vec(&ServerPdu::WireToSurface1(WireToSurface1Pdu {
                    surface_id: 0,
                    codec_id: Codec1Type::Avc444,
                    pixel_format: GfxPixelFormat::XRgb,
                    destination_rectangle: make_dest_rect(),
                    bitmap_data: avc444_data,
                })) {
                    raw_pdus.extend_from_slice(&data);
                }
            }
        } else {
            // AVC420 path: WireToSurface1 + Avc420BitmapStream
            let avc_stream = Avc420BitmapStream {
                rectangles: vec![make_rect()],
                quant_qual_vals: vec![make_qq()],
                data: &frame.h264_data,
            };

            if let Ok(avc_data) = encode_vec(&avc_stream) {
                if let Ok(data) = encode_vec(&ServerPdu::WireToSurface1(WireToSurface1Pdu {
                    surface_id: 0,
                    codec_id: Codec1Type::Avc420,
                    pixel_format: GfxPixelFormat::XRgb,
                    destination_rectangle: make_dest_rect(),
                    bitmap_data: avc_data,
                })) {
                    raw_pdus.extend_from_slice(&data);
                }
            }
        }

        // EndFrame
        if let Ok(data) = encode_vec(&ServerPdu::EndFrame(EndFramePdu { frame_id })) {
            raw_pdus.extend_from_slice(&data);
        }

        debug!(
            frame_id,
            raw_bytes = raw_pdus.len(),
            h264_bytes = frame.h264_data.len(),
            "GFX frame PDU created",
        );

        // Single ZGFX wrap for all concatenated PDUs
        wrap_zgfx(&raw_pdus)
    }

    /// Surface setup when needed (first frame, or the frame size changed), then StartFrame.
    /// Before the first surface, adopt the frame size: ResetGraphics and CreateSurface must
    /// both match the captured display. Returns the frame id.
    fn begin_surface_frame(state: &mut GfxState, width: u16, height: u16, raw_pdus: &mut Vec<u8>) -> u32 {
        if !state.surface_created {
            state.width = width;
            state.height = height;
        } else if state.width != width || state.height != height {
            info!(
                old_w = state.width, old_h = state.height,
                new_w = width, new_h = height,
                "GFX resolution changed, rebuilding surface"
            );
            state.width = width;
            state.height = height;
            state.surface_created = false;
        }

        // First frame: surface setup (same as H.264 path)
        if !state.surface_created {
            if let Ok(data) = encode_vec(&ServerPdu::ResetGraphics(ResetGraphicsPdu {
                width: state.width as u32,
                height: state.height as u32,
                monitors: vec![Monitor {
                    left: 0,
                    top: 0,
                    right: state.width as i32 - 1,
                    bottom: state.height as i32 - 1,
                    flags: MonitorFlags::PRIMARY,
                }],
            })) {
                raw_pdus.extend_from_slice(&data);
            }

            if let Ok(data) = encode_vec(&ServerPdu::CreateSurface(CreateSurfacePdu {
                surface_id: 0,
                width,
                height,
                pixel_format: GfxPixelFormat::XRgb,
            })) {
                raw_pdus.extend_from_slice(&data);
            }

            if let Ok(data) = encode_vec(&ServerPdu::MapSurfaceToOutput(MapSurfaceToOutputPdu {
                surface_id: 0,
                output_origin_x: 0,
                output_origin_y: 0,
            })) {
                raw_pdus.extend_from_slice(&data);
            }

            state.surface_created = true;
            state.progressive_context = None;
            info!("GFX surface created: {}x{}", width, height);
        }

        let frame_id = state.next_frame_id();

        if let Ok(data) = encode_vec(&ServerPdu::StartFrame(StartFramePdu {
            timestamp: Timestamp {
                milliseconds: 0,
                seconds: 0,
                minutes: 0,
                hours: 0,
            },
            frame_id,
        })) {
            raw_pdus.extend_from_slice(&data);
        }
        frame_id
    }

    /// EndFrame for `frame_id`, then a single ZGFX wrap of the whole frame.
    fn end_surface_frame(frame_id: u32, mut raw_pdus: Vec<u8>) -> Vec<u8> {
        if let Ok(data) = encode_vec(&ServerPdu::EndFrame(EndFramePdu { frame_id })) {
            raw_pdus.extend_from_slice(&data);
        }
        wrap_zgfx(&raw_pdus)
    }

    /// Create a ZGFX-wrapped buffer for uncompressed dirty rect updates.
    /// Uses WireToSurface1 with Codec1Type::Uncompressed for each rect.
    /// First call also includes surface setup PDUs.
    pub fn create_uncompressed_pdu(state: &mut GfxState, update: &GfxUncompressedUpdate) -> Vec<u8> {
        let mut raw_pdus = Vec::new();
        let frame_id = Self::begin_surface_frame(state, update.width, update.height, &mut raw_pdus);

        // WireToSurface1 per dirty rect with Codec1Type::Uncompressed
        let mut total_bytes = 0usize;
        for rect in &update.rects {
            if let Ok(data) = encode_vec(&ServerPdu::WireToSurface1(WireToSurface1Pdu {
                surface_id: 0,
                codec_id: Codec1Type::Uncompressed,
                pixel_format: GfxPixelFormat::XRgb,
                destination_rectangle: InclusiveRectangle {
                    left: rect.x,
                    top: rect.y,
                    right: rect.x + rect.width,
                    bottom: rect.y + rect.height,
                },
                bitmap_data: rect.pixel_data.to_vec(),
            })) {
                total_bytes += rect.pixel_data.len();
                raw_pdus.extend_from_slice(&data);
            }
        }

        debug!(
            frame_id,
            rects = update.rects.len(),
            raw_bytes = total_bytes,
            "GFX uncompressed frame PDU created",
        );

        Self::end_surface_frame(frame_id, raw_pdus)
    }

    /// Create a ZGFX-wrapped buffer for one RemoteFX Progressive update: WireToSurface2 with
    /// RDPGFX_CODECID_CAPROGRESSIVE. First call also includes surface setup PDUs.
    pub fn create_remotefx_pdu(state: &mut GfxState, update: &GfxRemoteFxUpdate) -> Vec<u8> {
        let mut raw_pdus = Vec::new();
        let frame_id = Self::begin_surface_frame(state, update.width, update.height, &mut raw_pdus);

        // A new surface gets a new codec context; its first message carries SYNC and CONTEXT.
        let (codec_context_id, header) = match state.progressive_context {
            Some(id) => (id, false),
            None => {
                let id = state.next_codec_context;
                state.next_codec_context = id.wrapping_add(1);
                state.progressive_context = Some(id);
                (id, true)
            }
        };
        let bitmap_data = crate::encoder::rfx::progressive_message(&update.data, frame_id, header);
        let rfx_bytes = bitmap_data.len();
        if let Ok(data) = encode_vec(&ServerPdu::WireToSurface2(WireToSurface2Pdu {
            surface_id: 0,
            codec_id: Codec2Type::RemoteFxProgressive,
            codec_context_id,
            pixel_format: GfxPixelFormat::XRgb,
            bitmap_data,
        })) {
            raw_pdus.extend_from_slice(&data);
        }

        debug!(
            frame_id, codec_context_id, header, tiles = update.tiles, rfx_bytes,
            "GFX RemoteFX Progressive frame PDU created"
        );

        Self::end_surface_frame(frame_id, raw_pdus)
    }
}

impl DvcProcessor for GfxHandler {
    fn channel_name(&self) -> &str {
        GFX_CHANNEL_NAME
    }

    fn start(&mut self, channel_id: u32) -> PduResult<Vec<DvcMessage>> {
        info!(channel_id, "GFX channel opened");
        let mut state = self.state.lock().unwrap();
        state.channel_id = Some(channel_id);
        Ok(Vec::new())
    }

    fn process(&mut self, _channel_id: u32, payload: &[u8]) -> PduResult<Vec<DvcMessage>> {
        // Client GFX data is also ZGFX-wrapped. Unwrap the ZGFX layer first.
        let raw_data = unwrap_zgfx(payload);
        let data = raw_data.as_deref().unwrap_or(payload);

        let client_pdu: ClientPdu = match decode_client_pdu(data) {
            Ok(pdu) => pdu,
            Err(e) if data.starts_with(&[0x12, 0x00]) => {
                // Without a CapsConfirm the client waits forever on "Configuring remote PC".
                warn!(payload = ?&data[..data.len().min(64)], "GFX: undecodable CapabilitiesAdvertise: {e}");
                return Ok(Vec::new());
            }
            Err(e) => {
                // Unknown PDU type (e.g., QoE FrameAcknowledge 0x16, CacheImportOffer 0x10)
                // Log and ignore rather than crashing the connection
                debug!(
                    payload_len = data.len(),
                    first_bytes = ?&data[..data.len().min(8)],
                    "GFX: ignoring unknown client PDU: {e}"
                );
                return Ok(Vec::new());
            }
        };

        match client_pdu {
            ClientPdu::CapabilitiesAdvertise(caps) => {
                let cap_sets = &caps.0;
                info!(sets = ?cap_sets, "GFX client capabilities: {} sets", cap_sets.len());

                let mut state = self.state.lock().unwrap();

                // If already confirmed, ignore duplicate CapabilitiesAdvertise
                if state.caps_confirmed {
                    info!("GFX caps already confirmed, ignoring duplicate CapabilitiesAdvertise");
                    return Ok(Vec::new());
                }

                let negotiated = negotiate_caps(cap_sets);
                state.avc420_supported = negotiated.avc420;
                state.avc444_supported = negotiated.avc444;
                let confirmed = negotiated.confirmed;

                info!(
                    avc420 = state.avc420_supported,
                    avc444_client = state.avc444_supported,
                    avc444_enabled = state.avc444_enabled,
                    ?confirmed,
                    "GFX capabilities negotiated"
                );

                // Send CapabilitiesConfirm from the DVC handler so it goes through
                // DrdynvcServer's proper encoding path. Bitmaps are suppressed once
                // GFX channel is open, so no bitmap/GFX mixing will occur.
                state.confirmed_cap = Some(confirmed.clone());
                state.caps_confirmed = true;
                state.request_idr();
                state.caps_notify.notify_one();

                let confirm_pdu = ServerPdu::CapabilitiesConfirm(CapabilitiesConfirmPdu(confirmed));
                let msg = make_dvc_message(&confirm_pdu)?;
                info!("GFX CapabilitiesConfirm sent via DVC handler");
                Ok(vec![msg])
            }

            ClientPdu::FrameAcknowledge(ack) => {
                let mut state = self.state.lock().unwrap();
                state.ack_frame(ack.frame_id, ack.queue_depth, Instant::now());

                // Log stats every 60 acked frames (~1 second at 60fps)
                if ack.frame_id % 60 == 0 {
                    let net_ms = (state.rtt_ewma_ms - state.last_encode_ms).max(0.0);

                    // Compute instant bitrate stats from samples since last log
                    let n = state.bitrate_samples.len() as f64;
                    let (inst_avg, inst_std) = if n > 0.0 {
                        let sum: f64 = state.bitrate_samples.iter().sum();
                        let avg = sum / n;
                        let var: f64 = state.bitrate_samples.iter().map(|x| (x - avg).powi(2)).sum::<f64>() / n;
                        (avg, var.sqrt())
                    } else {
                        (0.0, 0.0)
                    };
                    let inst_max = if state.bitrate_max > 0.0 { state.bitrate_max } else { 0.0 };
                    let inst_min = if state.bitrate_min < f64::MAX { state.bitrate_min } else { 0.0 };

                    info!(
                        "RTT {:.1}ms | encode {:.1}ms | net {:.1}ms | instant {:.1}/{:.1}/{:.1} Mbps (avg/max/min) | std {:.1} | {}KB/f | {} pending",
                        state.rtt_ewma_ms,
                        state.last_encode_ms,
                        net_ms,
                        inst_avg,
                        inst_max,
                        inst_min,
                        inst_std,
                        state.last_frame_bytes / 1024,
                        state.pending_acks,
                    );

                    // Reset for next window
                    state.bitrate_samples.clear();
                    state.bitrate_max = 0.0;
                    state.bitrate_min = f64::MAX;
                }
                Ok(Vec::new())
            }
        }
    }

    fn close(&mut self, channel_id: u32) {
        info!(channel_id, "GFX channel closed");
        let mut state = self.state.lock().unwrap();
        state.channel_id = None;
        state.surface_created = false;
        state.caps_confirmed = false;
        state.avc420_supported = false;
        state.avc444_supported = false;
        state.confirmed_cap = None;
    }
}

impl DvcServerProcessor for GfxHandler {}

#[cfg(test)]
mod tests {
    use super::*;

    /// RDPGFX_CAPS_ADVERTISE_PDU with the given (version, caps data) sets.
    fn caps_advertise(sets: &[(u32, &[u8])]) -> Vec<u8> {
        let body: usize = sets.iter().map(|(_, d)| 8 + d.len()).sum();
        let mut pdu = Vec::new();
        pdu.extend_from_slice(&0x0012u16.to_le_bytes());
        pdu.extend_from_slice(&0u16.to_le_bytes());
        pdu.extend_from_slice(&((8 + 2 + body) as u32).to_le_bytes());
        pdu.extend_from_slice(&(sets.len() as u16).to_le_bytes());
        for (version, data) in sets {
            pdu.extend_from_slice(&version.to_le_bytes());
            pdu.extend_from_slice(&(data.len() as u32).to_le_bytes());
            pdu.extend_from_slice(data);
        }
        pdu
    }

    #[test]
    fn caps_advertise_with_unknown_version_still_decodes() {
        // A client advertising a version ironrdp-pdu does not know (0xB0000) next to V10.7.
        let data = caps_advertise(&[(0x000B_0000, &[0, 0, 0, 0]), (0x000A_0701, &[0, 0, 0, 0])]);
        assert!(decode::<ClientPdu>(&data).is_err(), "precondition: plain decode rejects it");

        let pdu = decode_client_pdu(&data).expect("unknown caps set must be skipped");
        let ClientPdu::CapabilitiesAdvertise(caps) = pdu else { panic!("not a caps advertise") };
        assert_eq!(caps.0.len(), 1);
        assert!(matches!(caps.0[0], CapabilitySet::V10_7 { .. }));
    }

    #[test]
    fn known_caps_advertise_decodes_unchanged() {
        let data = caps_advertise(&[(0x000A_0701, &[0, 0, 0, 0]), (0x0008_0004, &[0, 0, 0, 0])]);
        let ClientPdu::CapabilitiesAdvertise(caps) = decode_client_pdu(&data).unwrap() else {
            panic!("not a caps advertise")
        };
        assert_eq!(caps.0.len(), 2);
    }

    fn sent(n: u32) -> GfxState {
        let mut s = GfxState::new(64, 64, false);
        for _ in 0..n {
            s.next_frame_id();
        }
        s
    }

    #[test]
    fn gate_allows_up_to_two_pending() {
        let mut s = sent(2);
        assert!(s.can_send(Instant::now()));
        s.next_frame_id();
        assert!(!s.can_send(Instant::now()));
    }

    #[tokio::test]
    async fn ack_reopens_gate() {
        let mut s = sent(3);
        assert!(!s.can_send(Instant::now()));
        let notify = Arc::clone(&s.ack_notify);
        let woken = notify.notified();
        s.ack_frame(1, QueueDepth::Unavailable, Instant::now());
        assert!(s.can_send(Instant::now()));
        assert!(tokio::time::timeout(Duration::from_millis(100), woken).await.is_ok());
    }

    #[test]
    fn suspend_disables_gate() {
        let mut s = sent(3);
        s.ack_frame(1, QueueDepth::Suspend, Instant::now());
        for _ in 0..5 {
            s.next_frame_id();
        }
        assert!(s.can_send(Instant::now()));
        // Resuming must not inherit the frames sent while suspended (never acked).
        s.ack_frame(2, QueueDepth::AvailableBytes(0), Instant::now());
        assert!(s.can_send(Instant::now()));
        for _ in 0..3 {
            s.next_frame_id();
        }
        assert!(!s.can_send(Instant::now()));
    }

    #[test]
    fn stall_recovers_with_idr() {
        let mut s = sent(3);
        let t = s.last_frame_time.expect("frame sent");
        assert!(!s.can_send(t + Duration::from_millis(900)));
        assert!(s.can_send(t + Duration::from_millis(1100)));
        assert_eq!(s.pending_acks, 0);
        assert!(s.take_idr_request());
        assert!(!s.take_idr_request());
    }

    #[test]
    fn no_false_stall_while_acks_flow() {
        let mut s = sent(4);
        let t = s.last_frame_time.expect("frame sent");
        s.ack_frame(1, QueueDepth::Unavailable, t + Duration::from_millis(900));
        assert!(!s.can_send(t + Duration::from_millis(1500)));
        assert!(!s.take_idr_request());
    }

    /// One frame acked `rtt` after it was sent, at `fps`.
    fn with_rtt(rtt: Duration, fps: u32) -> GfxState {
        let mut s = sent(1);
        s.frame_rate = fps;
        s.ack_frame(1, QueueDepth::Unavailable, Instant::now() + rtt);
        s
    }

    #[test]
    fn wan_rtt_widens_gate_by_baseline_in_flight() {
        // 100 ms at 60 fps: 6 frames are in flight on an idle path.
        let mut s = with_rtt(Duration::from_millis(100), 60);
        for _ in 0..8 {
            s.next_frame_id();
        }
        assert!(s.can_send(Instant::now()));
        assert_eq!(s.backlog(), 2);
        s.next_frame_id();
        assert!(!s.can_send(Instant::now()));
        assert_eq!(s.backlog(), 3);
    }

    #[test]
    fn lan_rtt_keeps_two_frame_gate() {
        let mut s = with_rtt(Duration::from_millis(10), 60);
        for _ in 0..3 {
            s.next_frame_id();
        }
        assert!(!s.can_send(Instant::now()));
        assert_eq!(s.backlog(), 3);
    }

    #[test]
    fn rfx_quality_defaults_to_auto_at_medium() {
        let mut s = GfxState::new(64, 64, false);
        for auto in [None, Some("auto"), Some("best")] {
            s.set_rfx_quality(Some("high"));
            s.set_rfx_quality(auto);
            assert!(s.rfx_auto, "{auto:?}");
            assert_eq!(s.rfx_quality, RfxQuality::Medium, "{auto:?}");
        }
        s.set_rfx_quality(Some("low"));
        assert!(!s.rfx_auto);
        assert_eq!(s.rfx_quality, RfxQuality::Low);
        assert_eq!(RfxQuality::Low.as_str(), "low");
    }

    #[test]
    fn baseline_follows_minimum_rtt() {
        let mut s = with_rtt(Duration::from_millis(100), 60);
        s.next_frame_id();
        s.ack_frame(2, QueueDepth::Unavailable, Instant::now() + Duration::from_millis(400));
        assert_eq!(s.baseline_in_flight(), 6);
    }

    #[test]
    fn frame_in_the_channel_counts_toward_the_gate() {
        let mut s = sent(2);
        assert!(s.can_send(Instant::now()));
        // The pipeline hands frame 3 to the session, which has not counted it yet.
        s.frame_handed_off();
        assert!(!s.can_send(Instant::now()));
        s.next_frame_id();
        assert_eq!(s.backlog(), 3);
    }

    fn confirmed(avc420: bool) -> GfxState {
        let mut s = GfxState::new(816, 1544, false);
        s.channel_id = Some(7);
        s.caps_confirmed = true;
        s.avc420_supported = avc420;
        s
    }

    #[test]
    fn gfx_ready_without_avc() {
        let s = confirmed(false);
        assert!(s.is_ready(), "GFX confirmed without AVC is still a usable GFX channel");
        assert!(!s.avc_ready());
        let s = confirmed(true);
        assert!(s.is_ready() && s.avc_ready());
        let mut s = confirmed(true);
        s.caps_confirmed = false;
        assert!(!s.is_ready() && !s.avc_ready());
    }

    /// The iOS Windows App's known sets (its unknown 0xB01xx sets are skipped by the decoder).
    fn ios_caps() -> Vec<CapabilitySet> {
        use ironrdp_pdu::rdp::vc::dvc::gfx::CapabilitiesV8Flags;
        vec![
            CapabilitySet::V8 { flags: CapabilitiesV8Flags::SMALL_CACHE },
            CapabilitySet::V8_1 { flags: CapabilitiesV81Flags::SMALL_CACHE },
            CapabilitySet::V10 { flags: CapabilitiesV10Flags::AVC_DISABLED },
            CapabilitySet::V10_2 { flags: CapabilitiesV10Flags::AVC_DISABLED },
            CapabilitySet::V10_3 { flags: CapabilitiesV103Flags::AVC_DISABLED },
            CapabilitySet::V10_4 { flags: CapabilitiesV104Flags::AVC_DISABLED },
        ]
    }

    #[test]
    fn no_avc_client_gets_highest_version_confirmed() {
        let n = negotiate_caps(&ios_caps());
        assert_eq!(n.confirmed, CapabilitySet::V10_4 { flags: CapabilitiesV104Flags::AVC_DISABLED });
        assert!(!n.avc420 && !n.avc444);
    }

    #[test]
    fn avc_client_negotiation_unchanged() {
        let mut caps = ios_caps();
        caps.push(CapabilitySet::V10_7 { flags: CapabilitiesV107Flags::empty() });
        let n = negotiate_caps(&caps);
        assert!(matches!(n.confirmed, CapabilitySet::V10_7 { .. }));
        assert!(n.avc420 && n.avc444);
        let n = negotiate_caps(&[CapabilitySet::V8_1 { flags: CapabilitiesV81Flags::AVC420_ENABLED }]);
        assert!(n.avc420 && !n.avc444);
    }

    #[test]
    fn v10_4_caps_confirm_encodes() {
        let pdu = ServerPdu::CapabilitiesConfirm(CapabilitiesConfirmPdu(CapabilitySet::V10_4 {
            flags: CapabilitiesV104Flags::AVC_DISABLED,
        }));
        let raw = encode_vec(&pdu).unwrap();
        // header(type 0x0013, flags 0, length 20) + version 0xA0400 + dataLength 4 + flags
        let mut want = vec![0x13, 0, 0, 0, 20, 0, 0, 0, 0x00, 0x04, 0x0A, 0x00, 4, 0, 0, 0];
        want.extend_from_slice(&CapabilitiesV104Flags::AVC_DISABLED.bits().to_le_bytes());
        assert_eq!(raw, want);
    }

    #[test]
    fn zgfx_multipart_round_trips() {
        let data: Vec<u8> = (0..200_000u32).map(|i| i as u8).collect();
        let wrapped = wrap_zgfx(&data);
        assert_eq!(wrapped[0], 0xE1);
        assert_eq!(u16::from_le_bytes([wrapped[1], wrapped[2]]), 4);
        assert_eq!(u32::from_le_bytes([wrapped[3], wrapped[4], wrapped[5], wrapped[6]]), 200_000);
        let first_seg = u32::from_le_bytes([wrapped[7], wrapped[8], wrapped[9], wrapped[10]]);
        assert_eq!(first_seg, 65_535, "segmentSize counts the compression byte and fits u16 range");
        assert_eq!(unwrap_zgfx(&wrapped).unwrap(), data);
    }

    fn decode_all(raw: &[u8]) -> Vec<ServerPdu> {
        let mut cur = ironrdp_core::ReadCursor::new(raw);
        let mut out = Vec::new();
        while !cur.is_empty() {
            out.push(<ServerPdu as ironrdp_core::Decode>::decode(&mut cur).expect("valid GFX PDU"));
        }
        out
    }

    #[test]
    fn uncompressed_frame_sets_up_surface_at_frame_size() {
        use crate::display::UncompressedRect;
        // GfxState still holds a stale size; the surface must follow the frame (816x1544).
        let mut s = confirmed(false);
        s.width = 1920;
        s.height = 1080;
        let (w, h) = (816u16, 100u16);
        let update = GfxUncompressedUpdate {
            rects: vec![UncompressedRect {
                x: 0, y: 0, width: w, height: h,
                pixel_data: bytes::Bytes::from(vec![0xAB; w as usize * h as usize * 4]),
            }],
            width: 816,
            height: 1544,
        };
        let wrapped = GfxHandler::create_uncompressed_pdu(&mut s, &update);
        assert_eq!(wrapped[0], 0xE1, "326 KB frame needs multipart ZGFX");
        let pdus = decode_all(&unwrap_zgfx(&wrapped).unwrap());
        let ServerPdu::ResetGraphics(reset) = &pdus[0] else { panic!("ResetGraphics first: {pdus:?}") };
        assert_eq!((reset.width, reset.height), (816, 1544));
        let ServerPdu::CreateSurface(cs) = &pdus[1] else { panic!("CreateSurface second") };
        assert_eq!((cs.width, cs.height), (816, 1544));
        assert!(matches!(pdus[2], ServerPdu::MapSurfaceToOutput(_)));
        let ServerPdu::StartFrame(start) = &pdus[3] else { panic!("StartFrame") };
        let ServerPdu::WireToSurface1(w2s) = &pdus[4] else { panic!("WireToSurface1") };
        assert_eq!(w2s.codec_id, Codec1Type::Uncompressed);
        assert_eq!(w2s.bitmap_data.len(), w as usize * h as usize * 4);
        assert_eq!((w2s.destination_rectangle.right, w2s.destination_rectangle.bottom), (816, 100));
        let ServerPdu::EndFrame(end) = &pdus[5] else { panic!("EndFrame") };
        assert_eq!(start.frame_id, end.frame_id);
        assert_eq!(pdus.len(), 6);

        // Next update: no surface setup, still framed.
        let pdus = decode_all(&unwrap_zgfx(&GfxHandler::create_uncompressed_pdu(&mut s, &update)).unwrap());
        assert!(matches!(pdus[0], ServerPdu::StartFrame(_)));
        assert!(matches!(pdus.last(), Some(ServerPdu::EndFrame(_))));
    }

    #[test]
    fn remotefx_frame_is_progressive_wire_to_surface2() {
        use crate::encoder::rfx::progressive_message;
        let mut s = confirmed(false);
        let (w, h) = (816u16, 1544u16);
        let px = vec![0x80u8; usize::from(w) * usize::from(h) * 4];
        let rect = |x, y, width, height| ironrdp_pdu::codecs::rfx::RfxRectangle { x, y, width, height };
        let update = encode_gfx_progressive(&px, usize::from(w) * 4, w, h, &[rect(100, 100, 10, 10)], RfxQuality::Medium).expect("encode");
        let w2s = |pdus: &[ServerPdu], i: usize| match &pdus[i] {
            ServerPdu::WireToSurface2(p) => p.clone(),
            other => panic!("WireToSurface2 at {i}: {other:?}"),
        };

        let pdus = decode_all(&unwrap_zgfx(&GfxHandler::create_remotefx_pdu(&mut s, &update)).unwrap());
        let ServerPdu::CreateSurface(cs) = &pdus[1] else { panic!("CreateSurface: {pdus:?}") };
        assert_eq!((cs.width, cs.height, cs.pixel_format), (w, h, GfxPixelFormat::XRgb));
        let ServerPdu::StartFrame(start) = &pdus[3] else { panic!("StartFrame") };
        let first = w2s(&pdus, 4);
        assert_eq!(first.surface_id, 0);
        assert_eq!(first.codec_id, Codec2Type::RemoteFxProgressive);
        assert_eq!(first.codec_id as u16, 0x0009, "RDPGFX_CODECID_CAPROGRESSIVE");
        assert_eq!(first.pixel_format, GfxPixelFormat::XRgb);
        // First message of the codec context carries SYNC and CONTEXT.
        assert_eq!(first.bitmap_data, progressive_message(&update.data, start.frame_id, true));
        assert!(matches!(pdus[5], ServerPdu::EndFrame(_)));
        assert_eq!(pdus.len(), 6);

        // Same surface: no setup, same codec context, no SYNC/CONTEXT.
        let pdus = decode_all(&unwrap_zgfx(&GfxHandler::create_remotefx_pdu(&mut s, &update)).unwrap());
        assert_eq!(pdus.len(), 3);
        let ServerPdu::StartFrame(start) = &pdus[0] else { panic!("StartFrame") };
        let next = w2s(&pdus, 1);
        assert_eq!(next.codec_context_id, first.codec_context_id);
        assert_eq!(next.bitmap_data, progressive_message(&update.data, start.frame_id, false));

        // A rebuilt surface starts a new codec context.
        s.surface_created = false;
        let pdus = decode_all(&unwrap_zgfx(&GfxHandler::create_remotefx_pdu(&mut s, &update)).unwrap());
        let ServerPdu::StartFrame(start) = &pdus[3] else { panic!("StartFrame") };
        let rebuilt = w2s(&pdus, 4);
        assert_ne!(rebuilt.codec_context_id, first.codec_context_id);
        assert_eq!(rebuilt.bitmap_data, progressive_message(&update.data, start.frame_id, true));
    }

    #[test]
    fn caps_confirm_requests_idr() {
        let mut s = GfxState::new(64, 64, false);
        assert!(!s.take_idr_request());
        s.request_idr();
        s.request_idr();
        assert!(s.take_idr_request());
        assert!(!s.take_idr_request());
    }
}
