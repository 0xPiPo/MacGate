use std::io;

use ironrdp_acceptor::DesktopSize;
use ironrdp_core::{cast_int, cast_length, other_err, Encode as _, EncodeResult};
use ironrdp_graphics::color_conversion::to_64x64_ycbcr_tile;
use ironrdp_graphics::{dwt, quantization, rfx_encode_component, subband_reconstruction};
use ironrdp_graphics::rlgr::RlgrError;
use ironrdp_pdu::codecs::rfx::{
    self, Block, ChannelsPdu, CodecChannel, CodecVersionsPdu, FrameBeginPdu, FrameEndPdu, OperatingMode, Quant,
    RegionPdu, RfxChannel, RfxRectangle, SyncPdu, TileSetPdu,
};
use ironrdp_pdu::rdp::capability_sets::EntropyBits;
use ironrdp_pdu::WriteCursor;

use crate::{BitmapUpdate, GfxRemoteFxUpdate, PixelFormat};

#[derive(Debug, Clone)]
pub(crate) struct RfxEncoder {
    entropy_algorithm: rfx::EntropyAlgorithm,
}

impl RfxEncoder {
    pub(crate) fn new(entropy_bits: EntropyBits) -> Self {
        let entropy_algorithm = match entropy_bits {
            EntropyBits::Rlgr1 => rfx::EntropyAlgorithm::Rlgr1,
            EntropyBits::Rlgr3 => rfx::EntropyAlgorithm::Rlgr3,
        };
        Self { entropy_algorithm }
    }

    pub(crate) fn encode(
        &mut self,
        bitmap: &BitmapUpdate,
        output: &mut [u8],
        desktop_size: Option<DesktopSize>,
    ) -> EncodeResult<usize> {
        let mut cursor = WriteCursor::new(output);
        let entropy_algorithm = self.entropy_algorithm;

        // header messages
        if let Some(desktop_size) = desktop_size {
            let width = desktop_size.width;
            let height = desktop_size.height;
            Block::Sync(SyncPdu).encode(&mut cursor)?;
            let context = rfx::ContextPdu {
                flags: OperatingMode::IMAGE_MODE,
                entropy_algorithm,
            };
            Block::CodecChannel(CodecChannel::Context(context)).encode(&mut cursor)?;

            let channels = ChannelsPdu(vec![RfxChannel {
                width: cast_length!("width", width)?,
                height: cast_length!("height", height)?,
            }]);
            Block::Channels(channels).encode(&mut cursor)?;

            Block::CodecVersions(CodecVersionsPdu).encode(&mut cursor)?;
        }

        // data messages
        let frame_begin = FrameBeginPdu {
            index: 0,
            number_of_regions: 1,
        };
        Block::CodecChannel(CodecChannel::FrameBegin(frame_begin)).encode(&mut cursor)?;

        let width = bitmap.width.get();
        let height = bitmap.height.get();
        let rectangles = vec![RfxRectangle {
            x: 0,
            y: 0,
            width,
            height,
        }];
        let region = RegionPdu { rectangles };
        Block::CodecChannel(CodecChannel::Region(region)).encode(&mut cursor)?;

        let quant = Quant::default();

        let (encoder, mut data) = UpdateEncoder::new(bitmap, quant.clone(), entropy_algorithm);
        let tiles = encoder.encode(&mut data)?;

        let quants = vec![quant];
        let tile_set = TileSetPdu {
            entropy_algorithm,
            quants,
            tiles,
        };
        Block::CodecChannel(CodecChannel::TileSet(tile_set)).encode(&mut cursor)?;

        let frame_end = FrameEndPdu;
        Block::CodecChannel(CodecChannel::FrameEnd(frame_end)).encode(&mut cursor)?;

        Ok(cursor.pos())
    }
}

pub(crate) struct UpdateEncoder<'a> {
    bitmap: &'a BitmapUpdate,
    quant: Quant,
    entropy_algorithm: rfx::EntropyAlgorithm,
}

struct UpdateEncoderData(Vec<u8>);

struct EncodedTile<'a> {
    y_data: &'a [u8],
    cb_data: &'a [u8],
    cr_data: &'a [u8],
}

impl<'a> UpdateEncoder<'a> {
    fn new(
        bitmap: &'a BitmapUpdate,
        quant: Quant,
        entropy_algorithm: rfx::EntropyAlgorithm,
    ) -> (Self, UpdateEncoderData) {
        let this = Self {
            bitmap,
            quant,
            entropy_algorithm,
        };
        let data = this.alloc_data();

        (this, data)
    }

    fn alloc_data(&self) -> UpdateEncoderData {
        let (tiles_x, tiles_y) = self.tiles_xy();

        UpdateEncoderData(vec![0u8; 64 * 64 * 3 * tiles_x * tiles_y])
    }

    fn tiles_xy(&self) -> (usize, usize) {
        (
            self.bitmap.width.get().div_ceil(64).into(),
            self.bitmap.height.get().div_ceil(64).into(),
        )
    }

