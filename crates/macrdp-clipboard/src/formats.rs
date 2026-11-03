// Format conversion implemented in Task 6

use image::{RgbaImage, ImageFormat};
use std::io::Cursor;

pub const BITMAPINFOHEADER_SIZE: usize = 40;

/// Registered format ID for Windows "HTML Format"
pub const FORMAT_ID_HTML: u32 = 0xD010;

/// Registered format ID for FileGroupDescriptorW
pub const FORMAT_ID_FILE_LIST: u32 = 0xD011;

/// Convert PNG/TIFF image bytes to Windows CF_DIB format.
/// DIB = BITMAPINFOHEADER (40 bytes) + BGRA pixel data (bottom-up row order).
pub fn png_to_dib(image_data: &[u8]) -> anyhow::Result<Vec<u8>> {
    let img = image::load_from_memory(image_data)?;
    let rgba = img.to_rgba8();
    let (width, height) = rgba.dimensions();

    let row_size = (width * 4) as usize; // BGRA, always 4-byte aligned at 32bpp
    let pixel_data_size = row_size * height as usize;

    let mut dib = Vec::with_capacity(BITMAPINFOHEADER_SIZE + pixel_data_size);

    // BITMAPINFOHEADER (40 bytes)
    dib.extend_from_slice(&40u32.to_le_bytes());           // biSize
    dib.extend_from_slice(&(width as i32).to_le_bytes());  // biWidth
    dib.extend_from_slice(&(height as i32).to_le_bytes()); // biHeight (positive = bottom-up)
    dib.extend_from_slice(&1u16.to_le_bytes());            // biPlanes
    dib.extend_from_slice(&32u16.to_le_bytes());           // biBitCount (BGRA)
    dib.extend_from_slice(&0u32.to_le_bytes());            // biCompression (BI_RGB)
    dib.extend_from_slice(&(pixel_data_size as u32).to_le_bytes()); // biSizeImage
    dib.extend_from_slice(&0i32.to_le_bytes());            // biXPelsPerMeter
    dib.extend_from_slice(&0i32.to_le_bytes());            // biYPelsPerMeter
    dib.extend_from_slice(&0u32.to_le_bytes());            // biClrUsed
    dib.extend_from_slice(&0u32.to_le_bytes());            // biClrImportant

    // Pixel data: RGBA (top-down) → BGRA (bottom-up)
    for y in (0..height).rev() {
        for x in 0..width {
            let pixel = rgba.get_pixel(x, y);
            dib.push(pixel[2]); // B
            dib.push(pixel[1]); // G
            dib.push(pixel[0]); // R
            dib.push(pixel[3]); // A
        }
    }

    Ok(dib)
}

/// Largest DIB edge accepted from a client; bounds the RGBA allocation.
const MAX_DIB_DIMENSION: u32 = 16384;

/// BI_BITFIELDS: three DWORD colour masks follow a 40-byte BITMAPINFOHEADER.
const BI_BITFIELDS: u32 = 3;

