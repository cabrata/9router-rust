//! Port of open-sse/executors/chatgpt-web/imageSniff.js
//!
//! Compact image format + dimension sniffing (PNG / JPEG / GIF / WebP) for
//! chatgpt-web attachment validation.

pub const MAX_IMAGE_DECODE_EDGE: u32 = 11264;
pub const MAX_IMAGE_PIXELS: u32 = 40 * 1000 * 1000;

/// PNG signature: 89 50 4E 47 (PNG in hex after 0x89)
const PNG_SIGNATURE: u32 = 0x89504e47;
/// JPEG start marker
const JPEG_MARKER_START: u8 = 0xff;
const JPEG_MARKER_SOI: u8 = 0xd8;
/// GIF signature
const GIF_SIGNATURE: &[u8] = b"GIF";
/// RIFF signature for WebP
const RIFF_SIGNATURE: &[u8] = b"RIFF";
const WEBP_SIGNATURE: &[u8] = b"WEBP";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    Png,
    Jpeg,
    Gif,
    Webp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageDimensions {
    pub width: u32,
    pub height: u32,
}

fn png_dimensions(bytes: &[u8]) -> Option<ImageDimensions> {
    if bytes.len() < 24 {
        return None;
    }
    // Check PNG signature
    if u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) != PNG_SIGNATURE {
        return None;
    }
    let width = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let height = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    Some(ImageDimensions { width, height })
}

fn gif_dimensions(bytes: &[u8]) -> Option<ImageDimensions> {
    if bytes.len() < 10 {
        return None;
    }
    if &bytes[0..3] != GIF_SIGNATURE {
        return None;
    }
    let width = u16::from_le_bytes([bytes[6], bytes[7]]) as u32;
    let height = u16::from_le_bytes([bytes[8], bytes[9]]) as u32;
    Some(ImageDimensions { width, height })
}

fn webp_dimensions(bytes: &[u8]) -> Option<ImageDimensions> {
    if bytes.len() < 30 {
        return None;
    }
    if &bytes[0..4] != RIFF_SIGNATURE || &bytes[8..12] != WEBP_SIGNATURE {
        return None;
    }

    let chunk = &bytes[12..16];
    if chunk == b"VP8X" {
        if bytes.len() < 30 {
            return None;
        }
        let width_bits = bytes[24] as u32 | ((bytes[25] as u32) << 8) | ((bytes[26] as u32) << 16);
        let height_bits = bytes[27] as u32 | ((bytes[28] as u32) << 8) | ((bytes[29] as u32) << 16);
        let width = 1 + (width_bits & 0xffffff);
        let height = 1 + (height_bits & 0xffffff);
        return Some(ImageDimensions { width, height });
    }

    if chunk == b"VP8 " {
        if bytes.len() < 30 {
            return None;
        }
        let width = (u16::from_le_bytes([bytes[26], bytes[27]]) & 0x3fff) as u32;
        let height = (u16::from_le_bytes([bytes[28], bytes[29]]) & 0x3fff) as u32;
        return Some(ImageDimensions { width, height });
    }

    if chunk == b"VP8L" {
        if bytes.len() < 25 {
            return None;
        }
        let b = u32::from_le_bytes([bytes[21], bytes[22], bytes[23], bytes[24]]);
        let width = (b & 0x3fff) + 1;
        let height = ((b >> 14) & 0x3fff) + 1;
        return Some(ImageDimensions { width, height });
    }

    None
}

fn jpeg_dimensions(bytes: &[u8]) -> Option<ImageDimensions> {
    if bytes.len() < 4 || bytes[0] != 0xff || bytes[1] != 0xd8 {
        return None;
    }
    let mut offset = 2;
    while offset + 9 < bytes.len() {
        if bytes[offset] != 0xff {
            offset += 1;
            continue;
        }
        let marker = bytes[offset + 1];
        // SOF0..SOF15 except DHT, JPG, DHT, DNL variants
        let is_sof = (marker >= 0xc0 && marker <= 0xc3)
            || (marker >= 0xc5 && marker <= 0xc7)
            || (marker >= 0xc9 && marker <= 0xcb)
            || (marker >= 0xcd && marker <= 0xcf);
        if is_sof {
            let height = u16::from_be_bytes([bytes[offset + 5], bytes[offset + 6]]) as u32;
            let width = u16::from_be_bytes([bytes[offset + 7], bytes[offset + 8]]) as u32;
            return Some(ImageDimensions { width, height });
        }
        if marker == 0xd8 || marker == 0x01 || (marker >= 0xd0 && marker <= 0xd7) {
            offset += 2;
            continue;
        }
        let segment_len = u16::from_be_bytes([bytes[offset + 2], bytes[offset + 3]]) as usize;
        offset += 2 + segment_len;
    }
    None
}