    fn encode(&self, data: &'a mut UpdateEncoderData) -> EncodeResult<Vec<rfx::Tile<'a>>> {
        #[cfg(feature = "rayon")]
        use rayon::prelude::*;

        let (tiles_x, tiles_y) = self.tiles_xy();

        #[cfg(not(feature = "rayon"))]
        let chunks = data.0.chunks_mut(64 * 64 * 3);
        #[cfg(feature = "rayon")]
        let chunks = data.0.par_chunks_mut(64 * 64 * 3);

        let tiles: Vec<_> = (0..tiles_y).flat_map(|y| (0..tiles_x).map(move |x| (x, y))).collect();

        chunks
            .zip(tiles)
            .map(|(buf, (tile_x, tile_y))| {
                let EncodedTile {
                    y_data,
                    cb_data,
                    cr_data,
                } = self
                    .encode_tile(tile_x, tile_y, buf)
                    .map_err(|e| other_err!("rfxenc", source: e))?;

                let tile = rfx::Tile {
                    y_quant_index: 0,
                    cb_quant_index: 0,
                    cr_quant_index: 0,
                    x: cast_int!("tile_x", tile_x)?,
                    y: cast_int!("tile_y", tile_y)?,
                    y_data,
                    cb_data,
                    cr_data,
                };
                Ok(tile)
            })
            .collect()
    }

    fn encode_tile<'b>(&self, tile_x: usize, tile_y: usize, buf: &'b mut [u8]) -> Result<EncodedTile<'b>, RlgrError> {
        #![allow(clippy::similar_names)] // It’s hard to find better names for cr, cb, etc.

        assert!(buf.len() >= 4096 * 3);

        let bpp: usize = self.bitmap.format.bytes_per_pixel().into();
        let width: usize = self.bitmap.width.get().into();
        let height: usize = self.bitmap.height.get().into();

        let x = tile_x * 64;
        let y = tile_y * 64;
        let tile_width = u32::try_from(core::cmp::min(width - x, 64)).expect("can always fit in u32");
        let tile_height = u32::try_from(core::cmp::min(height - y, 64)).expect("can always fit in u32");
        let stride = self.bitmap.stride.get();
        let input = &self.bitmap.data[y * stride + x * bpp..];

        let stride = u32::try_from(stride).map_err(io::Error::other)?;
        let y = &mut [0i16; 4096];
        let cb = &mut [0i16; 4096];
        let cr = &mut [0i16; 4096];

        to_64x64_ycbcr_tile(input, tile_width, tile_height, stride, self.bitmap.format, y, cb, cr)
            .map_err(RlgrError::Yuv)?;

        let (y_data, buf) = buf.split_at_mut(4096);
        let (cb_data, cr_data) = buf.split_at_mut(4096);

        let len = rfx_encode_component(y, y_data, &self.quant, self.entropy_algorithm)?;
        let y_data = &y_data[..len];
        let len = rfx_encode_component(cb, cb_data, &self.quant, self.entropy_algorithm)?;
        let cb_data = &cb_data[..len];
        let len = rfx_encode_component(cr, cr_data, &self.quant, self.entropy_algorithm)?;
        let cr_data = &cr_data[..len];

        Ok(EncodedTile {
            y_data,
            cb_data,
            cr_data,
        })
    }
}

/// Quantization for every GFX RemoteFX tile component: the Microsoft RDP server default
/// (LL3/LH3/HL3/HH3 = 6, LH2/HL2 = 7, HH2/LH1/HL1 = 8, HH1 = 9, as in FreeRDP and xrdp's
/// "standard"). Coarser high bands save little on text and blur glyph edges first; finer
/// level-1/2 bands (7,7,7,7,7,8) measured +11% bytes for +2.7 dB on text.
const GFX_RFX_QUANT: Quant = Quant {
    ll3: 6,
    lh3: 6,
    hl3: 6,
    hh3: 6,
    lh2: 7,
    hl2: 7,
    hh2: 8,
    lh1: 8,
    hl1: 8,
    hh1: 9,
};

/// RemoteFX image quality (config `rfx_quality`): the quant every tile component uses.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RfxQuality {
    /// One step coarser in every band but LL3: fewer bytes, softer text edges.
    Low,
    /// The Microsoft server default ([`GFX_RFX_QUANT`]).
    #[default]
    Medium,
    /// Finer level-1/2 bands (7,7,7,7,7,8): about +11% bytes for +2.7 dB on text.
    High,
}

impl RfxQuality {
    /// `"low"`, `"medium"` or `"high"`; anything else (or unset) is medium.
    pub fn from_config(value: Option<&str>) -> Self {
        match value {
            Some("low") => Self::Low,
            Some("high") => Self::High,
            _ => Self::Medium,
        }
    }

    /// The config spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }

    fn quant(self) -> &'static Quant {
        const LOW: Quant = Quant { ll3: 6, lh3: 7, hl3: 7, hh3: 8, lh2: 8, hl2: 8, hh2: 9, lh1: 9, hl1: 9, hh1: 10 };
        const HIGH: Quant = Quant { ll3: 6, lh3: 6, hl3: 6, hh3: 6, lh2: 7, hl2: 7, hh2: 7, lh1: 7, hl1: 7, hh1: 8 };
        match self {
            Self::Low => &LOW,
            Self::Medium => &GFX_RFX_QUANT,
            Self::High => &HIGH,
        }
    }
}

/// Most threads one frame's tiles are split over (a full 816x1544 frame is ~30 ms on one core).
const RFX_THREADS: usize = 4;
/// Updates with fewer tiles (about 3 ms of work) encode on the calling thread.
const RFX_PARALLEL_MIN_TILES: usize = 32;

/// RFX_PROGRESSIVE block types (MS-RDPEGFX 2.2.4.2.1).
const WBT_REGION: u16 = 0xCCC4;
const WBT_TILE_SIMPLE: u16 = 0xCCC5;
/// RFX_PROGRESSIVE_REGION fixed part, before its rects.
const REGION_HEADER: usize = 18;
/// RFX_PROGRESSIVE_TILE_SIMPLE fixed part, before its component data.
const TILE_SIMPLE_HEADER: usize = 22;

