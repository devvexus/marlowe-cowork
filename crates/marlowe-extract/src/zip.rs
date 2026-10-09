//! A minimal ZIP reader, hand-rolled over `flate2`.
//!
//! # Why not the `zip` crate
//!
//! Measured against this workspace's lockfile it costs **23 new crates**, because it brings
//! backends for bzip2, zstd, deflate64, LZMA and AES. Office documents and epubs use exactly two
//! storage methods — **stored** and **deflate** — and `flate2` was already in the tree at zero
//! marginal cost. Twenty-three crates to decompress formats that never appear in the inputs is a
//! poor trade, and unlike the PDF decision there is no coverage argument on the other side.
//!
//! This reads the **central directory**, which is the correct way round: the local headers are
//! redundant, frequently have zeroed sizes with the real values in a trailing data descriptor,
//! and cannot be walked reliably without the directory anyway.

use std::io::Read;

/// Per-entry ceiling. Bounds a zip bomb: a 40 KB archive can otherwise declare a 4 GB member.
const MAX_ENTRY_BYTES: u64 = 96 * 1024 * 1024;

/// Total across all extracted entries.
const MAX_TOTAL_BYTES: u64 = 256 * 1024 * 1024;

const EOCD_SIG: u32 = 0x0605_4b50;
const CD_SIG: u32 = 0x0201_4b50;

#[derive(Debug, thiserror::Error)]
pub enum ZipError {
    #[error("not a zip archive: no end-of-central-directory record")]
    NotZip,
    #[error("the central directory is malformed at offset {at}")]
    Malformed { at: usize },
    #[error("entry {name} uses compression method {method}, which this build does not decode")]
    UnsupportedMethod { name: String, method: u16 },
    #[error("entry {name} inflates to more than the {MAX_ENTRY_BYTES} byte per-entry ceiling")]
    EntryTooLarge { name: String },
    #[error("decompressing {name}: {detail}")]
    Inflate { name: String, detail: String },
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub compressed_size: u64,
    pub uncompressed_size: u64,
    pub method: u16,
    local_header_offset: u64,
}

pub struct Archive<'a> {
    bytes: &'a [u8],
    pub entries: Vec<Entry>,
    spent: u64,
}

impl<'a> Archive<'a> {
    /// Parse the central directory. Does not decompress anything.
    pub fn open(bytes: &'a [u8]) -> Result<Self, ZipError> {
        let eocd = find_eocd(bytes).ok_or(ZipError::NotZip)?;
        let count = u16::from_le_bytes([bytes[eocd + 10], bytes[eocd + 11]]) as usize;
        let cd_offset = u32::from_le_bytes([
            bytes[eocd + 16],
            bytes[eocd + 17],
            bytes[eocd + 18],
            bytes[eocd + 19],
        ]) as usize;

        let mut entries = Vec::with_capacity(count.min(4_096));
        let mut at = cd_offset;
        while at + 46 <= bytes.len() {
            if read_u32(bytes, at) != CD_SIG {
                break;
            }
            let method = read_u16(bytes, at + 10);
            let compressed_size = read_u32(bytes, at + 20) as u64;
            let uncompressed_size = read_u32(bytes, at + 24) as u64;
            let name_len = read_u16(bytes, at + 28) as usize;
            let extra_len = read_u16(bytes, at + 30) as usize;
            let comment_len = read_u16(bytes, at + 32) as usize;
            let local_header_offset = read_u32(bytes, at + 42) as u64;

            let name_start = at + 46;
            let name_end = name_start + name_len;
            if name_end > bytes.len() {
                return Err(ZipError::Malformed { at });
            }
            // Names are CP437 or UTF-8; lossy is right here because a part name we cannot read is
            // a part we will not match against anyway.
            let name = String::from_utf8_lossy(&bytes[name_start..name_end]).into_owned();
            entries.push(Entry {
                name,
                compressed_size,
                uncompressed_size,
                method,
                local_header_offset,
            });
            at = name_end + extra_len + comment_len;
        }
        if entries.is_empty() {
            return Err(ZipError::NotZip);
        }
        Ok(Self { bytes, entries, spent: 0 })
    }