/// Convert Windows CF_DIB format to PNG bytes.
///
/// The header is client-supplied: dimensions are bounded and the pixel data must cover them,
/// so a malformed header is rejected instead of driving a huge allocation or loop.
pub fn dib_to_png(dib: &[u8]) -> anyhow::Result<Vec<u8>> {
    if dib.len() < BITMAPINFOHEADER_SIZE {
        anyhow::bail!("DIB data too small for BITMAPINFOHEADER");
    }

    let header_size = u32::from_le_bytes([dib[0], dib[1], dib[2], dib[3]]) as usize;
    let width_raw = i32::from_le_bytes([dib[4], dib[5], dib[6], dib[7]]);
    let height_raw = i32::from_le_bytes([dib[8], dib[9], dib[10], dib[11]]);
    let bits_per_pixel = u16::from_le_bytes([dib[14], dib[15]]);
    let compression = u32::from_le_bytes([dib[16], dib[17], dib[18], dib[19]]);

    let width = u32::try_from(width_raw).unwrap_or(0);
    let bottom_up = height_raw > 0;
    let height = height_raw.unsigned_abs();
    if width == 0 || height == 0 || width > MAX_DIB_DIMENSION || height > MAX_DIB_DIMENSION {
        anyhow::bail!("DIB dimensions {width_raw}x{height_raw} out of range");
    }
    if !matches!(bits_per_pixel, 24 | 32) {
        anyhow::bail!("Unsupported DIB bit depth {bits_per_pixel}");
    }
    if header_size < BITMAPINFOHEADER_SIZE {
        anyhow::bail!("DIB header size {header_size} too small");
    }

    let bytes_per_pixel = (bits_per_pixel / 8) as usize;
    let row_stride = (width as usize * bytes_per_pixel).div_ceil(4) * 4;

    let masks = if header_size == BITMAPINFOHEADER_SIZE && compression == BI_BITFIELDS { 12 } else { 0 };
    let pixel_offset = header_size + masks;
    let pixel_data = dib.get(pixel_offset..).unwrap_or_default();
    if pixel_data.len() < row_stride * height as usize {
        anyhow::bail!("DIB pixel data shorter than {width}x{height} at {bits_per_pixel} bpp");
    }

    let mut img = RgbaImage::new(width, height);

    let rows = |y: u32| {
        let src_y = if bottom_up { height - 1 - y } else { y };
        &pixel_data[src_y as usize * row_stride..][..width as usize * bytes_per_pixel]
    };
    // The 4th byte of a 32-bpp DIB is alpha only if anything uses it: Windows leaves the
    // reserved byte of BI_RGB images at 0, which would otherwise paste fully transparent.
    let has_alpha = bits_per_pixel == 32
        && (0..height).any(|y| rows(y).chunks_exact(4).any(|px| px[3] != 0));

    for y in 0..height {
        let row = rows(y);

        for (x, px) in row.chunks_exact(bytes_per_pixel).enumerate() {
            let a = if has_alpha { px[3] } else { 255 };
            img.put_pixel(x as u32, y, image::Rgba([px[2], px[1], px[0], a]));
        }
    }

    let mut png_bytes = Vec::new();
    img.write_to(&mut Cursor::new(&mut png_bytes), ImageFormat::Png)?;
    Ok(png_bytes)
}

/// Convert UTF-8 string to UTF-16LE bytes with null terminator.
pub fn utf8_to_utf16le(s: &str) -> Vec<u8> {
    let utf16: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
    utf16.iter().flat_map(|&w| w.to_le_bytes()).collect()
}

/// Convert UTF-16LE bytes to UTF-8 string, stripping null terminator.
pub fn utf16le_to_utf8(data: &[u8]) -> Option<String> {
    if data.is_empty() {
        return Some(String::new());
    }
    let words: Vec<u16> = data
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16(&words)
        .ok()
        .map(|s| s.trim_end_matches('\0').to_string())
}

/// Map macOS UTI to RDP ClipboardFormat.
/// Returns a tuple of (format_id, optional_name) for now.
/// The exact ClipboardFormat construction depends on ironrdp-cliprdr API.
pub fn uti_to_rdp_format_id(uti: &str) -> Option<u32> {
    match uti {
        "public.utf8-plain-text" | "public.plain-text" => Some(13), // CF_UNICODETEXT
        "public.png" | "public.tiff" | "public.jpeg" => Some(8),    // CF_DIB
        "public.html" => Some(FORMAT_ID_HTML),
        "public.file-url" => Some(FORMAT_ID_FILE_LIST),
        _ => None,
    }
}

