//! The transcript's laid-out rows, kept between frames.
//!
//! Laying out a reply (markdown, wrapping, LaTeX) is the expensive part of a frame, and almost all
//! of it is the same as the frame before: while a reply streams, one entry changes. Each entry's
//! rows are stored under a key of everything they depend on; a frame lays out again from the first
//! entry whose key changed (later rows moved) and reuses the rest. A width or theme change clears
//! it. `State::no_layout_cache` lays every entry out every frame, which is what the tests compare
//! the cache against, cell for cell.

use marlowe_surface::markdown::MathBlock;
use marlowe_surface::Theme;
use ratatui::text::Line;

use crate::state::{Entry, Mark, NoteTone, RowHit, ToolStatus};

/// Where an entry's rows end in the cache's three lists.
#[derive(Debug, Clone)]
struct Rec {
    key: u64,
    lines: usize,
    maths: usize,
    marks: usize,
    hits: usize,
}

#[derive(Debug, Clone, Default)]
pub struct TranscriptCache {
    width: usize,
    theme: Option<Theme>,
    recs: Vec<Rec>,
    /// Every entry's rows, gaps included, in order.
    pub lines: Vec<Line<'static>>,
    /// The display formulas in them, by line.
    pub maths: Vec<MathBlock>,
    /// The reasoning blocks' fold markers, by line.
    pub marks: Vec<Mark>,
    /// The click targets on the rows (a tool line, a file's name), by line.
    pub hits: Vec<RowHit>,
    /// Entries laid out by the last frame (the rest came from here).
    pub laid: usize,
}

impl TranscriptCache {
    /// Clears the cache when the width or theme changed, or `off` asks for every frame from scratch.
    pub fn reset_unless(&mut self, width: usize, theme: &Theme, off: bool) {
        if off || self.width != width || self.theme.as_ref() != Some(theme) {
            self.recs.clear();
            self.lines.clear();
            self.maths.clear();
            self.marks.clear();
            self.hits.clear();
            self.width = width;
            self.theme = Some(theme.clone());
        }
    }

    /// The transcript line after entry `idx`'s last row.
    pub fn entry_end(&self, idx: usize) -> Option<usize> {
        self.recs.get(idx).map(|r| r.lines)
    }

    pub fn key_at(&self, idx: usize) -> Option<u64> {
        self.recs.get(idx).map(|r| r.key)
    }

    /// Keeps the first `idx` entries and drops the rest.
    pub fn cut(&mut self, idx: usize) {
        let (l, m, k, h) = match idx.checked_sub(1).and_then(|i| self.recs.get(i)) {
            Some(r) => (r.lines, r.maths, r.marks, r.hits),
            None => (0, 0, 0, 0),
        };
        self.recs.truncate(idx);
        self.lines.truncate(l);
        self.maths.truncate(m);
        self.marks.truncate(k);
        self.hits.truncate(h);
    }

    /// The lists a new entry's rows are appended to.
    pub fn parts(&mut self) -> (&mut Vec<Line<'static>>, &mut Vec<MathBlock>, &mut Vec<Mark>, &mut Vec<RowHit>) {
        (&mut self.lines, &mut self.maths, &mut self.marks, &mut self.hits)
    }

    /// Closes the entry just appended, under its key.
    pub fn seal(&mut self, key: u64) {
        self.recs.push(Rec { key, lines: self.lines.len(), maths: self.maths.len(), marks: self.marks.len(), hits: self.hits.len() });
    }
}

/// A fast 64-bit hash (Fx): keys are compared within one process and one frame to the next, so it
/// need not resist anything, only be cheap over a long transcript.
#[derive(Default)]
pub struct Hasher(u64);

const K: u64 = 0x517c_c1b7_2722_0a95;

impl Hasher {
    pub fn u64(&mut self, v: u64) {
        self.0 = (self.0.rotate_left(5) ^ v).wrapping_mul(K);
    }

    pub fn str(&mut self, s: &str) {
        let b = s.as_bytes();
        let mut chunks = b.chunks_exact(8);
        for c in &mut chunks {
            self.u64(u64::from_le_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]]));
        }
        let mut tail = [0u8; 8];
        let rest = chunks.remainder();
        tail[..rest.len()].copy_from_slice(rest);
        self.u64(u64::from_le_bytes(tail));
        self.u64(b.len() as u64);
    }

    pub fn finish(&self) -> u64 {
        self.0
    }

    /// Everything about an entry that its rows are made of.
    pub fn entry(&mut self, e: &Entry) {
        match e {
            Entry::User { text, steer } => {
                self.u64(1 | u64::from(*steer) << 8);
                self.str(text);
            }
            Entry::Prose { text } => {
                self.u64(2);
                self.str(text);
            }
            Entry::Thinking { text, tokens, live, ms, read } => {
                self.u64(3 | u64::from(*live) << 8);
                self.u64(*tokens);
                self.u64(*ms);
                self.str(text);
                if let Some(r) = read {
                    self.str(&format!("{r:?}"));
                }
            }
            Entry::Tool { call_id, summary, status } => {
                self.u64(4);
                self.str(call_id);
                self.str(summary);
                match status {
                    ToolStatus::Running => self.u64(0),
                    ToolStatus::Done(s) => {
                        self.u64(1);
                        self.str(s);
                    }
                    ToolStatus::Failed(s) => {
                        self.u64(2);
                        self.str(s);
                    }
                }
            }
            Entry::Note { text, tone } => {
                self.u64(5 | (match tone {
                    NoteTone::Dim => 0,
                    NoteTone::Amber => 1,
                    NoteTone::Red => 2,
                }) << 8);
                self.str(text);
            }
            Entry::Recall { count, summary, timing } => {
                self.u64(6 | u64::from(*count) << 8);
                self.str(summary);
                if let Some(t) = timing {
                    self.str(&format!("{t:?}"));
                }
            }
            Entry::Job { id, command, output, code } => {
                self.u64(7 | (code.map_or(0, |c| i64::from(c) + 1) as u64) << 8);
                self.str(id);
                self.str(command);
                self.str(output);
            }
            Entry::News { label, text } => {
                self.u64(8);
                self.str(label);
                self.str(text);
            }
            // Its rows are the project's: the renderer adds the version of its state to the key.
            Entry::Project { id } => {
                self.u64(10);
                self.str(id);
            }
            // Rare and small: their debug text is every field.
            Entry::Usage { .. } | Entry::Brief { .. } | Entry::File { .. } | Entry::Timings(_) | Entry::Seq(_) => {
                self.u64(9);
                self.str(&format!("{e:?}"));
            }
        }
    }
}