    /// Find an entry by exact name.
    pub fn find(&self, name: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.name == name)
    }

    /// Every entry whose name starts with `prefix` and ends with `suffix`.
    pub fn matching(&self, prefix: &str, suffix: &str) -> Vec<Entry> {
        let mut v: Vec<Entry> = self
            .entries
            .iter()
            .filter(|e| e.name.starts_with(prefix) && e.name.ends_with(suffix))
            .cloned()
            .collect();
        // Deterministic order: slide2 before slide10, so a deck reads in its own sequence.
        v.sort_by(|a, b| natural_cmp(&a.name, &b.name));
        v
    }

    /// Decompress one entry.
    pub fn read(&mut self, entry: &Entry) -> Result<Vec<u8>, ZipError> {
        if entry.uncompressed_size > MAX_ENTRY_BYTES || self.spent > MAX_TOTAL_BYTES {
            return Err(ZipError::EntryTooLarge { name: entry.name.clone() });
        }
        let lho = entry.local_header_offset as usize;
        if lho + 30 > self.bytes.len() {
            return Err(ZipError::Malformed { at: lho });
        }
        // The local header's name and extra lengths are authoritative for where data begins; they
        // legitimately differ from the central directory's.
        let name_len = read_u16(self.bytes, lho + 26) as usize;
        let extra_len = read_u16(self.bytes, lho + 28) as usize;
        let start = lho + 30 + name_len + extra_len;
        if start > self.bytes.len() {
            return Err(ZipError::Malformed { at: start });
        }
        let end = if entry.compressed_size > 0 {
            (start + entry.compressed_size as usize).min(self.bytes.len())
        } else {
            self.bytes.len()
        };
        let data = &self.bytes[start..end];

        let out = match entry.method {
            0 => data.to_vec(),
            8 => {
                let cap = entry.uncompressed_size.min(MAX_ENTRY_BYTES);
                let mut buf = Vec::with_capacity(cap as usize);
                flate2::read::DeflateDecoder::new(data)
                    .take(MAX_ENTRY_BYTES)
                    .read_to_end(&mut buf)
                    .map_err(|e| ZipError::Inflate {
                        name: entry.name.clone(),
                        detail: e.to_string(),
                    })?;
                buf
            }
            other => {
                return Err(ZipError::UnsupportedMethod {
                    name: entry.name.clone(),
                    method: other,
                })
            }
        };
        self.spent += out.len() as u64;
        Ok(out)
    }
}

/// Scan backwards for the end-of-central-directory record.
///
/// It sits at the very end unless there is an archive comment, which may be up to 64 KB — so the
/// search window is bounded at 64 KB + the record size rather than scanning the whole file.
fn find_eocd(bytes: &[u8]) -> Option<usize> {
    if bytes.len() < 22 {
        return None;
    }
    let window = bytes.len().saturating_sub(66_000);
    let mut i = bytes.len() - 22;
    loop {
        if read_u32(bytes, i) == EOCD_SIG {
            return Some(i);
        }
        if i == 0 || i <= window {
            return None;
        }
        i -= 1;
    }
}

fn read_u16(b: &[u8], at: usize) -> u16 {
    if at + 2 > b.len() {
        return 0;
    }
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn read_u32(b: &[u8], at: usize) -> u32 {
    if at + 4 > b.len() {
        return 0;
    }
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// Compare names so embedded numbers sort numerically — `slide2` before `slide10`.
fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let mut ai = a.chars().peekable();
    let mut bi = b.chars().peekable();
    loop {
        match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => return std::cmp::Ordering::Equal,
            (None, _) => return std::cmp::Ordering::Less,
            (_, None) => return std::cmp::Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let nx: String = take_digits(&mut ai);
                let ny: String = take_digits(&mut bi);
                let vx: u64 = nx.parse().unwrap_or(0);
                let vy: u64 = ny.parse().unwrap_or(0);
                match vx.cmp(&vy) {
                    std::cmp::Ordering::Equal => {}
                    other => return other,
                }
            }
            (Some(x), Some(y)) => {
                ai.next();
                bi.next();
                match x.cmp(&y) {
                    std::cmp::Ordering::Equal => {}
                    other => return other,
                }
            }
        }
    }
}

