//! Image attachments (U3): Ctrl+V of a clipboard image, a dropped file path, or `@shot.png`.
//!
//! The composer holds a `[Image #n]` marker for each one, so the user can delete it with the rest
//! of the text and the model sees which picture is which. On send, each attachment still marked
//! in the text goes up as a `PutBlob` (the daemon downscales it per provider and answers with a
//! `BlobStored` reference), and the `Ask` follows with the references in `images`.

use std::sync::Arc;

use marlowe_model::ImageRef;
use marlowe_proto::{Request, SessionId};

/// Biggest image taken from disk or the clipboard (the daemon downscales; this only bounds the
/// wire).
pub const MAX_IMAGE_BYTES: usize = 12 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    /// The `n` in `[Image #n]`.
    pub n: u32,
    pub mime: String,
    pub bytes: Arc<Vec<u8>>,
    pub w: u32,
    pub h: u32,
    /// Where it came from, for the chip.
    pub source: String,
}

impl Attachment {
    pub fn new(n: u32, mime: &str, bytes: Vec<u8>, source: &str) -> Attachment {
        let (w, h) = dimensions(&bytes).unwrap_or((0, 0));
        Attachment { n, mime: mime.to_string(), bytes: Arc::new(bytes), w, h, source: source.to_string() }
    }

    pub fn marker(&self) -> String {
        format!("[Image #{}]", self.n)
    }

    /// `[Image #1] 1024×768 · 212 KB`
    pub fn chip(&self) -> String {
        let size = if self.w > 0 { format!(" {}×{} ·", self.w, self.h) } else { String::new() };
        format!("{}{size} {}", self.marker(), crate::text::bytes(self.bytes.len() as u64))
    }
}

/// The image MIME type for a file name, if it is one the providers take.
pub fn image_mime(path: &str) -> Option<&'static str> {
    let ext = path.rsplit_once('.')?.1.to_ascii_lowercase();
    match ext.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "bmp" => Some("image/bmp"),
        _ => None,
    }
}

/// A pasted string that is one path to an image: what a terminal types when a file is dropped on
/// it. Quotes (Windows Terminal) and a `file://` prefix are removed.
pub fn dropped_image_path(pasted: &str) -> Option<String> {
    let t = pasted.trim();
    if t.contains('\n') || t.is_empty() || t.len() > 1024 {
        return None;
    }
    let quoted = t.starts_with(['"', '\'']);
    let t = t.trim_matches(|c| c == '"' || c == '\'');
    let t = t.strip_prefix("file:///").unwrap_or(t);
    // A sentence that happens to end in `.png` is text; a path is quoted or rooted.
    let b = t.as_bytes();
    let rooted = t.starts_with(['/', '\\', '~']) || t.starts_with("./") || (b.len() > 2 && b[1] == b':' && b[0].is_ascii_alphabetic());
    if !(quoted || rooted) {
        return None;
    }
    image_mime(t).map(|_| t.to_string())
}

/// `@path` tokens in a message that name images, in order: `@shot.png`, `@"my shots/a b.png"`.
pub fn image_mentions(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for tok in text.split_whitespace() {
        if let Some(p) = tok.strip_prefix('@') {
            let p = p.trim_matches('"');
            if image_mime(p).is_some() && !out.contains(&p.to_string()) {
                out.push(p.to_string());
            }
        }
    }
    out
}

/// Width and height from a PNG, GIF, JPEG or BMP header; `None` for anything else.
pub fn dimensions(b: &[u8]) -> Option<(u32, u32)> {
    if b.len() > 24 && b.starts_with(b"\x89PNG\r\n\x1a\n") {
        let w = u32::from_be_bytes(b[16..20].try_into().ok()?);
        let h = u32::from_be_bytes(b[20..24].try_into().ok()?);
        return Some((w, h));
    }
    if b.len() > 10 && (b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a")) {
        let w = u16::from_le_bytes([b[6], b[7]]) as u32;
        let h = u16::from_le_bytes([b[8], b[9]]) as u32;
        return Some((w, h));
    }
    if b.len() > 26 && b.starts_with(b"BM") {
        let w = i32::from_le_bytes(b[18..22].try_into().ok()?).unsigned_abs();
        let h = i32::from_le_bytes(b[22..26].try_into().ok()?).unsigned_abs();
        return Some((w, h));
    }
    if b.len() > 4 && b[0] == 0xFF && b[1] == 0xD8 {
        let mut i = 2;
        while i + 9 < b.len() {
            if b[i] != 0xFF {
                i += 1;
                continue;
            }
            let marker = b[i + 1];
            if marker == 0xFF {
                i += 1;
                continue;
            }
            let len = u16::from_be_bytes([b[i + 2], b[i + 3]]) as usize;
            if (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
                let h = u16::from_be_bytes([b[i + 5], b[i + 6]]) as u32;
                let w = u16::from_be_bytes([b[i + 7], b[i + 8]]) as u32;
                return Some((w, h));
            }
            i += 2 + len.max(2);
        }
    }
    None
}

