//! Shared rig for the surface suite.
//!
//! Every test here renders into a `TestBackend` buffer at a chosen size. That is possible only
//! because the surface reads no clock and holds no producer: a render is a pure function of its
//! arguments, which is what turns "zero flicker" from an opinion into a cell count.
//!
//! There is deliberately no application in this crate any more, so a [`Pane`] stands in for the
//! conversation: a list of [`marlowe_view::Entry`] laid out through the same `markdown::render_prose`
//! the product uses, and drawn into a `Buffer`. It has no borders, no scrollbar and no regions --
//! those belong to whichever layout is built on this crate -- so assertions here are about the
//! text and its styling, not about where a layout puts it.

// Each test binary compiles this module and uses a different subset of it.
#![allow(dead_code)]

use marlowe_surface::chrome::Ink;
use marlowe_surface::markdown;
use marlowe_surface::Theme;
use marlowe_view::notice::Speech;
use marlowe_view::run::{CheckpointView, OrphanPolicyLabel, RunState, RunView};
use marlowe_view::Entry;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use ratatui::Terminal;

/// The sizes the flicker rows are verified across. The low end is the minimum the product
/// supports; the high end is a 4K-ish terminal at a small font.
pub const SIZES: [(u16, u16); 5] = [(120, 30), (140, 40), (160, 45), (200, 50), (240, 60)];

pub fn theme() -> Theme {
    Theme::default_truecolor()
}

/// A conversation reduced to its entries.
pub struct Pane {
    pub entries: Vec<Entry>,
}

impl Pane {
    pub fn new(entries: Vec<Entry>) -> Self {
        Self { entries }
    }

    /// One model reply and nothing else.
    pub fn saying(text: &str) -> Self {
        Self::new(vec![Entry::Said(Speech::Model(text.to_string()))])
    }

    /// One user message and nothing else -- the **flat** path, which has no sanitiser in front of
    /// it, so what happens to a character there is a statement about ratatui.
    pub fn user(text: &str) -> Self {
        Self::new(vec![Entry::User(text.to_string())])
    }
}

/// The product's former flat-path wrap, copied verbatim: it splits on `split_whitespace()`, which
/// drops U+2028 before ratatui sees it. Kept identical so flat-path behaviour is unchanged.
fn wrap(text: &str, w: usize) -> Vec<String> {
    let mut out = Vec::new();
    for para in text.split('\n') {
        let mut line = String::new();
        for word in para.split_whitespace() {
            let extra = if line.is_empty() { 0 } else { 1 };
            if line.chars().count() + extra + word.chars().count() > w && !line.is_empty() {
                out.push(std::mem::take(&mut line));
            }
            // A word longer than the pane is broken rather than left to overflow.
            let mut rest = word;
            while w > 0 && rest.chars().count() > w {
                let cut = rest.char_indices().nth(w).map_or(rest.len(), |(i, _)| i);
                if !line.is_empty() {
                    out.push(std::mem::take(&mut line));
                }
                out.push(rest[..cut].to_string());
                rest = &rest[cut..];
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(rest);
        }
        out.push(line);
    }
    out
}

/// The pane's lines, wrapped to `width`: model prose through the markdown renderer in Marlowe's
/// voice, user text flat in the terminal's own foreground. Each entry is followed by a blank line.
pub fn lines(pane: &Pane, theme: &Theme, width: u16) -> Vec<Line<'static>> {
    let w = width.max(20) as usize;
    let mut out: Vec<Line<'static>> = Vec::new();
    for entry in &pane.entries {
        match entry {
            Entry::User(text) => {
                for l in wrap(text, w) {
                    out.push(Line::from(Span::styled(l, Style::default())));
                }
                out.push(Line::from(""));
            }
            Entry::Said(Speech::Model(t)) => {
                out.extend(markdown::render_prose(t, w, theme, Ink::Speech.style(theme)));
                out.push(Line::from(""));
            }
            other => panic!("the test pane does not draw {other:?}"),
        }
    }
    out
}

/// Draw the pane into `area`, leaving a two-column gutter on the right as the product's scroll
/// area does.
pub fn draw(pane: &Pane, theme: &Theme, area: Rect, buf: &mut Buffer) {
    let text_area = Rect { width: area.width.saturating_sub(2), ..area };
    Paragraph::new(lines(pane, theme, text_area.width)).render(text_area, buf);
}

/// Render one frame and hand back the buffer.
pub fn frame(pane: &Pane, w: u16, h: u16) -> Buffer {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| draw(pane, &theme(), f.area(), f.buffer_mut())).unwrap();
    term.backend().buffer().clone()
}

/// A persistent terminal, so the *next* draw is a real differential update.
pub fn terminal(w: u16, h: u16) -> Terminal<TestBackend> {
    Terminal::new(TestBackend::new(w, h)).unwrap()
}

pub fn draw_into(term: &mut Terminal<TestBackend>, pane: &Pane) -> Buffer {
    term.draw(|f| draw(pane, &theme(), f.area(), f.buffer_mut())).unwrap();
    term.backend().buffer().clone()
}

/// Cells that differ between two buffers of the same size.
pub fn diff_cells(a: &Buffer, b: &Buffer) -> Vec<(u16, u16)> {
    let mut out = Vec::new();
    for y in 0..a.area.height {
        for x in 0..a.area.width {
            let (pa, pb) = (a.cell((x, y)), b.cell((x, y)));
            if pa != pb {
                out.push((x, y));
            }
        }
    }
    out
}

/// Every symbol in a buffer row, for tests that assert on what is drawn where.
pub fn row_text(buf: &Buffer, y: u16) -> String {
    (0..buf.area.width)
        .map(|x| buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" ").to_string())
        .collect()
}

pub fn buffer_text(buf: &Buffer) -> String {
    (0..buf.area.height).map(|y| row_text(buf, y)).collect::<Vec<_>>().join("\n")
}

/// The text inside one rect, and **only** that rect. Search the region, not the whole buffer: a
/// whole-buffer `contains` can match chrome instead of prose and go green for the wrong reason.
pub fn region_text(buf: &Buffer, r: Rect) -> String {
    (r.y..r.bottom())
        .map(|y| {
            (r.x..r.right())
                .map(|x| buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" ").to_string())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A run that is running, with a ceiling and no children. **No `Default` anywhere near this** --
/// [`RunView`] deliberately has none, so every test states the run it is talking about.
pub fn run_view() -> RunView {
    RunView {
        id: "a1b2c3d4".into(),
        // The name the daemon derives from the FULL id; `sayable` on this fixture's uuid.
        name: "daring-storm".into(),
        state: RunState::Running,
        parent: None,
        // 1m 33s, so an elapsed assertion is about a formatted figure rather than about zero.
        elapsed_ms: 93_000,
        spend_micros_usd: 120_000,
        ceiling_micros_usd: 3_000_000,
        spent_tokens: 400,
        granted_tokens: 50_000,
        depth: 0,
        checkpoint: CheckpointView { last_completed: Some(41), resumable: true },
        orphan_policy: OrphanPolicyLabel::Detach,
        pending_steers: 0,
        output: Vec::new(),
        subagents: Vec::new(),
        budget: Vec::new(),
        scope_memory: Vec::new(),
        meetings: Vec::new(),
    }
}
