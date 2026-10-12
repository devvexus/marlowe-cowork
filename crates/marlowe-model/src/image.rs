//! Images: the blake3 blob store behind [`ImageRef`], ingest, and per-provider downscaling
//! (design 3.10).
//!
//! A message carries only an `ImageRef { mime, blob, w, h }`; the bytes live in a [`BlobStore`]
//! (`<home>\blobs\<hex>` for [`FsBlobStore`]). [`ingest`] decodes whatever was pasted or read
//! (png, jpeg, gif, webp), caps its long edge, re-encodes it as png or jpeg and stores it.
//! A provider calls [`wire_image`] at request time: the stored image is downscaled again, and
//! the result cached in the store, when the provider's long edge is smaller than the stored one.
//! Nothing here is async and no decode failure panics.

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::path::PathBuf;
use std::sync::Mutex;

use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, ImageEncoder, ImageFormat, ImageReader, Limits};

use crate::info::ProviderKind;
use crate::message::ImageRef;

/// Long edge Ollama vision models are sent (Qwen-VL class: more pixels is more prefill, not more
/// sight).
pub const OLLAMA_LONG_EDGE: u32 = 1024;
/// Anthropic's own downscale target; larger images are resized server side anyway.
pub const ANTHROPIC_LONG_EDGE: u32 = 1568;
/// OpenRouter frontier models.
pub const OPENROUTER_LONG_EDGE: u32 = 2048;
/// The cap [`ingest`] applies when the caller names none: the largest any provider takes.
pub const STORE_LONG_EDGE: u32 = 2048;
/// Input larger than this is refused before decoding.
pub const MAX_INPUT_BYTES: usize = 25 * 1024 * 1024;
/// Input with more pixels than this is refused (decompression bombs).
pub const MAX_INPUT_PIXELS: u64 = 80_000_000;
/// An encoded image above this is re-encoded as jpeg.
pub const MAX_WIRE_BYTES: usize = 4 * 1024 * 1024;

/// The long edge `kind` should be sent.
pub fn long_edge_for(kind: ProviderKind) -> u32 {
    match kind {
        ProviderKind::Ollama | ProviderKind::LocalGguf => OLLAMA_LONG_EDGE,
        ProviderKind::Anthropic => ANTHROPIC_LONG_EDGE,
        ProviderKind::OpenRouter | ProviderKind::OpenAi => OPENROUTER_LONG_EDGE,
    }
}

/// Tokens an image costs, about one per 750 pixels clamped to what providers charge. The same
/// rule the context meter uses.
pub fn estimate_tokens(w: u32, h: u32) -> u32 {
    (u64::from(w) * u64::from(h) / 750).clamp(85, 1_600) as u32
}

/// Size after fitting the long edge to `long_edge`; never enlarges, never returns a zero side.
pub fn target_size(w: u32, h: u32, long_edge: u32) -> (u32, u32) {
    let long = w.max(h);
    if long == 0 || long <= long_edge {
        return (w.max(1), h.max(1));
    }
    let scale = f64::from(long_edge) / f64::from(long);
    let s = |v: u32| ((f64::from(v) * scale).round() as u32).max(1);
    (s(w), s(h))
}

/// Why an image could not be stored or sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageError {
    /// Not an image format this build reads.
    Unsupported(String),
    TooLarge(String),
    Decode(String),
    Io(String),
    /// The blob is not in the store.
    Missing(String),
}

impl fmt::Display for ImageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImageError::Unsupported(s) => write!(f, "unsupported image: {s}"),
            ImageError::TooLarge(s) => write!(f, "image too large: {s}"),
            ImageError::Decode(s) => write!(f, "could not decode the image: {s}"),
            ImageError::Io(s) => write!(f, "image store error: {s}"),
            ImageError::Missing(h) => write!(f, "image {h} is not in the blob store"),
        }
    }
}

impl std::error::Error for ImageError {}

/// Content-addressed bytes. Keys are lower-case hex blake3.
pub trait BlobStore: Send + Sync {
    /// Stores `bytes` and returns their key. Storing the same bytes twice is cheap.
    fn put(&self, bytes: &[u8]) -> io::Result<String>;
    /// The bytes for `key`, or `NotFound`.
    fn get(&self, key: &str) -> io::Result<Vec<u8>>;
}