/// RemoteFX Progressive for the GFX channel (WireToSurface2, RDPGFX_CODECID_CAPROGRESSIVE):
/// one RFX_PROGRESSIVE_REGION block holding a TILE_SIMPLE for each 64x64 tile of a
/// `width`x`height` BGRA surface (`stride` bytes per row) that intersects `rects`, at `quality`.
///
/// Single pass at full quality, as GNOME Remote Desktop and FreeRDP send it: the original
/// RemoteFX DWT (region flags 0, no RFX_DWT_REDUCE_EXTRAPOLATE), RLGR1, one quant, no
/// progressive quality passes and no tile differencing. Rects and tiles are in surface
/// coordinates. Frame and context blocks come from [`progressive_message`].
pub fn encode_gfx_progressive(
    src: &[u8],
    stride: usize,
    width: u16,
    height: u16,
    rects: &[RfxRectangle],
    quality: RfxQuality,
) -> EncodeResult<GfxRemoteFxUpdate> {
    let q = quality.quant();
    let (w, h) = (usize::from(width), usize::from(height));
    if w == 0 || h == 0 || stride < w * 4 || src.len() < (h - 1) * stride + w * 4 {
        return Err(other_err!("rfxprog", "pixel buffer smaller than the surface"));
    }
    let region: Vec<RfxRectangle> = rects
        .iter()
        .filter(|r| r.x < width && r.y < height && r.width > 0 && r.height > 0)
        .map(|r| RfxRectangle {
            x: r.x,
            y: r.y,
            width: r.width.min(width - r.x),
            height: r.height.min(height - r.y),
        })
        .collect();
    if region.is_empty() {
        return Err(other_err!("rfxprog", "no dirty pixels on the surface"));
    }

    // Tiles touching the region, row-major as (row, column) on the surface grid.
    let mut tiles: Vec<(u16, u16)> = region
        .iter()
        .flat_map(|r| {
            (r.y / 64..=(r.y + r.height - 1) / 64)
                .flat_map(move |ty| (r.x / 64..=(r.x + r.width - 1) / 64).map(move |tx| (ty, tx)))
        })
        .collect();
    tiles.sort_unstable();
    tiles.dedup();

    let (arena, lens) = if tiles.len() < RFX_PARALLEL_MIN_TILES {
        encode_gfx_tiles(src, stride, w, h, &tiles, q)
    } else {
        // Contiguous chunks keep the tiles' order; one short-lived thread per chunk.
        std::thread::scope(|scope| {
            let workers: Vec<_> = tiles
                .chunks(tiles.len().div_ceil(RFX_THREADS))
                .map(|part| scope.spawn(move || encode_gfx_tiles(src, stride, w, h, part, q)))
                .collect();
            let mut arena = Vec::new();
            let mut lens = Vec::with_capacity(tiles.len());
            for worker in workers {
                let (a, l) = worker
                    .join()
                    .map_err(|_| RlgrError::Io(io::Error::other("RFX tile worker panicked")))??;
                arena.extend_from_slice(&a);
                lens.extend(l);
            }
            Ok((arena, lens))
        })
    }
    .map_err(|e| other_err!("rfxprog", source: e))?;

    // RFX_COMPONENT_CODEC_QUANT: band order differs from TS_RFX_CODEC_QUANT (MS-RDPEGFX 2.2.4.2.1.5.2).
    let quant = [
        q.ll3 | q.hl3 << 4,
        q.lh3 | q.hh3 << 4,
        q.hl2 | q.lh2 << 4,
        q.hh2 | q.hl1 << 4,
        q.lh1 | q.hh1 << 4,
    ];
    let tile_data_size = tiles.len() * TILE_SIMPLE_HEADER + arena.len();
    let block_len = REGION_HEADER + region.len() * 8 + quant.len() + tile_data_size;
    let mut data = Vec::with_capacity(block_len);
    data.extend_from_slice(&WBT_REGION.to_le_bytes());
    data.extend_from_slice(&u32::to_le_bytes(cast_length!("blockLen", block_len)?));
    data.push(64); // tileSize
    data.extend_from_slice(&u16::to_le_bytes(cast_length!("numRects", region.len())?));
    data.push(1); // numQuant
    data.push(0); // numProgQuant
    data.push(0); // flags: original DWT
    data.extend_from_slice(&u16::to_le_bytes(cast_length!("numTiles", tiles.len())?));
    data.extend_from_slice(&u32::to_le_bytes(cast_length!("tileDataSize", tile_data_size)?));
    for r in &region {
        for v in [r.x, r.y, r.width, r.height] {
            data.extend_from_slice(&v.to_le_bytes());
        }
    }
    data.extend_from_slice(&quant);
    let mut rest = arena.as_slice();
    for (&(ty, tx), lens) in tiles.iter().zip(&lens) {
        let size = lens.iter().sum::<usize>();
        data.extend_from_slice(&WBT_TILE_SIMPLE.to_le_bytes());
        data.extend_from_slice(&u32::to_le_bytes(cast_length!("blockLen", TILE_SIMPLE_HEADER + size)?));
        data.extend_from_slice(&[0, 0, 0]); // quantIdxY, quantIdxCb, quantIdxCr
        data.extend_from_slice(&tx.to_le_bytes());
        data.extend_from_slice(&ty.to_le_bytes());
        data.push(0); // flags: not a difference tile
        for &len in lens {
            data.extend_from_slice(&u16::to_le_bytes(cast_length!("componentLen", len)?));
        }
        data.extend_from_slice(&0u16.to_le_bytes()); // tailLen
        let (component_data, tail) = rest.split_at(size);
        data.extend_from_slice(component_data);
        rest = tail;
    }

    Ok(GfxRemoteFxUpdate {
        data: bytes::Bytes::from(data),
        tiles: tiles.len(),
        width,
        height,
    })
}

