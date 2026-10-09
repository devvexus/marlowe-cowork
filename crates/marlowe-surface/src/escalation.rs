//! The escalation overlay — M3-DESIGN §3.4 and §3.6, drawn.
//!
//! §3.6 calls this *"the highest-value display attack in the product"*, and the reason is precise:
//! every other surface shows untrusted text to a human who is **reading**; this one shows it to a
//! human **about to choose between labelled options**. A bidi override that swaps what a row reads
//! as versus what it is lands directly on a decision.
//!
//! # Three mechanisms, and none of them is a filter
//!
//! 1. **Reserved-first layout.** [`layout`] computes the terminate region from the **frame
//!    height** before the option list is given a single row, and sizes it to hold the whole label
//!    at the frame's width. SECURITY-AUDIT B3's defect is a modal sized from unwrapped content,
//!    which `ratatui` clips silently — on the row that is the escape hatch. Growth in
//!    `IrreversibleAct` or `survivors` cannot displace it because those regions are handed what is
//!    left, never the other way round.
//!
//! 2. **Unforgeable chrome.** The harness's row is prefixed with [`crate::chrome::QUOTE_RULE`], a
//!    Block-Elements glyph. [`crate::chrome::is_reserved`] covers Box-Drawing and Block-Elements as
//!    **ranges**, and [`crate::chrome::prepare_model_text`] substitutes every reserved codepoint in
//!    model text with `<U+XXXX>` — so an agent that writes the terminate label verbatim, prefix and
//!    all, gets `<U+258F> terminate this agent and the runs under it` in its own row while the
//!    harness's row keeps the glyph. **The forgery is visible rather than blocked**, which is the
//!    same trade `sanitize` makes: a stripped payload and a clean string must not be
//!    indistinguishable to the person deciding.
//!
//! 3. **Nothing to omit.** `EscalationView` has no field for the terminate row, so this function
//!    does not consult the producer about whether to draw it. See that type's module header.
//!
//! # What this file deliberately does not do
//!
//! It draws **no border**. `tests/b13_region_contract.rs` allowlists `region.rs` and `overlay.rs`
//! for `Block::bordered`, and adding a third entry to that allowlist to decorate a modal would
//! widen a guard for a decoration. The overlay is distinguished by its scrim and its reserved
//! glyph, neither of which a model can produce.

use marlowe_view::escalation::{EscalationView, IrreversibleAct, SourceEvidence};
use marlowe_view::TERMINATE_LABEL;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Widget};

use crate::chrome::{self, Ink, QUOTE_RULE};
use crate::theme::Theme;

/// The regions, in the order they are claimed.
///
/// **`terminate` is populated first and is never reduced.** Every other field is what was left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EscalationLayout {
    pub overlay: Rect,
    /// Claimed **first**, from the frame height, and wide enough to hold the whole label.
    pub terminate: Rect,
    pub header: Rect,
    /// Scrolls within itself. The one region that grows with what the agent supplied.
    pub options: Rect,
    pub cost: Rect,
}

/// How many rows a string of `chars` characters needs at `width`, wrapped on whole words.
fn wrapped_rows(s: &str, width: u16) -> u16 {
    if width == 0 {
        return 1;
    }
    let mut rows = 1u16;
    let mut used = 0usize;
    for word in s.split_whitespace() {
        let w = word.chars().count();
        let need = if used == 0 { w } else { used + 1 + w };
        if need > width as usize && used > 0 {
            rows = rows.saturating_add(1);
            used = w;
        } else {
            used = need;
        }
    }
    rows.max(1)
}

/// Wrap on whole words, at `width`. Harness-side, so the label the harness authored is the label
/// that reaches the screen — never a `ratatui` truncation of it.
fn wrap(s: &str, width: u16) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }
    let mut out: Vec<String> = Vec::new();
    let mut line = String::new();
    for word in s.split_whitespace() {
        let w = word.chars().count();
        let need = if line.is_empty() { w } else { line.chars().count() + 1 + w };
        if need > width as usize && !line.is_empty() {
            out.push(std::mem::take(&mut line));
            line.push_str(word);
        } else {
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
    }
    out.push(line);
    out
}

/// The harness's own row, prefix included. **One definition**: the drawing reads this and so does
/// every assertion about it.
pub fn terminate_row_text() -> String {
    format!("{QUOTE_RULE} {TERMINATE_LABEL}")
}

