//! Feishu's image-embedding caps (ADR-0076).
//!
//! An image can render in the card only if Feishu's upload API accepts it:
//! a mime in its set, bytes within its size cap, and dimensions within its
//! limits (GIF has its own, smaller limit). Those are platform facts, so the
//! decision lives here — beside the upload — and never in `src/backend/`.
//! A file outside the caps is simply not embeddable; the caller leaves it to
//! the File Message path (ticket #649) rather than failing the card.

use crate::backend::FileContent;

/// The image mimes Feishu's `im/v1/images` upload accepts (tiff/heic are
/// converted to jpg server-side, the rest pass through). Anything else is not
/// an embeddable image.
const EMBEDDABLE_IMAGE_MIMES: [&str; 10] = [
    "image/jpeg",
    "image/jpg",
    "image/png",
    "image/webp",
    "image/gif",
    "image/bmp",
    "image/x-icon",
    "image/vnd.microsoft.icon",
    "image/tiff",
    "image/heic",
];

/// Feishu's image upload size cap: 10MB.
pub(crate) const MAX_IMAGE_BYTES: u64 = 10 * 1024 * 1024;

/// A GIF's dimension cap (Feishu animates GIFs, so it bounds them tighter).
const MAX_GIF_DIMENSION: u32 = 2000;

/// Every other image's dimension cap.
const MAX_IMAGE_DIMENSION: u32 = 12000;

/// Whether `content` is an image Feishu will let cola embed in a card. The
/// inline bytes must be present (a reference-only block never reaches here —
/// [`FileContent::decode`] already dropped it), in the mime set and within
/// Feishu's size and dimension caps. An unparseable dimension is left to
/// Feishu to reject on upload rather than guessed.
pub(crate) fn embeddable_image(content: &FileContent) -> bool {
    if !EMBEDDABLE_IMAGE_MIMES.contains(&content.mime.as_str()) {
        return false;
    }
    if content.size > MAX_IMAGE_BYTES {
        return false;
    }
    let Some(bytes) = content.bytes() else {
        return false;
    };
    let Some((width, height)) = image_dimensions(&bytes, &content.mime) else {
        // Dimensions unknown (a format this build does not parse): let Feishu
        // decide. A rejected upload fails the image alone, never the card.
        return true;
    };
    let cap = if content.mime == "image/gif" {
        MAX_GIF_DIMENSION
    } else {
        MAX_IMAGE_DIMENSION
    };
    width <= cap && height <= cap
}

/// The pixel dimensions of `bytes` for the common formats (PNG, GIF, BMP,
/// JPEG). `None` when the mime is not one this build parses or the bytes are
/// too short/malformed — never a guess.
fn image_dimensions(bytes: &[u8], mime: &str) -> Option<(u32, u32)> {
    match mime {
        "image/png" => png_dimensions(bytes),
        "image/gif" => gif_dimensions(bytes),
        "image/bmp" => bmp_dimensions(bytes),
        "image/jpeg" | "image/jpg" => jpeg_dimensions(bytes),
        _ => None,
    }
}

/// PNG: the 8-byte signature, then the IHDR chunk's big-endian `width`/`height`
/// (bytes 16..24).
fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    if !bytes.starts_with(&SIGNATURE) {
        return None;
    }
    let width = u32::from_be_bytes(bytes.get(16..20)?.try_into().ok()?);
    let height = u32::from_be_bytes(bytes.get(20..24)?.try_into().ok()?);
    Some((width, height))
}

/// GIF: `GIF87a`/`GIF89a`, then little-endian `width`/`height` (bytes 6..10).
fn gif_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if !bytes.starts_with(b"GIF87a") && !bytes.starts_with(b"GIF89a") {
        return None;
    }
    let width = u16::from_le_bytes(bytes.get(6..8)?.try_into().ok()?);
    let height = u16::from_le_bytes(bytes.get(8..10)?.try_into().ok()?);
    Some((u32::from(width), u32::from(height)))
}

/// BMP: `BM`, a BITMAPINFOHEADER or later (header size ≥ 40 at byte 14), then
/// little-endian `width`/`height` (bytes 18..26). A negative height (a
/// top-down bitmap) reads by magnitude.
fn bmp_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if !bytes.starts_with(b"BM") {
        return None;
    }
    let header_size = u32::from_le_bytes(bytes.get(14..18)?.try_into().ok()?);
    if header_size < 40 {
        return None;
    }
    let width = i32::from_le_bytes(bytes.get(18..22)?.try_into().ok()?);
    let height = i32::from_le_bytes(bytes.get(22..26)?.try_into().ok()?);
    Some((width.unsigned_abs(), height.unsigned_abs()))
}