/// True for a 64-character lower-case hex key (so a key can never be a path).
pub fn is_blob_key(key: &str) -> bool {
    key.len() == 64 && key.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The key `bytes` are stored under.
pub fn blob_key(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

fn bad_key(key: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, format!("`{key}` is not a blob key"))
}

/// A store in memory, for tests and one-shot tools.
#[derive(Default)]
pub struct MemoryBlobStore {
    map: Mutex<HashMap<String, Vec<u8>>>,
}

impl MemoryBlobStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of blobs held.
    pub fn len(&self) -> usize {
        self.map.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl BlobStore for MemoryBlobStore {
    fn put(&self, bytes: &[u8]) -> io::Result<String> {
        let key = blob_key(bytes);
        self.map.lock().unwrap_or_else(|e| e.into_inner()).entry(key.clone()).or_insert_with(|| bytes.to_vec());
        Ok(key)
    }

    fn get(&self, key: &str) -> io::Result<Vec<u8>> {
        if !is_blob_key(key) {
            return Err(bad_key(key));
        }
        self.map
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such blob"))
    }
}

/// A directory of files named by key (`<home>\blobs`). Writes go through a temp file and a
/// rename, so a crash leaves either the whole blob or none.
pub struct FsBlobStore {
    dir: PathBuf,
}

impl FsBlobStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &std::path::Path {
        &self.dir
    }
}

impl BlobStore for FsBlobStore {
    fn put(&self, bytes: &[u8]) -> io::Result<String> {
        let key = blob_key(bytes);
        let path = self.dir.join(&key);
        if path.is_file() {
            return Ok(key);
        }
        std::fs::create_dir_all(&self.dir)?;
        let tmp = self.dir.join(format!("{key}.{}.tmp", std::process::id()));
        std::fs::write(&tmp, bytes)?;
        match std::fs::rename(&tmp, &path) {
            Ok(()) => Ok(key),
            Err(e) => {
                // Another writer won the race for the same content: that is success.
                let _ = std::fs::remove_file(&tmp);
                if path.is_file() {
                    Ok(key)
                } else {
                    Err(e)
                }
            }
        }
    }

    fn get(&self, key: &str) -> io::Result<Vec<u8>> {
        if !is_blob_key(key) {
            return Err(bad_key(key));
        }
        std::fs::read(self.dir.join(key))
    }
}

/// The mime type of `bytes` by magic number.
pub fn sniff_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

fn format_of(mime: &str) -> Option<ImageFormat> {
    match mime {
        "image/png" => Some(ImageFormat::Png),
        "image/jpeg" => Some(ImageFormat::Jpeg),
        "image/gif" => Some(ImageFormat::Gif),
        "image/webp" => Some(ImageFormat::WebP),
        _ => None,
    }
}

fn reader(bytes: &[u8], format: ImageFormat) -> ImageReader<io::Cursor<&[u8]>> {
    let mut r = ImageReader::with_format(io::Cursor::new(bytes), format);
    let mut limits = Limits::default();
    limits.max_alloc = Some(MAX_INPUT_PIXELS * 4 + (64 << 20));
    r.limits(limits);
    r
}

fn decode(bytes: &[u8]) -> Result<(DynamicImage, &'static str), ImageError> {
    if bytes.len() > MAX_INPUT_BYTES {
        return Err(ImageError::TooLarge(format!("{} bytes (limit {MAX_INPUT_BYTES})", bytes.len())));
    }
    let mime = sniff_mime(bytes).ok_or_else(|| {
        ImageError::Unsupported("not png, jpeg, gif or webp (bmp, tiff, svg and heic are not read)".into())
    })?;
    let format = format_of(mime).ok_or_else(|| ImageError::Unsupported(mime.into()))?;
    let (w, h) = reader(bytes, format).into_dimensions().map_err(|e| ImageError::Decode(e.to_string()))?;
    if u64::from(w) * u64::from(h) > MAX_INPUT_PIXELS {
        return Err(ImageError::TooLarge(format!("{w}x{h} pixels")));
    }
    let img = reader(bytes, format).decode().map_err(|e| ImageError::Decode(e.to_string()))?;
    Ok((img, mime))
}

/// Encodes as png (`prefer_jpeg` false) or jpeg; falls back to jpeg when the png is too large.
fn encode(img: &DynamicImage, prefer_jpeg: bool) -> Result<(Vec<u8>, &'static str), ImageError> {
    let jpeg = |img: &DynamicImage| -> Result<Vec<u8>, ImageError> {
        let flat = if img.color().has_alpha() { flatten_on_white(img) } else { img.to_rgb8() };
        let mut out = Vec::new();
        JpegEncoder::new_with_quality(&mut out, 88)
            .write_image(flat.as_raw(), flat.width(), flat.height(), image::ExtendedColorType::Rgb8)
            .map_err(|e| ImageError::Decode(e.to_string()))?;
        Ok(out)
    };
    if prefer_jpeg {
        return Ok((jpeg(img)?, "image/jpeg"));
    }
    let rgba = img.to_rgba8();
    let mut out = Vec::new();
    PngEncoder::new(&mut out)
        .write_image(rgba.as_raw(), rgba.width(), rgba.height(), image::ExtendedColorType::Rgba8)
        .map_err(|e| ImageError::Decode(e.to_string()))?;
    if out.len() > MAX_WIRE_BYTES {
        return Ok((jpeg(img)?, "image/jpeg"));
    }
    Ok((out, "image/png"))
}

fn flatten_on_white(img: &DynamicImage) -> image::RgbImage {
    let rgba = img.to_rgba8();
    let mut out = image::RgbImage::new(rgba.width(), rgba.height());
    for (x, y, p) in rgba.enumerate_pixels() {
        let a = u32::from(p[3]);
        let mix = |c: u8| ((u32::from(c) * a + 255 * (255 - a) + 127) / 255) as u8;
        out.put_pixel(x, y, image::Rgb([mix(p[0]), mix(p[1]), mix(p[2])]));
    }
    out
}

/// Decodes `bytes`, caps the long edge at `long_edge` (never enlarging), re-encodes unless the
/// original is already a png/jpeg within the cap and size, stores it, and returns the reference.
pub fn ingest(store: &dyn BlobStore, bytes: &[u8], long_edge: u32) -> Result<ImageRef, ImageError> {
    let (img, mime) = decode(bytes)?;
    let (w, h) = (img.width(), img.height());
    let (tw, th) = target_size(w, h, long_edge.max(16));
    let native = matches!(mime, "image/png" | "image/jpeg");
    if native && (tw, th) == (w, h) && bytes.len() <= MAX_WIRE_BYTES {
        let blob = store.put(bytes).map_err(|e| ImageError::Io(e.to_string()))?;
        return Ok(ImageRef { mime: mime.to_string(), blob, w, h });
    }
    let img = if (tw, th) == (w, h) { img } else { img.resize_exact(tw, th, FilterType::Lanczos3) };
    let (encoded, out_mime) = encode(&img, mime == "image/jpeg")?;
    let blob = store.put(&encoded).map_err(|e| ImageError::Io(e.to_string()))?;
    Ok(ImageRef { mime: out_mime.to_string(), blob, w: tw, h: th })
}

/// The bytes of a stored image.
pub fn load(store: &dyn BlobStore, r: &ImageRef) -> Result<Vec<u8>, ImageError> {
    store.get(&r.blob).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => ImageError::Missing(r.blob.clone()),
        _ => ImageError::Io(e.to_string()),
    })
}