/// **The escape hatch is laid out before anything else, and the precedence when the frame is
/// small is stated rather than emergent.**
///
/// `terminate > options > header > cost`. A 10-row pane cannot show six cost rows and the
/// alternative — giving each region a fixed share — is what makes the bottom row leave the buffer.
/// Cost is the section that yields, because §3.5's accounting is useless to a human who cannot see
/// the control it is the cost *of*.
pub fn layout(area: Rect) -> EscalationLayout {
    // The overlay is the frame, inset by one column each side when there is room. It is NOT sized
    // from the content: that is B3's defect, and it is the whole reason this function takes an
    // area and not a view.
    let overlay = Rect {
        x: area.x + u16::from(area.width > 4),
        y: area.y,
        width: area.width.saturating_sub(2 * u16::from(area.width > 4)),
        height: area.height,
    };

    // ── 1. TERMINATE. Claimed first, sized to hold the whole label at this width. ──────────
    let t_rows = wrapped_rows(&terminate_row_text(), overlay.width).min(overlay.height.max(1));
    let terminate = Rect {
        x: overlay.x,
        y: overlay.y + overlay.height.saturating_sub(t_rows),
        width: overlay.width,
        height: t_rows,
    };

    let mut avail = overlay.height.saturating_sub(t_rows);
    let mut y = overlay.y;

    // ── 2. HEADER: who raised it, how loud, what kind, and A8 arm (b)'s sentence. ─────────
    // Three rows where there is room, two where there is not, none on a very short frame. The
    // sentence is the row that goes first, because it is the one piece of the header a MODEL
    // wrote.
    let h_rows = if avail >= 5 {
        3
    } else if avail >= 4 {
        2
    } else {
        0
    };
    let header = Rect { x: overlay.x, y, width: overlay.width, height: h_rows };
    y += h_rows;
    avail -= h_rows;

    // ── 3. COST: §3.5's accounting. Yields first when the frame is short. ──────────────────
    // At least two rows are held back for the option list, because an overlay showing a cost and
    // no choices is a dialog with nothing to decide.
    // `ROWS` is the cost's own six fields; the evidence line is the seventh row of this region
    // and is counted here rather than folded into `ROWS`, which would make that constant stop
    // describing the type it is on.
    let c_rows = marlowe_view::TerminationCost::ROWS as u16 + 1;
    let cost_rows = c_rows.min(avail.saturating_sub(2));
    let cost_y = overlay.y + overlay.height.saturating_sub(t_rows + cost_rows);
    let cost = Rect { x: overlay.x, y: cost_y, width: overlay.width, height: cost_rows };
    avail -= cost_rows;

    // ── 4. OPTIONS: what is left, and it scrolls. ─────────────────────────────────────────
    let options = Rect { x: overlay.x, y, width: overlay.width, height: avail };

    EscalationLayout { overlay, terminate, header, options, cost }
}