/// Standard base64 with padding.
pub fn base64(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let n = (u32::from(c[0]) << 16) | (u32::from(*c.get(1).unwrap_or(&0)) << 8) | u32::from(*c.get(2).unwrap_or(&0));
        out.push(A[(n >> 18) as usize & 63] as char);
        out.push(A[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { A[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { A[n as usize & 63] as char } else { '=' });
    }
    out
}

/// A message on its way: the `PutBlob`s are out, the `Ask` waits for their references.
#[derive(Debug, Clone, PartialEq)]
pub struct Upload {
    pub session: Option<SessionId>,
    pub text: String,
    pub workspace: Option<String>,
    pub waiting: usize,
    pub done: Vec<ImageRef>,
}

impl Upload {
    /// The requests that start an upload of `atts`.
    pub fn start(
        session: Option<SessionId>,
        text: String,
        workspace: Option<String>,
        atts: &[Attachment],
    ) -> (Upload, Vec<Request>) {
        let reqs = atts.iter().map(|a| Request::PutBlob { mime: a.mime.clone(), base64: base64(&a.bytes) }).collect();
        (Upload { session, text, workspace, waiting: atts.len(), done: Vec::new() }, reqs)
    }

    /// One reference came back. The `Ask` once all have.
    pub fn stored(&mut self, blob: &str, mime: &str, w: u32, h: u32) -> Option<Request> {
        self.done.push(ImageRef { mime: mime.to_string(), blob: blob.to_string(), w, h });
        self.waiting = self.waiting.saturating_sub(1);
        (self.waiting == 0).then(|| Request::Ask {
            session: self.session.clone(),
            text: std::mem::take(&mut self.text),
            workspace: self.workspace.take(),
            images: std::mem::take(&mut self.done),
            options: None,
            client_ms: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_rfc_vectors() {
        for (i, o) in [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foob", "Zm9vYg=="), ("fooba", "Zm9vYmE="), ("foobar", "Zm9vYmFy")] {
            assert_eq!(base64(i.as_bytes()), o);
        }
    }

    #[test]
    fn headers_give_dimensions() {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend_from_slice(&640u32.to_be_bytes());
        png.extend_from_slice(&480u32.to_be_bytes());
        png.extend_from_slice(&[8, 6, 0, 0, 0]);
        assert_eq!(dimensions(&png), Some((640, 480)));
        let gif = b"GIF89a\x20\x01\x10\x00\0\0\0\0".to_vec();
        assert_eq!(dimensions(&gif), Some((288, 16)));
        // SOI, APP0 (len 16), SOF0 (h=300, w=200).
        let mut jpg = vec![0xFF, 0xD8, 0xFF, 0xE0, 0, 16];
        jpg.extend_from_slice(&[0; 14]);
        jpg.extend_from_slice(&[0xFF, 0xC0, 0, 17, 8, 1, 44, 0, 200, 3, 1, 0x22, 0, 2, 0x11, 1]);
        assert_eq!(dimensions(&jpg), Some((200, 300)));
        assert_eq!(dimensions(b"not an image at all, just text"), None);
    }

    #[test]
    fn a_dropped_path_is_one_image_file() {
        assert_eq!(dropped_image_path("\"C:\\Users\\m\\Pictures\\shot 1.PNG\"").as_deref(), Some("C:\\Users\\m\\Pictures\\shot 1.PNG"));
        assert_eq!(dropped_image_path("  /home/m/a.jpeg\n").as_deref(), Some("/home/m/a.jpeg"));
        assert!(dropped_image_path("notes.txt").is_none());
        assert!(dropped_image_path("a.png\nb.png").is_none());
        assert!(dropped_image_path("please look at a.png").is_none(), "a sentence is text");
    }

    #[test]
    fn mentions_name_images_only() {
        assert_eq!(image_mentions("look at @shot.png and @src/main.rs and @\"x y.jpg\" and @shot.png"), ["shot.png"]);
    }

    #[test]
    fn the_ask_waits_for_every_reference() {
        let a = Attachment::new(1, "image/png", vec![1, 2, 3], "clipboard");
        let b = Attachment::new(2, "image/png", vec![4, 5, 6], "clipboard");
        let (mut up, reqs) = Upload::start(Some(SessionId::from("s")), "see [Image #1] [Image #2]".into(), None, &[a, b]);
        assert_eq!(reqs.len(), 2);
        assert!(matches!(&reqs[0], Request::PutBlob { mime, base64 } if mime == "image/png" && base64 == "AQID"));
        assert!(up.stored("b1", "image/png", 10, 10).is_none());
        let ask = up.stored("b2", "image/png", 20, 20).unwrap();
        match ask {
            Request::Ask { images, text, .. } => {
                assert_eq!(images.len(), 2);
                assert_eq!(images[1].blob, "b2");
                assert!(text.contains("[Image #2]"));
            }
            other => panic!("{other:?}"),
        }
    }
}