/// One RemoteFX Progressive message (WireToSurface2 bitmap data): SYNC and CONTEXT when
/// `header` (the first message of a codec context), then FRAME_BEGIN, `region` and FRAME_END.
pub(crate) fn progressive_message(region: &[u8], frame_index: u32, header: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(region.len() + 40);
    if header {
        // SYNC: blockLen 12, magic 0xCACCACCA, version 0x0100.
        out.extend_from_slice(&[0xC0, 0xCC, 12, 0, 0, 0, 0xCA, 0xAC, 0xCC, 0xCA, 0x00, 0x01]);
        // CONTEXT: blockLen 10, ctxId 0, tileSize 64, flags 0 (no subband diffing).
        out.extend_from_slice(&[0xC3, 0xCC, 10, 0, 0, 0, 0, 64, 0, 0]);
    }
    // FRAME_BEGIN: blockLen 12, frameIndex, regionCount 1.
    out.extend_from_slice(&[0xC1, 0xCC, 12, 0, 0, 0]);
    out.extend_from_slice(&frame_index.to_le_bytes());
    out.extend_from_slice(&[1, 0]);
    out.extend_from_slice(region);
    // FRAME_END: blockLen 6.
    out.extend_from_slice(&[0xC2, 0xCC, 6, 0, 0, 0]);
    out
}

/// Encode `tiles` ((row, column) on the 64x64 grid) of a `w`x`h` BGRA surface. Returns the
/// coded Y, Cb, Cr data back to back and each tile's three lengths.
fn encode_gfx_tiles(
    src: &[u8],
    stride: usize,
    w: usize,
    h: usize,
    tiles: &[(u16, u16)],
    quant: &Quant,
) -> Result<(Vec<u8>, Vec<[usize; 3]>), RlgrError> {
    let stride32 = u32::try_from(stride).map_err(io::Error::other)?;
    let mut arena = Vec::new();
    let mut lens = Vec::with_capacity(tiles.len());
    let mut temp = [0i16; 4096];
    let (mut y, mut cb, mut cr) = ([0i16; 4096], [0i16; 4096], [0i16; 4096]);
    for &(ty, tx) in tiles {
        let (x0, y0) = (usize::from(tx) * 64, usize::from(ty) * 64);
        let tile_w = u32::try_from((w - x0).min(64)).map_err(io::Error::other)?;
        let tile_h = u32::try_from((h - y0).min(64)).map_err(io::Error::other)?;
        let input = &src[y0 * stride + x0 * 4..];
        to_64x64_ycbcr_tile(input, tile_w, tile_h, stride32, PixelFormat::BgrA32, &mut y, &mut cb, &mut cr)
            .map_err(RlgrError::Yuv)?;
        let mut tile_lens = [0usize; 3];
        for (len, component) in tile_lens.iter_mut().zip([&mut y, &mut cb, &mut cr]) {
            // rfx_encode_component minus its RLGR1 coder (see rlgr1_encode).
            dwt::encode(component, &mut temp);
            quantization::encode(component, quant);
            subband_reconstruction::encode(&mut component[4032..]);
            let start = arena.len();
            rlgr1_encode(component, &mut arena);
            *len = arena.len() - start;
        }
        lens.push(tile_lens);
    }
    Ok((arena, lens))
}

/// RLGR1 entropy coding of one component (MS-RDPRFX 3.1.8.1.7.3), appended to `out`.
/// ironrdp-graphics 0.7 raises kp by UP_GR after a zero in Golomb-Rice mode where the spec
/// and every decoder use UQ_GR, so its RLGR1 output does not decode (fixed upstream later).
fn rlgr1_encode(input: &[i16], out: &mut Vec<u8>) {
    const KP_MAX: u32 = 80;
    const LS_GR: u32 = 3;
    const UP_GR: u32 = 4;
    const DN_GR: u32 = 6;
    const UQ_GR: u32 = 3;
    const DQ_GR: u32 = 3;

    let mut bits = BitWriter { out, acc: 0, n: 0 };
    let code_gr = |bits: &mut BitWriter<'_>, krp: &mut u32, val: u32| {
        let kr = *krp >> LS_GR;
        let vk = val >> kr;
        bits.ones(vk);
        bits.put(1, 0);
        bits.put(kr, val & ((1 << kr) - 1));
        if vk == 0 {
            *krp = krp.saturating_sub(2);
        } else if vk > 1 {
            *krp = (*krp + vk).min(KP_MAX);
        }
    };
    let (mut k, mut kp, mut krp) = (1u32, 1u32 << LS_GR, 1u32 << LS_GR);
    let mut input = input.iter().copied().peekable();
    while let Some(&first) = input.peek() {
        if k == 0 {
            // Golomb-Rice mode: 2 * magnitude - sign.
            input.next();
            let two_ms = 2 * u32::from(first.unsigned_abs()) - u32::from(first < 0);
            code_gr(&mut bits, &mut krp, two_ms);
            kp = if two_ms == 0 { (kp + UQ_GR).min(KP_MAX) } else { kp.saturating_sub(DQ_GR) };
        } else {
            // Run-length mode: a run of zeros, then the next value's sign and magnitude - 1.
            let mut zeros = 0u32;
            while input.next_if_eq(&0).is_some() {
                zeros += 1;
            }
            while zeros >= 1 << k {
                bits.put(1, 0);
                zeros -= 1 << k;
                kp = (kp + UP_GR).min(KP_MAX);
                k = kp >> LS_GR;
            }
            bits.put(1, 1);
            bits.put(k, zeros);
            if let Some(val) = input.next() {
                bits.put(1, u32::from(val < 0));
                code_gr(&mut bits, &mut krp, u32::from(val.unsigned_abs()) - 1);
            }
            kp = kp.saturating_sub(DN_GR);
        }
        k = kp >> LS_GR;
    }
    bits.flush();
}

