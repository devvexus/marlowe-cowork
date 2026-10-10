//! A text file as the edit tools see it. Decoding remembers the encoding (UTF-8, UTF-8 with a
//! BOM, UTF-16 with a BOM) so a write puts back exactly what it found; the dominant line ending
//! lets new text follow the file's convention.

/// How a file's bytes encode its text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Utf8,
    Utf8Bom,
    Utf16Le,
    Utf16Be,
}

/// A file's line-ending convention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Eol {
    Lf,
    Crlf,
}

impl Eol {
    pub fn as_str(self) -> &'static str {
        match self {
            Eol::Lf => "\n",
            Eol::Crlf => "\r\n",
        }
    }
}

/// Decoded text plus what is needed to encode it the same way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextDoc {
    pub encoding: Encoding,
    /// The dominant ending (CRLF only when CRLF lines outnumber bare LF lines).
    pub eol: Eol,
    /// The text without its BOM.
    pub text: String,
}

impl TextDoc {
    /// A new UTF-8 document with LF endings unless `text` is mostly CRLF.
    pub fn utf8(text: impl Into<String>) -> Self {
        let text = text.into();
        Self { encoding: Encoding::Utf8, eol: dominant_eol(&text), text }
    }

    /// Decodes `bytes`; `None` when they are not text this tool can round-trip.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let (encoding, text) = if let Some(rest) = bytes.strip_prefix(b"\xEF\xBB\xBF") {
            (Encoding::Utf8Bom, String::from_utf8(rest.to_vec()).ok()?)
        } else if let Some(rest) = bytes.strip_prefix(b"\xFF\xFE") {
            (Encoding::Utf16Le, decode_utf16(rest, u16::from_le_bytes)?)
        } else if let Some(rest) = bytes.strip_prefix(b"\xFE\xFF") {
            (Encoding::Utf16Be, decode_utf16(rest, u16::from_be_bytes)?)
        } else {
            (Encoding::Utf8, String::from_utf8(bytes.to_vec()).ok()?)
        };
        Some(Self { encoding, eol: dominant_eol(&text), text })
    }

    /// The bytes for `text` in this document's encoding.
    pub fn encode_text(&self, text: &str) -> Vec<u8> {
        match self.encoding {
            Encoding::Utf8 => text.as_bytes().to_vec(),
            Encoding::Utf8Bom => [b"\xEF\xBB\xBF".as_slice(), text.as_bytes()].concat(),
            Encoding::Utf16Le => {
                let mut out = vec![0xFF, 0xFE];
                text.encode_utf16().for_each(|u| out.extend_from_slice(&u.to_le_bytes()));
                out
            }
            Encoding::Utf16Be => {
                let mut out = vec![0xFE, 0xFF];
                text.encode_utf16().for_each(|u| out.extend_from_slice(&u.to_be_bytes()));
                out
            }
        }
    }

    /// The bytes for this document.
    pub fn encode(&self) -> Vec<u8> {
        self.encode_text(&self.text)
    }

    pub fn has_bom(&self) -> bool {
        self.encoding != Encoding::Utf8
    }
}

fn decode_utf16(bytes: &[u8], word: fn([u8; 2]) -> u16) -> Option<String> {
    if bytes.len() % 2 != 0 {
        return None;
    }
    let units = bytes.chunks_exact(2).map(|c| word([c[0], c[1]]));
    char::decode_utf16(units).collect::<Result<String, _>>().ok()
}

/// CRLF when CRLF lines outnumber bare-LF lines, else LF.
pub fn dominant_eol(text: &str) -> Eol {
    let crlf = text.matches("\r\n").count();
    let lf = text.matches('\n').count() - crlf;
    if crlf > lf {
        Eol::Crlf
    } else {
        Eol::Lf
    }
}

/// Whether every `\n` in `text` is part of a CRLF (and there is at least one).
pub fn uniformly_crlf(text: &str) -> bool {
    let crlf = text.matches("\r\n").count();
    crlf > 0 && crlf == text.matches('\n').count()
}

/// `text` with every line ending (CRLF or LF) rewritten as `eol`. Lone CRs are kept.
pub fn to_eol(text: &str, eol: Eol) -> String {
    let lf = text.replace("\r\n", "\n");
    match eol {
        Eol::Lf => lf,
        Eol::Crlf => lf.replace('\n', "\r\n"),
    }
}

/// `content` as it should land over a file that was `existing` (`None`: a new file): a BOM the
/// model copied in is dropped (the encoding puts it back), and LF-only text written over a
/// CRLF file takes CRLF.
pub fn conform(existing: Option<&TextDoc>, content: &str) -> TextDoc {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    match existing {
        None => TextDoc::utf8(content),
        Some(doc) => {
            let text = if doc.eol == Eol::Crlf && !content.contains("\r\n") && content.contains('\n') {
                to_eol(content, Eol::Crlf)
            } else {
                content.to_string()
            };
            TextDoc { encoding: doc.encoding, eol: dominant_eol(&text), text }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_encoding_round_trips_byte_for_byte() {
        let samples: Vec<Vec<u8>> = vec![
            b"plain\n".to_vec(),
            b"\xEF\xBB\xBFbom\r\n".to_vec(),
            TextDoc { encoding: Encoding::Utf16Le, eol: Eol::Lf, text: "w\u{e9}\n".into() }.encode(),
            TextDoc { encoding: Encoding::Utf16Be, eol: Eol::Lf, text: "x\n".into() }.encode(),
        ];
        for bytes in samples {
            let doc = TextDoc::decode(&bytes).unwrap();
            assert_eq!(doc.encode(), bytes);
        }
        assert!(TextDoc::decode(&[0xff, 0x00, 0x80]).is_none());
        assert!(TextDoc::decode(b"\xFF\xFEa").is_none(), "odd UTF-16 length");
    }

    #[test]
    fn conform_keeps_bom_and_crlf() {
        let old = TextDoc::decode(b"\xEF\xBB\xBFa\r\nb\r\n").unwrap();
        let new = conform(Some(&old), "\u{feff}x\ny\n");
        assert_eq!(new.encode(), b"\xEF\xBB\xBFx\r\ny\r\n");
        assert_eq!(conform(None, "x\n").encode(), b"x\n");
        assert_eq!(dominant_eol("a\r\nb\nc\r\n"), Eol::Crlf);
        assert!(!uniformly_crlf("a\r\nb\n"));
    }
}