fn take_digits(it: &mut std::iter::Peekable<std::str::Chars<'_>>) -> String {
    let mut s = String::new();
    while let Some(c) = it.peek().copied() {
        if c.is_ascii_digit() {
            s.push(c);
            it.next();
        } else {
            break;
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Build a one-entry archive by hand, so the test does not depend on the thing it tests.
    fn make_zip(name: &str, content: &[u8], deflate: bool) -> Vec<u8> {
        let (method, payload) = if deflate {
            let mut e = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
            e.write_all(content).unwrap();
            (8u16, e.finish().unwrap())
        } else {
            (0u16, content.to_vec())
        };
        let mut z = Vec::new();
        let lho = 0u32;
        // local file header
        z.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        z.extend_from_slice(&[0; 4]); // version, flags
        z.extend_from_slice(&method.to_le_bytes());
        z.extend_from_slice(&[0; 8]); // time, date, crc (unused here)
        z.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        z.extend_from_slice(&(content.len() as u32).to_le_bytes());
        z.extend_from_slice(&(name.len() as u16).to_le_bytes());
        z.extend_from_slice(&0u16.to_le_bytes());
        z.extend_from_slice(name.as_bytes());
        z.extend_from_slice(&payload);

        let cd = z.len() as u32;
        z.extend_from_slice(&CD_SIG.to_le_bytes());
        z.extend_from_slice(&[0; 6]);
        z.extend_from_slice(&method.to_le_bytes());
        z.extend_from_slice(&[0; 8]);
        z.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        z.extend_from_slice(&(content.len() as u32).to_le_bytes());
        z.extend_from_slice(&(name.len() as u16).to_le_bytes());
        z.extend_from_slice(&[0; 8]);
        z.extend_from_slice(&[0; 4]);
        z.extend_from_slice(&lho.to_le_bytes());
        z.extend_from_slice(name.as_bytes());

        let cd_size = z.len() as u32 - cd;
        z.extend_from_slice(&EOCD_SIG.to_le_bytes());
        z.extend_from_slice(&[0; 4]);
        z.extend_from_slice(&1u16.to_le_bytes());
        z.extend_from_slice(&1u16.to_le_bytes());
        z.extend_from_slice(&cd_size.to_le_bytes());
        z.extend_from_slice(&cd.to_le_bytes());
        z.extend_from_slice(&0u16.to_le_bytes());
        z
    }

    #[test]
    fn a_stored_entry_round_trips() {
        let z = make_zip("word/document.xml", b"<w:t>hello</w:t>", false);
        let mut a = Archive::open(&z).expect("opens");
        let e = a.find("word/document.xml").cloned().expect("found");
        assert_eq!(a.read(&e).unwrap(), b"<w:t>hello</w:t>");
    }

    #[test]
    fn a_deflated_entry_round_trips() {
        let body = "content ".repeat(500);
        let z = make_zip("xl/workbook.xml", body.as_bytes(), true);
        let mut a = Archive::open(&z).expect("opens");
        let e = a.find("xl/workbook.xml").cloned().expect("found");
        assert_eq!(a.read(&e).unwrap(), body.as_bytes());
    }

    #[test]
    fn a_non_zip_is_refused_rather_than_misparsed() {
        assert!(matches!(Archive::open(b"not a zip at all"), Err(ZipError::NotZip)));
    }

    #[test]
    fn slide_names_sort_numerically_so_a_deck_reads_in_order() {
        let mut names = vec!["ppt/slides/slide10.xml", "ppt/slides/slide2.xml", "ppt/slides/slide1.xml"];
        names.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(names[0], "ppt/slides/slide1.xml");
        assert_eq!(names[1], "ppt/slides/slide2.xml");
        assert_eq!(names[2], "ppt/slides/slide10.xml");
    }
}