/// MSB-first bit writer.
struct BitWriter<'a> {
    out: &'a mut Vec<u8>,
    acc: u64,
    n: u32,
}

impl BitWriter<'_> {
    /// The low `n` (at most 32) bits of `val`.
    fn put(&mut self, n: u32, val: u32) {
        self.acc = (self.acc << n) | (u64::from(val) & ((1 << n) - 1));
        self.n += n;
        while self.n >= 8 {
            self.n -= 8;
            self.out.push((self.acc >> self.n).to_le_bytes()[0]);
        }
    }

    fn ones(&mut self, mut count: u32) {
        while count > 0 {
            let n = count.min(32);
            self.put(n, u32::MAX);
            count -= n;
        }
    }

    /// Pad the last byte with zero bits.
    fn flush(&mut self) {
        if self.n > 0 {
            self.put(8 - self.n, 0);
        }
    }
}

#[cfg(feature = "__bench")]
#[expect(clippy::missing_panics_doc, reason = "panics in benches are allowed")]
pub(crate) mod bench {
    use super::*;

    pub fn rfx_enc_tile(
        bitmap: &BitmapUpdate,
        quant: &Quant,
        algo: rfx::EntropyAlgorithm,
        tile_x: usize,
        tile_y: usize,
    ) {
        let (enc, mut data) = UpdateEncoder::new(bitmap, quant.clone(), algo);

        enc.encode_tile(tile_x, tile_y, &mut data.0)
            .expect("cannot propagate error in benchmark");
    }

    pub fn rfx_enc(bitmap: &BitmapUpdate, quant: &Quant, algo: rfx::EntropyAlgorithm) {
        let (enc, mut data) = UpdateEncoder::new(bitmap, quant.clone(), algo);

        enc.encode(&mut data).expect("cannot propagate error in benchmark");
    }
}

#[cfg(test)]
mod tests {
    use ironrdp_graphics::color_conversion::{ycbcr_to_argb, YCbCrBuffer};
    use ironrdp_graphics::rlgr;

    use super::*;

    fn rect(x: u16, y: u16, width: u16, height: u16) -> RfxRectangle {
        RfxRectangle { x, y, width, height }
    }