/// JPEG: walk the marker segments from SOI to the first Start-Of-Frame, whose
/// `height`/`width` are the segment's big-endian words at offset 5..9 (after
/// the 2-byte length and the 1-byte sample precision).
fn jpeg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if !bytes.starts_with(&[0xff, 0xd8]) {
        return None;
    }
    let mut i = 2;
    while i + 1 < bytes.len() {
        if bytes[i] != 0xff {
            // Not on a marker boundary: malformed, give up rather than guess.
            return None;
        }
        // Skip fill bytes (`0xff`) before the marker code.
        let mut code = bytes[i + 1];
        let mut marker_at = i + 1;
        while code == 0xff {
            marker_at += 1;
            code = *bytes.get(marker_at)?;
        }
        i = marker_at + 1;
        // Standalone markers (RSTn, SOI, EOI, TEM) carry no length.
        match code {
            0xd8 | 0xd9 | 0x01 | 0xd0..=0xd7 => continue,
            _ => {}
        }
        let length = u16::from_be_bytes(bytes.get(i..i + 2)?.try_into().ok()?) as usize;
        if length < 2 {
            return None;
        }
        // Start-Of-Frame markers (excluding DHT 0xc4, JPG 0xc8, DAC 0xcc).
        let is_sof = matches!(code, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf);
        if is_sof {
            let height = u16::from_be_bytes(bytes.get(i + 3..i + 5)?.try_into().ok()?);
            let width = u16::from_be_bytes(bytes.get(i + 5..i + 7)?.try_into().ok()?);
            return Some((u32::from(width), u32::from(height)));
        }
        i += length;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A File Content from raw bytes and a mime, as the render path decodes it.
    fn content(bytes: &[u8], mime: &str) -> FileContent {
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        FileContent::decode(&format!("data:{mime};base64,{encoded}"), Some(mime), Some("a")).unwrap()
    }

    /// A PNG `data:` URI whose IHDR declares `width`×`height`. The header is
    /// real (the parser must read the signature and the chunk), the rest is
    /// filler.
    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        bytes.extend_from_slice(&13u32.to_be_bytes());
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
        bytes
    }

    fn gif(width: u16, height: u16) -> Vec<u8> {
        let mut bytes = b"GIF89a".to_vec();
        bytes.extend_from_slice(&width.to_le_bytes());
        bytes.extend_from_slice(&height.to_le_bytes());
        bytes
    }

    /// A BMP `BM` file header (14 bytes) plus a BITMAPINFOHEADER declaring
    /// `width`×`height` (a negative height is a top-down bitmap, read by
    /// magnitude). The header is real (the parser must read the signature and
    /// the DIB header), the pixel data is absent — the parser never reads it.
    fn bmp(width: i32, height: i32) -> Vec<u8> {
        let mut bytes = b"BM".to_vec();
        bytes.extend_from_slice(&0u32.to_le_bytes()); // file size (unread)
        bytes.extend_from_slice(&0u16.to_le_bytes()); // reserved
        bytes.extend_from_slice(&0u16.to_le_bytes()); // reserved
        bytes.extend_from_slice(&54u32.to_le_bytes()); // pixel-data offset (unread)
        bytes.extend_from_slice(&40u32.to_le_bytes()); // BITMAPINFOHEADER size
        bytes.extend_from_slice(&width.to_le_bytes());
        bytes.extend_from_slice(&height.to_le_bytes());
        bytes
    }

    #[test]
    fn a_mime_outside_feishus_set_is_not_embeddable() {
        assert!(!embeddable_image(&content(b"%PDF-1.7", "application/pdf")));
        assert!(!embeddable_image(&content(b"hello", "text/plain")));
        assert!(embeddable_image(&content(&png(10, 10), "image/png")));
    }

    #[test]
    fn an_image_past_the_size_cap_is_not_embeddable() {
        // One byte over 10MB, in the mime set — rejected on size alone.
        let mut big = png(10, 10);
        big.resize(MAX_IMAGE_BYTES as usize + 1, 0);
        assert!(!embeddable_image(&content(&big, "image/png")));
        // Exactly at the cap stays embeddable.
        let mut at_cap = png(10, 10);
        at_cap.resize(MAX_IMAGE_BYTES as usize, 0);
        assert!(embeddable_image(&content(&at_cap, "image/png")));
    }

    #[test]
    fn a_gif_past_its_smaller_dimension_cap_is_not_embeddable() {
        assert!(embeddable_image(&content(&gif(2000, 2000), "image/gif")));
        assert!(!embeddable_image(&content(&gif(2001, 100), "image/gif")));
        assert!(!embeddable_image(&content(&gif(100, 2001), "image/gif")));
    }

    #[test]
    fn a_non_gif_past_its_dimension_cap_is_not_embeddable() {
        assert!(embeddable_image(&content(&png(12000, 12000), "image/png")));
        assert!(!embeddable_image(&content(&png(12001, 10), "image/png")));
        // GIF's own cap is stricter, but a 3000-wide PNG is fine.
        assert!(embeddable_image(&content(&png(3000, 3000), "image/png")));
    }

    #[test]
    fn dimensions_are_read_for_the_common_formats() {
        assert_eq!(png_dimensions(&png(3, 4)), Some((3, 4)));
        assert_eq!(gif_dimensions(&gif(5, 6)), Some((5, 6)));
        // A JPEG with an SOI, an APP0 segment, then an SOF0 (precision 8,
        // height 7, width 9).
        let jpeg = [
            0xff, 0xd8, // SOI
            0xff, 0xe0, 0x00, 0x04, 0x00, 0x00, // APP0, length 4
            0xff, 0xc0, 0x00, 0x11, 0x08, 0x00, 0x07, 0x00, 0x09, 0x03, // SOF0
            0xff, 0xd9, // EOI
        ];
        assert_eq!(jpeg_dimensions(&jpeg), Some((9, 7)));
        // BMP is read through the mime dispatch (`image/bmp`), so dropping that
        // arm — not just the parser — fails here. A top-down bitmap's negative
        // height reads by magnitude.
        assert_eq!(image_dimensions(&bmp(11, 12), "image/bmp"), Some((11, 12)));
        assert_eq!(image_dimensions(&bmp(11, -12), "image/bmp"), Some((11, 12)));
        // Truncated/malformed input yields no dimensions, never a panic.
        assert_eq!(png_dimensions(&[0x89, b'P']), None);
        assert_eq!(gif_dimensions(b"GIF89a"), None);
        assert_eq!(bmp_dimensions(b"BM"), None);
        assert_eq!(jpeg_dimensions(&[0xff, 0xd8, 0xff]), None);
        assert_eq!(image_dimensions(b"x", "image/webp"), None);
    }
}