/// `r` itself when it fits `long_edge`, else a downscaled copy stored alongside it.
pub fn fit(store: &dyn BlobStore, r: &ImageRef, long_edge: u32) -> Result<ImageRef, ImageError> {
    if r.w.max(r.h) <= long_edge {
        return Ok(r.clone());
    }
    let bytes = load(store, r)?;
    ingest(store, &bytes, long_edge)
}

/// An image ready for a request body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireImage {
    pub mime: String,
    pub base64: String,
    pub w: u32,
    pub h: u32,
}

impl WireImage {
    /// `data:<mime>;base64,<data>`, the form OpenAI-dialect `image_url` takes.
    pub fn data_uri(&self) -> String {
        format!("data:{};base64,{}", self.mime, self.base64)
    }
}

/// Loads `r`, fits it to `long_edge` and base64-encodes it.
pub fn wire_image(store: &dyn BlobStore, r: &ImageRef, long_edge: u32) -> Result<WireImage, ImageError> {
    let fitted = fit(store, r, long_edge)?;
    let bytes = load(store, &fitted)?;
    Ok(WireImage { mime: fitted.mime, base64: base64_encode(&bytes), w: fitted.w, h: fitted.h })
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding.
pub fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = match chunk.len() {
            3 => u32::from(chunk[0]) << 16 | u32::from(chunk[1]) << 8 | u32::from(chunk[2]),
            2 => u32::from(chunk[0]) << 16 | u32::from(chunk[1]) << 8,
            _ => u32::from(chunk[0]) << 16,
        };
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { B64[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { B64[n as usize & 63] as char } else { '=' });
    }
    out
}