/// Map RDP format ID to macOS UTI.
pub fn rdp_format_id_to_uti(format_id: u32) -> Option<&'static str> {
    match format_id {
        13 => Some("public.utf8-plain-text"), // CF_UNICODETEXT
        8 => Some("public.png"),              // CF_DIB
        FORMAT_ID_HTML => Some("public.html"),
        FORMAT_ID_FILE_LIST => Some("public.file-url"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_to_utf16le_ascii() {
        let result = utf8_to_utf16le("hello");
        assert_eq!(result.len(), 12); // 5 chars + null = 6 code units = 12 bytes
        assert_eq!(result[0], 0x68); // 'h'
        assert_eq!(result[1], 0x00);
        assert_eq!(result[10], 0x00); // null terminator
        assert_eq!(result[11], 0x00);
    }

    #[test]
    fn utf8_to_utf16le_chinese() {
        let result = utf8_to_utf16le("你好");
        assert_eq!(result.len(), 6); // 2 chars + null = 3 code units = 6 bytes
        assert_eq!(result[0], 0x60); // '你' = U+4F60 → LE [0x60, 0x4F]
        assert_eq!(result[1], 0x4F);
    }

    #[test]
    fn utf16le_to_utf8_roundtrip() {
        let original = "Hello 世界! 🎉";
        let encoded = utf8_to_utf16le(original);
        let decoded = utf16le_to_utf8(&encoded).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn utf16le_to_utf8_empty() {
        let result = utf16le_to_utf8(&[]);
        assert_eq!(result, Some(String::new()));
    }

    #[test]
    fn uti_to_rdp_text() {
        assert_eq!(uti_to_rdp_format_id("public.utf8-plain-text"), Some(13));
    }

    #[test]
    fn uti_to_rdp_image() {
        assert_eq!(uti_to_rdp_format_id("public.png"), Some(8));
    }

    #[test]
    fn uti_to_rdp_unknown() {
        assert_eq!(uti_to_rdp_format_id("com.apple.custom"), None);
    }

    #[test]
    fn png_to_dib_and_back() {
        let mut img = image::RgbaImage::new(2, 2);
        img.put_pixel(0, 0, image::Rgba([255, 0, 0, 255]));
        img.put_pixel(1, 0, image::Rgba([0, 255, 0, 255]));
        img.put_pixel(0, 1, image::Rgba([0, 0, 255, 255]));
        img.put_pixel(1, 1, image::Rgba([255, 255, 255, 255]));

        let mut png_bytes = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut png_bytes), image::ImageFormat::Png).unwrap();

        let dib = png_to_dib(&png_bytes).unwrap();
        assert!(dib.len() > 40);

        let width = i32::from_le_bytes([dib[4], dib[5], dib[6], dib[7]]);
        let height = i32::from_le_bytes([dib[8], dib[9], dib[10], dib[11]]);
        assert_eq!(width, 2);
        assert!(height > 0); // Positive = bottom-up

        let png_back = dib_to_png(&dib).unwrap();
        assert!(!png_back.is_empty());

        // Verify roundtrip preserves pixels
        let img_back = image::load_from_memory(&png_back).unwrap().to_rgba8();
        assert_eq!(img_back.get_pixel(0, 0), &image::Rgba([255, 0, 0, 255]));
        assert_eq!(img_back.get_pixel(1, 1), &image::Rgba([255, 255, 255, 255]));
    }

    #[test]
    fn dib_too_small() {
        let result = dib_to_png(&[0u8; 10]);
        assert!(result.is_err());
    }

    #[test]
    fn dib_with_zero_reserved_bytes_is_opaque() {
        let mut dib = vec![0u8; BITMAPINFOHEADER_SIZE];
        dib[0..4].copy_from_slice(&40u32.to_le_bytes());
        dib[4..8].copy_from_slice(&1i32.to_le_bytes());
        dib[8..12].copy_from_slice(&1i32.to_le_bytes());
        dib[14..16].copy_from_slice(&32u16.to_le_bytes());
        dib.extend_from_slice(&[0x10, 0x20, 0x30, 0x00]); // B, G, R, reserved = 0
        let png = dib_to_png(&dib).unwrap();
        let img = image::load_from_memory(&png).unwrap().to_rgba8();
        assert_eq!(img.get_pixel(0, 0), &image::Rgba([0x30, 0x20, 0x10, 0xFF]));
    }

    #[test]
    fn dib_hostile_header_rejected() {
        let mut dib = vec![0u8; BITMAPINFOHEADER_SIZE];
        dib[0..4].copy_from_slice(&40u32.to_le_bytes());
        dib[14..16].copy_from_slice(&32u16.to_le_bytes());
        // Negative width.
        dib[4..8].copy_from_slice(&(-1i32).to_le_bytes());
        dib[8..12].copy_from_slice(&i32::MAX.to_le_bytes());
        assert!(dib_to_png(&dib).is_err());
        // Plausible dimensions but no pixel data.
        dib[4..8].copy_from_slice(&4000i32.to_le_bytes());
        dib[8..12].copy_from_slice(&4000i32.to_le_bytes());
        assert!(dib_to_png(&dib).is_err());
    }

    #[test]
    fn uti_to_rdp_html() {
        assert_eq!(uti_to_rdp_format_id("public.html"), Some(FORMAT_ID_HTML));
    }

    #[test]
    fn uti_to_rdp_file_url() {
        assert_eq!(uti_to_rdp_format_id("public.file-url"), Some(FORMAT_ID_FILE_LIST));
    }

    #[test]
    fn rdp_to_uti_html() {
        assert_eq!(rdp_format_id_to_uti(FORMAT_ID_HTML), Some("public.html"));
    }
}