    /// BGRA test surface: colour gradient with dark one-pixel strokes (text-like edges).
    fn surface(w: usize, h: usize) -> Vec<u8> {
        let mut px = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 4;
                let stroke = (x % 9 == 3 && y % 16 < 11) || (y % 16 == 5 && x % 9 < 6);
                let (b, g, r) = if stroke {
                    (20, 20, 20)
                } else {
                    ((x * 255 / w) as u8, (y * 255 / h) as u8, 200)
                };
                px[i..i + 4].copy_from_slice(&[b, g, r, 0xFF]);
            }
        }
        px
    }

    fn u16_at(b: &[u8], i: usize) -> u16 {
        u16::from_le_bytes([b[i], b[i + 1]])
    }

    fn u32_at(b: &[u8], i: usize) -> u32 {
        u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
    }

    /// (blockType, whole block) for each block of a progressive message.
    fn blocks(data: &[u8]) -> Vec<(u16, &[u8])> {
        let mut out = Vec::new();
        let mut rest = data;
        while !rest.is_empty() {
            let len = u32_at(rest, 2) as usize;
            assert!(len >= 6 && len <= rest.len(), "block length {len}");
            out.push((u16_at(rest, 0), &rest[..len]));
            rest = &rest[len..];
        }
        out
    }

    struct Tile<'a> {
        x: u16,
        y: u16,
        quant_idx: [u8; 3],
        flags: u8,
        data: [&'a [u8]; 3],
    }

    struct Region<'a> {
        flags: u8,
        num_prog_quant: u8,
        rects: Vec<RfxRectangle>,
        quants: Vec<&'a [u8]>,
        tiles: Vec<Tile<'a>>,
    }

    /// Parse an RFX_PROGRESSIVE_REGION block (MS-RDPEGFX 2.2.4.2.1.5), checking its lengths.
    fn region(block: &[u8]) -> Region<'_> {
        assert_eq!(u16_at(block, 0), 0xCCC4, "REGION");
        assert_eq!(u32_at(block, 2) as usize, block.len());
        assert_eq!(block[6], 64, "tileSize");
        let num_rects = usize::from(u16_at(block, 7));
        let num_quant = usize::from(block[9]);
        let num_prog_quant = block[10];
        let flags = block[11];
        let num_tiles = usize::from(u16_at(block, 12));
        let tile_data_size = u32_at(block, 14) as usize;
        let rects = (0..num_rects)
            .map(|i| {
                let o = 18 + i * 8;
                rect(u16_at(block, o), u16_at(block, o + 2), u16_at(block, o + 4), u16_at(block, o + 6))
            })
            .collect();
        let q0 = 18 + num_rects * 8;
        let quants = (0..num_quant).map(|i| &block[q0 + i * 5..q0 + i * 5 + 5]).collect();
        let tile_bytes = &block[q0 + num_quant * 5..];
        assert_eq!(tile_bytes.len(), tile_data_size, "tileDataSize");
        let tiles: Vec<Tile<'_>> = blocks(tile_bytes)
            .into_iter()
            .map(|(ty, t)| {
                assert_eq!(ty, 0xCCC5, "TILE_SIMPLE");
                let lens = [u16_at(t, 14), u16_at(t, 16), u16_at(t, 18)].map(usize::from);
                assert_eq!(u16_at(t, 20), 0, "tailLen");
                assert_eq!(t.len(), 22 + lens.iter().sum::<usize>(), "tile blockLen");
                let (y, rest) = t[22..].split_at(lens[0]);
                let (cb, cr) = rest.split_at(lens[1]);
                Tile {
                    x: u16_at(t, 9),
                    y: u16_at(t, 11),
                    quant_idx: [t[6], t[7], t[8]],
                    flags: t[13],
                    data: [y, cb, cr],
                }
            })
            .collect();
        assert_eq!(tiles.len(), num_tiles, "numTiles");
        Region {
            flags,
            num_prog_quant,
            rects,
            quants,
            tiles,
        }
    }

    /// Classic RemoteFX component decode (original DWT): what a progressive decoder does for a
    /// TILE_SIMPLE when the region does not set RFX_DWT_REDUCE_EXTRAPOLATE.
    fn decode_component(data: &[u8], quant: &Quant) -> Vec<i16> {
        let mut buf = vec![0i16; 4096];
        rlgr::decode(rfx::EntropyAlgorithm::Rlgr1, data, &mut buf).expect("rlgr");
        subband_reconstruction::decode(&mut buf[4032..]);
        quantization::decode(&mut buf, quant);
        let mut temp = vec![0i16; 4096];
        dwt::decode(&mut buf, &mut temp);
        buf
    }

    /// Client side of a progressive REGION: paint the decoded tiles, clipped to the region
    /// rects, onto a BGRA `w`-wide surface. Tiles and rects are in surface coordinates.
    fn paint(update: &GfxRemoteFxUpdate, out: &mut [u8], w: usize) {
        let reg = region(&update.data);
        // The quant the region carries, unpacked from its RFX_COMPONENT_CODEC_QUANT nibbles.
        let q = reg.quants[0];
        let quant = Quant {
            ll3: q[0] & 15,
            hl3: q[0] >> 4,
            lh3: q[1] & 15,
            hh3: q[1] >> 4,
            hl2: q[2] & 15,
            lh2: q[2] >> 4,
            hh2: q[3] & 15,
            hl1: q[3] >> 4,
            lh1: q[4] & 15,
            hh1: q[4] >> 4,
        };
        for tile in &reg.tiles {
            let [y, cb, cr] = tile.data.map(|d| decode_component(d, &quant));
            let mut argb = vec![0u8; 4096 * 4];
            ycbcr_to_argb(YCbCrBuffer { y: &y, cb: &cb, cr: &cr }, &mut argb).expect("ycbcr");
            for r in &reg.rects {
                for py in 0..64 {
                    for px in 0..64 {
                        let (sx, sy) = (usize::from(tile.x) * 64 + px, usize::from(tile.y) * 64 + py);
                        let inside = sx >= usize::from(r.x)
                            && sx < usize::from(r.x + r.width)
                            && sy >= usize::from(r.y)
                            && sy < usize::from(r.y + r.height);
                        if inside {
                            let s = (py * 64 + px) * 4;
                            let d = (sy * w + sx) * 4;
                            out[d..d + 4].copy_from_slice(&[argb[s + 3], argb[s + 2], argb[s + 1], 0xFF]);
                        }
                    }
                }
            }
        }
    }

    fn psnr(a: &[u8], b: &[u8]) -> f64 {
        let (mut se, mut n) = (0f64, 0f64);
        for (pa, pb) in a.chunks_exact(4).zip(b.chunks_exact(4)) {
            for c in 0..3 {
                let d = f64::from(pa[c]) - f64::from(pb[c]);
                se += d * d;
                n += 1.0;
            }
        }
        10.0 * (255.0f64 * 255.0 / (se / n).max(1e-9)).log10()
    }

    /// Dark one-pixel strokes on white, like UI text.
    fn text(w: usize, h: usize) -> Vec<u8> {
        surface(w, h)
            .chunks_exact(4)
            .flat_map(|p| if p[0] == 20 { [30, 30, 30, 255] } else { [255, 255, 255, 255] })
            .collect()
    }

    #[test]
    fn rfx_quality_trades_bytes_for_fidelity() {
        let (w, h) = (200usize, 150usize);
        let src = text(w, h);
        let measured = [RfxQuality::Low, RfxQuality::Medium, RfxQuality::High].map(|q| {
            let update = encode_gfx_progressive(&src, w * 4, 200, 150, &[rect(0, 0, 200, 150)], q).expect("encode");
            let mut out = vec![0u8; w * h * 4];
            paint(&update, &mut out, w);
            (update.data.len(), psnr(&src, &out))
        });
        let [low, medium, high] = measured;
        assert!(low.0 < medium.0 && medium.0 < high.0, "bytes {measured:?}");
        assert!(low.1 < medium.1 && medium.1 < high.1, "PSNR {measured:?}");
    }

    #[test]
    fn rfx_quality_reads_config_values() {
        assert_eq!(RfxQuality::from_config(Some("low")), RfxQuality::Low);
        assert_eq!(RfxQuality::from_config(Some("high")), RfxQuality::High);
        for medium in [Some("medium"), None, Some("best")] {
            assert_eq!(RfxQuality::from_config(medium), RfxQuality::Medium);
        }
    }

    #[test]
    fn gfx_progressive_round_trips_with_good_quality() {
        let (w, h) = (200usize, 150usize);
        // Measured at GFX_RFX_QUANT: text 37 dB, saturated colour strokes 28 dB. A broken
        // transform, quantizer or tile placement lands far below.
        for (src, min_db) in [(text(w, h), 35.0), (surface(w, h), 26.0)] {
            let update = encode_gfx_progressive(&src, w * 4, 200, 150, &[rect(0, 0, 200, 150)], RfxQuality::Medium).expect("encode");
            assert_eq!(update.tiles, 4 * 3);
            let mut out = vec![0u8; w * h * 4];
            paint(&update, &mut out, w);
            let q = psnr(&src, &out);
            assert!(q > min_db, "PSNR {q:.1} dB");
        }
    }

    #[test]
    fn gfx_progressive_full_frame_uses_workers_and_keeps_tile_order() {
        let (w, h) = (816usize, 1544usize);
        let src = text(w, h);
        let update = encode_gfx_progressive(&src, w * 4, 816, 1544, &[rect(0, 0, 816, 1544)], RfxQuality::Medium).expect("encode");
        assert!(update.tiles >= RFX_PARALLEL_MIN_TILES);
        assert_eq!(update.tiles, 13 * 25);
        let reg = region(&update.data);
        let order: Vec<(u16, u16)> = reg.tiles.iter().map(|t| (t.y, t.x)).collect();
        let mut sorted = order.clone();
        sorted.sort_unstable();
        assert_eq!(order, sorted);
        let mut out = vec![0u8; w * h * 4];
        paint(&update, &mut out, w);
        let q = psnr(&src, &out);
        assert!(q > 35.0, "PSNR {q:.1} dB");
    }

    #[test]
    fn gfx_progressive_region_is_single_pass_simple_tiles() {
        let src = surface(130, 70);
        let update = encode_gfx_progressive(&src, 130 * 4, 130, 70, &[rect(0, 0, 130, 70)], RfxQuality::Medium).expect("encode");
        let reg = region(&update.data);
        // Original DWT (no RFX_DWT_REDUCE_EXTRAPOLATE), no progressive quality passes.
        assert_eq!(reg.flags, 0);
        assert_eq!(reg.num_prog_quant, 0);
        // RFX_COMPONENT_CODEC_QUANT nibbles: LL3|HL3, LH3|HH3, HL2|LH2, HH2|HL1, LH1|HH1.
        assert_eq!(reg.quants, vec![&[0x66u8, 0x66, 0x77, 0x88, 0x98][..]]);
        assert_eq!(reg.rects, vec![rect(0, 0, 130, 70)]);
        assert_eq!(reg.tiles.len(), 3 * 2);
        for t in &reg.tiles {
            assert_eq!((t.quant_idx, t.flags), ([0, 0, 0], 0));
        }
    }

    #[test]
    fn gfx_progressive_encodes_only_dirty_tiles_in_surface_coordinates() {
        let (w, h) = (816usize, 1544usize);
        let src = surface(w, h);
        let enc = |r: &[RfxRectangle]| encode_gfx_progressive(&src, w * 4, 816, 1544, r, RfxQuality::Medium).expect("encode");

        // Inside tile (1, 1): one tile at its surface grid index, the rect as sent.
        let one = enc(&[rect(100, 100, 10, 10)]);
        assert_eq!(one.tiles, 1);
        let reg = region(&one.data);
        assert_eq!((reg.tiles[0].x, reg.tiles[0].y), (1, 1));
        assert_eq!(reg.rects, vec![rect(100, 100, 10, 10)]);

        // Across a tile corner: four tiles.
        assert_eq!(enc(&[rect(60, 60, 10, 10)]).tiles, 4);

        // Bottom-right partial tile; the rect is clipped to the surface.
        let edge = enc(&[rect(800, 1540, 100, 100)]);
        assert_eq!(edge.tiles, 1);
        let reg = region(&edge.data);
        assert_eq!((reg.tiles[0].x, reg.tiles[0].y), (12, 24));
        assert_eq!(reg.rects, vec![rect(800, 1540, 16, 4)]);

        // Overlapping rects share tiles.
        assert_eq!(enc(&[rect(0, 0, 64, 64), rect(10, 10, 20, 20)]).tiles, 1);

        // Painted result matches the source inside the dirty rect, and nothing else is painted.
        let src = text(w, h);
        let mut out = vec![0u8; w * h * 4];
        let one = encode_gfx_progressive(&src, w * 4, 816, 1544, &[rect(100, 100, 10, 10)], RfxQuality::Medium).expect("encode");
        paint(&one, &mut out, w);
        let pick = |buf: &[u8]| -> Vec<u8> {
            (100..110).flat_map(|y| buf[(y * w + 100) * 4..(y * w + 110) * 4].to_vec()).collect()
        };
        assert!(psnr(&pick(&src), &pick(&out)) > 35.0);
        assert!(out[..(100 * w) * 4].iter().all(|&b| b == 0));
    }

    #[test]
    fn gfx_progressive_rejects_short_buffer_and_empty_region() {
        let src = vec![0u8; 64 * 4 * 10];
        assert!(encode_gfx_progressive(&src, 64 * 4, 64, 64, &[rect(0, 0, 64, 64)], RfxQuality::Medium).is_err());
        let src = surface(64, 64);
        assert!(encode_gfx_progressive(&src, 64 * 4, 64, 64, &[rect(64, 0, 8, 8)], RfxQuality::Medium).is_err());
        assert!(encode_gfx_progressive(&src, 64 * 4, 64, 64, &[], RfxQuality::Medium).is_err());
    }

    #[test]
    fn worst_case_tiles_fit_the_component_buffer() {
        let mut seed = 0x2545_F491_u32;
        let mut noise = vec![0u8; 64 * 64 * 4];
        for b in &mut noise {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            *b = seed.to_le_bytes()[0];
        }
        let checker: Vec<u8> = (0..64 * 64)
            .flat_map(|i| if (i % 64 + i / 64) % 2 == 0 { [0, 0, 0, 255] } else { [255, 255, 255, 255] })
            .collect();
        for px in [noise, checker] {
            let update = encode_gfx_progressive(&px, 64 * 4, 64, 64, &[rect(0, 0, 64, 64)], RfxQuality::Medium).expect("encode");
            for d in region(&update.data).tiles[0].data {
                // Lengths are u16 on the wire.
                assert!(d.len() < 8 * 1024, "component {} bytes", d.len());
            }
        }
    }

    #[test]
    fn rlgr1_round_trips_through_the_decoder() {
        // Zero runs, small and large magnitudes: run-length and Golomb-Rice modes both.
        let mut seed = 0x1234_5678_u32;
        let input: Vec<i16> = (0..4096)
            .map(|i| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                let v = i16::try_from(seed % 2001).expect("small") - 1000;
                match i % 7 {
                    0..=3 => 0,
                    4 => v % 3,
                    _ => v,
                }
            })
            .collect();
        let mut data = Vec::new();
        rlgr1_encode(&input, &mut data);
        let mut out = vec![0i16; 4096];
        rlgr::decode(rfx::EntropyAlgorithm::Rlgr1, &data, &mut out).expect("rlgr");
        assert_eq!(out, input);
    }

    #[test]
    fn progressive_message_starts_codec_context_once() {
        let src = surface(130, 70);
        let update = encode_gfx_progressive(&src, 130 * 4, 130, 70, &[rect(0, 0, 130, 70)], RfxQuality::Medium).expect("encode");

        let first = progressive_message(&update.data, 7, true);
        let all = blocks(&first);
        let types: Vec<u16> = all.iter().map(|b| b.0).collect();
        assert_eq!(types, [0xCCC0, 0xCCC3, 0xCCC1, 0xCCC4, 0xCCC2]);
        // SYNC: magic, version 1.0.
        assert_eq!(all[0].1[6..], [0xCA, 0xAC, 0xCC, 0xCA, 0x00, 0x01]);
        // CONTEXT: ctxId 0, tileSize 64, flags 0.
        assert_eq!(all[1].1[6..], [0, 64, 0, 0]);
        // FRAME_BEGIN: frameIndex, one region.
        assert_eq!((u32_at(all[2].1, 6), u16_at(all[2].1, 10), all[2].1.len()), (7, 1, 12));
        assert_eq!(all[3].1, &update.data[..]);
        assert_eq!(all[4].1.len(), 6);

        let next = progressive_message(&update.data, 8, false);
        let types: Vec<u16> = blocks(&next).iter().map(|b| b.0).collect();
        assert_eq!(types, [0xCCC1, 0xCCC4, 0xCCC2]);
    }

    /// `cargo test -p ironrdp-server --release --lib progressive_full_frame_timing -- --ignored --nocapture`
    #[test]
    #[ignore = "timing report, run in release"]
    fn progressive_full_frame_timing() {
        let (w, h) = (816usize, 1544usize);
        let full = [rect(0, 0, 816, 1544)];
        for (name, src) in [("colour strokes", surface(w, h)), ("text on white", text(w, h))] {
            let _ = encode_gfx_progressive(&src, w * 4, 816, 1544, &full, RfxQuality::Medium).expect("warm-up");
            let runs = 20u32;
            let t0 = std::time::Instant::now();
            let mut u = None;
            for _ in 0..runs {
                u = Some(encode_gfx_progressive(&src, w * 4, 816, 1544, &full, RfxQuality::Medium).expect("encode"));
            }
            let ms = t0.elapsed().as_secs_f64() * 1000.0 / f64::from(runs);
            let u = u.expect("ran");
            let small = [rect(100, 100, 120, 100)]; // 3x2 tiles, single-threaded path
            let t1 = std::time::Instant::now();
            for _ in 0..runs * 10 {
                let _ = encode_gfx_progressive(&src, w * 4, 816, 1544, &small, RfxQuality::Medium).expect("encode");
            }
            let per_tile = t1.elapsed().as_secs_f64() * 1000.0 / f64::from(runs * 10) / 6.0;
            std::println!(
                "Progressive 816x1544 {name}: full frame {ms:.2} ms ({} tiles, {} bytes); {per_tile:.3} ms/tile on one thread",
                u.tiles,
                u.data.len()
            );
        }
    }
}
