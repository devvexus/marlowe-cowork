//! Images as the file tools meet them: type and pixel size from the header (PNG, JPEG, GIF,
//! WebP; no decoding), and base64 for notebook outputs. Pure Rust, no image crate.

/// The image's MIME type and pixel size, when `bytes` start like a PNG, JPEG, GIF or WebP.
pub fn sniff_image(bytes: &[u8]) -> Option<(&'static str, u32, u32)> {
    let be16 = |i: usize| -> Option<u32> { Some(u32::from(u16::from_be_bytes([*bytes.get(i)?, *bytes.get(i + 1)?]))) };
    let le16 = |i: usize| -> Option<u32> { Some(u32::from(u16::from_le_bytes([*bytes.get(i)?, *bytes.get(i + 1)?]))) };
    let be32 = |i: usize| -> Option<u32> { Some(u32::from_be_bytes(bytes.get(i..i + 4)?.try_into().ok()?)) };
    let le24 = |i: usize| -> Option<u32> {
        let b = bytes.get(i..i + 3)?;
        Some(u32::from(b[0]) | u32::from(b[1]) << 8 | u32::from(b[2]) << 16)
    };
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") && bytes.get(12..16) == Some(b"IHDR") {
        return Some(("image/png", be32(16)?, be32(20)?));
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some(("image/gif", le16(6)?, le16(8)?));
    }
    if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        return match bytes.get(12..16)? {
            b"VP8 " => Some(("image/webp", le16(26)? & 0x3fff, le16(28)? & 0x3fff)),
            b"VP8L" => {
                let b = bytes.get(21..25)?;
                let v = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
                Some(("image/webp", (v & 0x3fff) + 1, ((v >> 14) & 0x3fff) + 1))
            }
            b"VP8X" => Some(("image/webp", le24(24)? + 1, le24(27)? + 1)),
            _ => None,
        };
    }
    if bytes.starts_with(&[0xFF, 0xD8]) {
        let mut i = 2usize;
        while i + 4 <= bytes.len() {
            if bytes[i] != 0xFF {
                i += 1;
                continue;
            }
            let marker = bytes[i + 1];
            if marker == 0xFF {
                i += 1;
                continue;
            }
            if marker == 0xD8 || marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
                i += 2;
                continue;
            }
            let len = be16(i + 2)? as usize;
            let sof = (0xC0..=0xCF).contains(&marker) && ![0xC4, 0xC8, 0xCC].contains(&marker);
            if sof {
                return Some(("image/jpeg", be16(i + 7)?, be16(i + 5)?));
            }
            if marker == 0xD9 || marker == 0xDA {
                return None;
            }
            i += 2 + len;
        }
    }
    None
}

/// The MIME type an image file name suggests (`.png`, `.jpg`, `.jpeg`, `.gif`, `.webp`).
pub fn image_mime_for(name: &str) -> Option<&'static str> {
    let ext = name.rsplit('.').next()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => return None,
    })
}

/// Standard base64 (whitespace ignored, padding optional); `None` on a bad character.
pub fn base64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        } as u32)
    }
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for &c in s.as_bytes() {
        if c.is_ascii_whitespace() {
            continue;
        }
        if c == b'=' {
            break;
        }
        acc = (acc << 6) | val(c)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 3x2 PNG header (IHDR only; enough for sniffing).
    pub fn png_header(w: u32, h: u32) -> Vec<u8> {
        let mut v = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR".to_vec();
        v.extend_from_slice(&w.to_be_bytes());
        v.extend_from_slice(&h.to_be_bytes());
        v.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
        v
    }

    #[test]
    fn headers_give_type_and_size() {
        assert_eq!(sniff_image(&png_header(3, 2)), Some(("image/png", 3, 2)));
        assert_eq!(sniff_image(b"GIF89a\x10\x00\x20\x00rest"), Some(("image/gif", 16, 32)));
        let jpeg = [0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0, 0, 0xFF, 0xC0, 0x00, 0x11, 8, 0x00, 0x30, 0x00, 0x40, 3];
        assert_eq!(sniff_image(&jpeg), Some(("image/jpeg", 64, 48)));
        let mut webp = b"RIFF\0\0\0\0WEBPVP8X\x0a\0\0\0\0\0\0\0".to_vec();
        webp.extend_from_slice(&[99, 0, 0, 49, 0, 0]);
        assert_eq!(sniff_image(&webp), Some(("image/webp", 100, 50)));
        assert_eq!(sniff_image(b"plain text"), None);
        assert_eq!(image_mime_for("a/B.JPG"), Some("image/jpeg"));
    }

    #[test]
    fn base64_matches_the_standard_vectors() {
        assert_eq!(base64_decode("TWFu").unwrap(), b"Man");
        assert_eq!(base64_decode("TWE=").unwrap(), b"Ma");
        assert_eq!(base64_decode("TQ==\n").unwrap(), b"M");
        assert_eq!(base64_decode("aGVs\nbG8=").unwrap(), b"hello");
        assert!(base64_decode("a$b").is_none());
    }
}