/// Draw it.
///
/// `first_option` is the scroll offset **into the agent's rows only**. There is deliberately no
/// scroll offset that can reach the terminate region: a scroll state that could move it is a
/// scroll state an agent could drive by supplying more options.
pub fn draw_escalation(
    view: &EscalationView,
    theme: &Theme,
    area: Rect,
    buf: &mut Buffer,
    first_option: usize,
) {
    // The scrim: a foreground rewrite, never a fill — §B13 asks for zero background fills.
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            if let Some(cell) = buf.cell_mut((x, y)) {
                let s = cell.style();
                cell.set_style(
                    s.remove_modifier(Modifier::BOLD)
                        .add_modifier(Modifier::DIM)
                        .fg(Ink::Dimmer.color(theme)),
                );
            }
        }
    }

    let l = layout(area);
    Clear.render(l.overlay, buf);

    if l.header.height > 0 {
        let mut head = vec![
            Line::from(Span::styled(
                format!("{} escalation from {}", view.severity.word(), view.raised_by),
                theme.bright(),
            )),
            Line::from(Span::styled(
                view.category.word().to_string(),
                Ink::Dim.style(theme),
            )),
        ];
        if let Some(sentence) = &view.sentence {
            // **Model bytes, through the model-text pipeline.** The first two rows are composed
            // from closed harness enums and a harness-derived name; this one is not, and it is the
            // only row of the header that goes through `prepare_model_text`.
            head.push(Line::from(Span::styled(
                chrome::prepare_model_text(sentence.as_str()).into_owned(),
                theme.normal(),
            )));
        }
        head.truncate(l.header.height as usize);
        Paragraph::new(head).render(l.header, buf);
    }

    if l.options.height > 0 {
        let rows: Vec<Line> = view
            .options()
            .iter()
            .enumerate()
            .skip(first_option)
            .map(|(i, o)| {
                // **The whole §3.6 mechanism, in one call.** `prepare_model_text` is
                // `marlowe_contract::text::sanitize_prose` followed by `chrome::mark_reserved`:
                // the first refuses the tag block and the BiDi overrides `ratatui` passes
                // through, the second substitutes every Box-Drawing and Block-Elements codepoint,
                // which is what makes an agent's copy of the harness row read `<U+258F>`.
                //
                // The label is already an `OptionLabel`, so it has been sanitized once at
                // construction. Running it again is not redundant belt-and-braces: this is the
                // pass that reserves the CHROME, and `OptionLabel` knows nothing about chrome.
                let index = format!("  {}  ", i + 1);
                Line::from(vec![
                    Span::styled(index, Ink::Accent.style(theme)),
                    Span::styled(
                        chrome::prepare_model_text(o.label.as_str()).into_owned(),
                        theme.normal(),
                    ),
                ])
            })
            .collect();
        Paragraph::new(rows).render(l.options, buf);
    }

    if l.cost.height > 0 {
        let mut rows = cost_rows(&view.cost);
        rows.push(evidence_row(&view.evidence));
        Paragraph::new(rows).render(l.cost, buf);
    }

    // ── The escape hatch. Last drawn, first laid out. ─────────────────────────────────────
    //
    // Harness-authored text, wrapped by the harness so no truncation can eat it, in a region
    // nothing above could have shrunk.
    let text: Vec<Line> = wrap(&terminate_row_text(), l.terminate.width)
        .into_iter()
        .map(|s| Line::from(Span::styled(s, Ink::Red.style(theme))))
        .collect();
    Paragraph::new(text).render(l.terminate, buf);
}

/// §3.5's accounting, **one row per [`marlowe_view::TerminationCost`] field, always six**.
///
/// A field with no row is instance #16, so the count is asserted against
/// [`marlowe_view::TerminationCost::ROWS`] and `every_termination_cost_field_is_rendered` mutates
/// each of the six and asserts the rendering changes. The evidence line is NOT here: it belongs to
/// [`marlowe_view::SourceEvidence`], and counting it as a cost row would make `ROWS` stop
/// describing the type it is a constant on.
pub fn cost_rows(c: &marlowe_view::TerminationCost) -> Vec<Line<'static>> {
    let irreversible = if c.irreversible.is_empty() {
        "nothing irreversible recorded".to_string()
    } else {
        c.irreversible
            .iter()
            .map(|a| match a {
                IrreversibleAct::FilesWritten { count } => format!("{count} files written"),
                IrreversibleAct::CommitsPushed { count } => format!("{count} commits pushed"),
                IrreversibleAct::MessagesSent { count } => format!("{count} messages sent"),
                IrreversibleAct::ProcessesRun { count } => format!("{count} processes run"),
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    let survivors = if c.survivors.is_empty() {
        "nothing survives it".to_string()
    } else {
        // **The row that keeps `TERMINATE_LABEL` honest.** `Control::cancel` is per-run and a
        // detached child outlives the cancel, so a label promising "everything under it" would be
        // a lie the harness tells on the one row it authors *because* the agent would lie.
        format!(
            "keeps running: {}",
            c.survivors
                .iter()
                .map(|s| format!("{} ({})", s.run, s.policy.plainly()))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    let rows = vec![
        Line::from(format!("  {} runs", c.runs)),
        Line::from(format!("  running for {}s", c.age_ms / 1_000)),
        Line::from(format!("  spent {}c", c.spend_micros_usd / 10_000)),
        Line::from(format!("  {} artifacts", c.artifacts)),
        Line::from(format!("  {irreversible}")),
        Line::from(format!("  {survivors}")),
    ];
    debug_assert_eq!(rows.len(), marlowe_view::TerminationCost::ROWS);
    rows
}

/// §3.6's *"show provenance, not just a caution"*, as one row.
pub fn evidence_row(e: &SourceEvidence) -> Line<'static> {
    Line::from(match e {
        SourceEvidence::NoExternalSources => "  no external sources read".to_string(),
        SourceEvidence::External { sources, most_recent } => format!(
            "  {sources} external sources; most recent {} {}s ago",
            most_recent.host.0,
            most_recent.fetched_ms_ago / 1_000
        ),
    })
}