/// Decodes standard or url-safe base64; whitespace, padding and a `data:...;base64,` prefix are
/// tolerated. `None` on any other byte.
pub fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let body = match text.trim().split_once(";base64,") {
        Some((head, rest)) if head.starts_with("data:") => rest,
        _ => text.trim(),
    };
    let mut out = Vec::with_capacity(body.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for b in body.bytes() {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' | b'\n' | b'\r' | b' ' | b'\t' => continue,
            _ => return None,
        };
        acc = acc << 6 | u32::from(v);
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

    fn png(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbaImage::from_fn(w, h, |x, y| image::Rgba([(x % 251) as u8, (y % 241) as u8, 90, 255]));
        let mut out = Vec::new();
        PngEncoder::new(&mut out).write_image(img.as_raw(), w, h, image::ExtendedColorType::Rgba8).unwrap();
        out
    }

    fn jpeg(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbImage::from_fn(w, h, |x, y| image::Rgb([(x % 200) as u8, (y % 200) as u8, 40]));
        let mut out = Vec::new();
        JpegEncoder::new_with_quality(&mut out, 80)
            .write_image(img.as_raw(), w, h, image::ExtendedColorType::Rgb8)
            .unwrap();
        out
    }

    #[test]
    fn base64_matches_the_rfc_vectors_and_round_trips() {
        for (raw, enc) in
            [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foob", "Zm9vYg=="), ("fooba", "Zm9vYmE="), ("foobar", "Zm9vYmFy")]
        {
            assert_eq!(base64_encode(raw.as_bytes()), enc);
            assert_eq!(base64_decode(enc).unwrap(), raw.as_bytes());
        }
        assert_eq!(base64_decode("Zm9v\nYmFy").unwrap(), b"foobar");
        assert_eq!(base64_decode("data:image/png;base64,Zm9v").unwrap(), b"foo");
        assert!(base64_decode("Zm9v!").is_none());
        let all: Vec<u8> = (0..=255).collect();
        assert_eq!(base64_decode(&base64_encode(&all)).unwrap(), all);
    }

    #[test]
    fn target_size_fits_the_long_edge_without_enlarging() {
        assert_eq!(target_size(3000, 1500, 1024), (1024, 512));
        assert_eq!(target_size(1500, 3000, 1024), (512, 1024));
        assert_eq!(target_size(800, 600, 1024), (800, 600));
        assert_eq!(target_size(10_000, 3, 1024), (1024, 1), "a side never reaches zero");
    }

    #[test]
    fn a_large_png_is_downscaled_on_ingest_and_stored_once() {
        let store = MemoryBlobStore::new();
        let r = ingest(&store, &png(3000, 1500), OLLAMA_LONG_EDGE).unwrap();
        assert_eq!((r.w, r.h, r.mime.as_str()), (1024, 512, "image/png"));
        assert!(is_blob_key(&r.blob));
        let stored = store.get(&r.blob).unwrap();
        assert_eq!(blob_key(&stored), r.blob, "content addressed");
        assert_eq!(image::load_from_memory(&stored).unwrap().width(), 1024);
        let again = ingest(&store, &png(3000, 1500), OLLAMA_LONG_EDGE).unwrap();
        assert_eq!(again, r);
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn an_image_within_the_cap_is_stored_untouched() {
        let store = MemoryBlobStore::new();
        let bytes = png(640, 480);
        let r = ingest(&store, &bytes, OLLAMA_LONG_EDGE).unwrap();
        assert_eq!(r.blob, blob_key(&bytes));
        assert_eq!((r.w, r.h), (640, 480));
        let j = jpeg(500, 300);
        let r = ingest(&store, &j, OLLAMA_LONG_EDGE).unwrap();
        assert_eq!((r.mime.as_str(), r.blob.as_str()), ("image/jpeg", blob_key(&j).as_str()));
    }

    #[test]
    fn a_big_jpeg_stays_jpeg_and_a_gif_becomes_png() {
        let store = MemoryBlobStore::new();
        let r = ingest(&store, &jpeg(2400, 1200), 1568).unwrap();
        assert_eq!((r.w, r.h, r.mime.as_str()), (1568, 784, "image/jpeg"));
        let mut gif = Vec::new();
        {
            let frame = image::RgbaImage::from_pixel(32, 16, image::Rgba([200, 10, 10, 255]));
            image::codecs::gif::GifEncoder::new(&mut gif)
                .encode(frame.as_raw(), 32, 16, image::ExtendedColorType::Rgba8)
                .unwrap();
        }
        assert_eq!(sniff_mime(&gif), Some("image/gif"));
        let r = ingest(&store, &gif, OLLAMA_LONG_EDGE).unwrap();
        assert_eq!((r.mime.as_str(), r.w, r.h), ("image/png", 32, 16));
    }

    #[test]
    fn fit_downscales_for_a_smaller_provider_and_caches_the_copy() {
        let store = MemoryBlobStore::new();
        let big = ingest(&store, &png(2000, 1000), OPENROUTER_LONG_EDGE).unwrap();
        assert_eq!(big.w, 2000);
        assert_eq!(fit(&store, &big, OPENROUTER_LONG_EDGE).unwrap(), big);
        let small = fit(&store, &big, OLLAMA_LONG_EDGE).unwrap();
        assert_eq!((small.w, small.h), (1024, 512));
        assert_ne!(small.blob, big.blob);
        let wire = wire_image(&store, &big, OLLAMA_LONG_EDGE).unwrap();
        assert_eq!((wire.w, wire.h), (1024, 512));
        assert!(wire.data_uri().starts_with("data:image/png;base64,iVBOR"));
        assert_eq!(base64_decode(&wire.base64).unwrap(), store.get(&small.blob).unwrap());
    }

    #[test]
    fn bad_input_is_a_typed_error_never_a_panic() {
        let store = MemoryBlobStore::new();
        assert!(matches!(ingest(&store, b"%PDF-1.7", 1024), Err(ImageError::Unsupported(_))));
        assert!(matches!(ingest(&store, b"", 1024), Err(ImageError::Unsupported(_))));
        let mut cut = png(64, 64);
        cut.truncate(cut.len() / 2);
        assert!(matches!(ingest(&store, &cut, 1024), Err(ImageError::Decode(_))));
        let ghost = ImageRef { mime: "image/png".into(), blob: "0".repeat(64), w: 5000, h: 5000 };
        assert!(matches!(wire_image(&store, &ghost, 1024), Err(ImageError::Missing(_))));
        assert!(store.get("../../etc/passwd").is_err());
    }

    #[test]
    fn the_filesystem_store_round_trips_and_refuses_paths() {
        let dir = std::env::temp_dir().join(format!("marlowe-blobs-{}-{}", std::process::id(), blob_key(b"fs")));
        let store = FsBlobStore::new(&dir);
        let r = ingest(&store, &png(100, 50), 1024).unwrap();
        assert_eq!(store.get(&r.blob).unwrap(), png(100, 50));
        assert_eq!(store.put(&png(100, 50)).unwrap(), r.blob, "putting twice is a no-op");
        assert!(store.get("..\\x").is_err());
        assert!(store.get(&"A".repeat(64)).is_err(), "upper-case is not a key");
        assert!(std::fs::read_dir(&dir).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().ends_with(".tmp")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn provider_edges_follow_the_design() {
        assert_eq!(long_edge_for(ProviderKind::Ollama), 1024);
        assert_eq!(long_edge_for(ProviderKind::Anthropic), 1568);
        assert_eq!(long_edge_for(ProviderKind::OpenRouter), 2048);
        assert_eq!(estimate_tokens(1024, 512), 699);
        assert_eq!(estimate_tokens(10, 10), 85);
    }
}