/// Sniff the image format or None.
pub fn sniff_image_format(bytes: &[u8]) -> Option<ImageFormat> {
    if bytes.len() >= 8 && u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) == PNG_SIGNATURE {
        return Some(ImageFormat::Png);
    }
    if bytes.len() >= 3 && bytes[0] == 0xff && bytes[1] == 0xd8 {
        return Some(ImageFormat::Jpeg);
    }
    if bytes.len() >= 3 && &bytes[0..3] == GIF_SIGNATURE {
        return Some(ImageFormat::Gif);
    }
    if bytes.len() >= 12 && &bytes[0..4] == RIFF_SIGNATURE && &bytes[8..12] == WEBP_SIGNATURE {
        return Some(ImageFormat::Webp);
    }
    None
}

/// Sniff pixel dimensions or None when undecodable/unsupported.
pub fn sniff_image_dimensions(bytes: &[u8]) -> Option<ImageDimensions> {
    png_dimensions(bytes)
        .or_else(|| jpeg_dimensions(bytes))
        .or_else(|| gif_dimensions(bytes))
        .or_else(|| webp_dimensions(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_png_detection() {
        // Minimal valid PNG: 89 50 4E 47 ... (width at offset 16-19, height at 20-23)
        let mut png_bytes = vec![0u8; 24];
        png_bytes[0] = 0x89;
        png_bytes[1] = 0x50;
        png_bytes[2] = 0x4e;
        png_bytes[3] = 0x47;
        // Width: 100 (0x64)
        png_bytes[16] = 0x00;
        png_bytes[17] = 0x00;
        png_bytes[18] = 0x00;
        png_bytes[19] = 0x64;
        // Height: 200 (0xc8)
        png_bytes[20] = 0x00;
        png_bytes[21] = 0x00;
        png_bytes[22] = 0x00;
        png_bytes[23] = 0xc8;

        assert_eq!(sniff_image_format(&png_bytes), Some(ImageFormat::Png));
        assert_eq!(
            sniff_image_dimensions(&png_bytes),
            Some(ImageDimensions {
                width: 100,
                height: 200
            })
        );
    }

    #[test]
    fn test_jpeg_detection() {
        // JPEG starts with FF D8
        let jpeg_bytes = [0xffu8, 0xd8, 0xff];
        assert_eq!(sniff_image_format(&jpeg_bytes), Some(ImageFormat::Jpeg));
    }

    #[test]
    fn test_gif_detection() {
        let mut gif_bytes = vec![0u8; 10];
        gif_bytes[0] = b'G';
        gif_bytes[1] = b'I';
        gif_bytes[2] = b'F';
        // Width at offset 6-7 (little-endian)
        gif_bytes[6] = 0x50; // 80
        gif_bytes[7] = 0x00;
        // Height at offset 8-9 (little-endian)
        gif_bytes[8] = 0x60; // 96
        gif_bytes[9] = 0x00;

        assert_eq!(sniff_image_format(&gif_bytes), Some(ImageFormat::Gif));
        assert_eq!(
            sniff_image_dimensions(&gif_bytes),
            Some(ImageDimensions {
                width: 80,
                height: 96
            })
        );
    }

    #[test]
    fn test_webp_detection() {
        let mut webp_bytes = vec![0u8; 30];
        // RIFF signature
        webp_bytes[0] = b'R';
        webp_bytes[1] = b'I';
        webp_bytes[2] = b'F';
        webp_bytes[3] = b'F';
        // WEBP signature at offset 8
        webp_bytes[8] = b'W';
        webp_bytes[9] = b'E';
        webp_bytes[10] = b'B';
        webp_bytes[11] = b'P';
        // VP8X chunk
        webp_bytes[12] = b'V';
        webp_bytes[13] = b'P';
        webp_bytes[14] = b'8';
        webp_bytes[15] = b'X';

        assert_eq!(sniff_image_format(&webp_bytes), Some(ImageFormat::Webp));
    }

    #[test]
    fn test_too_short() {
        assert_eq!(sniff_image_format(&[0xff, 0xd8]), None); // too short for JPEG dims
        assert_eq!(sniff_image_format(&[]), None);
    }
}
