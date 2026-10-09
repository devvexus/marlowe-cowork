//! JSON-lines framing.

use std::io::{self, BufRead, Write};

use serde::de::DeserializeOwned;
use serde::Serialize;

/// Longest accepted line. A longer line is consumed and reported, never buffered whole.
pub const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;

/// A framing failure.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("i/o: {0}")]
    Io(#[from] io::Error),
    #[error("line exceeds {MAX_LINE_BYTES} bytes")]
    TooLong,
    #[error("line is not UTF-8")]
    NotUtf8,
    #[error("malformed frame: {0}")]
    Json(#[from] serde_json::Error),
}

impl FrameError {
    /// True when the stream is unusable (as opposed to one bad line).
    pub fn is_fatal(&self) -> bool {
        matches!(self, FrameError::Io(_))
    }
}

/// Writes `value` as one JSON line and flushes.
pub fn write_line<W: Write + ?Sized, T: Serialize + ?Sized>(w: &mut W, value: &T) -> io::Result<()> {
    let mut buf = serde_json::to_vec(value).map_err(io::Error::other)?;
    buf.push(b'\n');
    w.write_all(&buf)?;
    w.flush()
}

/// Reads JSON lines with a length cap.
///
/// Resumable: an I/O error (such as a read timeout) mid-line keeps the bytes read so far, and the
/// next call continues the same line.
pub struct LineReader<R> {
    inner: R,
    buf: Vec<u8>,
    max: usize,
    overflow: bool,
}

impl<R: BufRead> LineReader<R> {
    pub fn new(inner: R) -> Self {
        Self::with_limit(inner, MAX_LINE_BYTES)
    }

    pub fn with_limit(inner: R, max: usize) -> Self {
        Self { inner, buf: Vec::new(), max, overflow: false }
    }

    /// The next raw line without its newline; `Ok(None)` at end of stream. Blank lines are skipped.
    pub fn next_line(&mut self) -> Result<Option<String>, FrameError> {
        loop {
            loop {
                let chunk = match self.inner.fill_buf() {
                    Ok(c) => c,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    // Partial line (and overflow state) stay buffered for the next call.
                    Err(e) => return Err(e.into()),
                };
                if chunk.is_empty() {
                    if self.buf.is_empty() && !self.overflow {
                        return Ok(None);
                    }
                    break;
                }
                let (take, done) = match chunk.iter().position(|b| *b == b'\n') {
                    Some(i) => (i + 1, true),
                    None => (chunk.len(), false),
                };
                if !self.overflow {
                    if self.buf.len() + take > self.max + 1 {
                        self.overflow = true;
                        self.buf = Vec::new();
                    } else {
                        self.buf.extend_from_slice(&chunk[..take]);
                    }
                }
                self.inner.consume(take);
                if done {
                    break;
                }
            }
            if std::mem::take(&mut self.overflow) {
                self.buf.clear();
                return Err(FrameError::TooLong);
            }
            while matches!(self.buf.last(), Some(b'\n' | b'\r')) {
                self.buf.pop();
            }
            if self.buf.iter().all(|b| b.is_ascii_whitespace()) {
                self.buf.clear();
                continue;
            }
            return String::from_utf8(std::mem::take(&mut self.buf))
                .map(Some)
                .map_err(|_| FrameError::NotUtf8);
        }
    }

    /// The next decoded frame; `Ok(None)` at end of stream.
    pub fn next<T: DeserializeOwned>(&mut self) -> Result<Option<T>, FrameError> {
        match self.next_line()? {
            Some(line) => Ok(Some(serde_json::from_str(&line)?)),
            None => Ok(None),
        }
    }

    pub fn get_ref(&self) -> &R {
        &self.inner
    }

    pub fn into_inner(self) -> R {
        self.inner
    }
}
