//! §B9's approval overlay. **The one place a border is drawn outside the region contract.**
//!
//! §B2 says every bordered region carries a hotkey on its bottom border, and `region.rs` makes
//! that unconstructable otherwise. This overlay is bordered and does not: its keys are stated in
//! its body, because it is modal and there is nothing to *jump focus to*.
//!
//! It lives in its own file so `tests/b13_region_contract.rs` can allowlist **this file** rather
//! than `render.rs`. Exempting the whole drawing module would blind the grep to the one place a
//! stray decorative border would actually appear — the same argument `determinism_guard.rs` makes
//! for refusing directory-level exemptions.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph, Widget};

use crate::app::App;
use crate::chrome::Ink;
use crate::theme::Theme;

fn inner(r: Rect) -> Rect {
    Rect {
        x: r.x + 1,
        y: r.y + 1,
        width: r.width.saturating_sub(2),
        height: r.height.saturating_sub(2),
    }
}

/// §B9. **In a design where every region is bordered, a border no longer signals modality.**
///
/// So the overlay dims the entire frame behind it, uses a doubled border in its risk tier's
/// colour, and centres itself. It is the only element permitted to dim the rest of the screen.
///
/// **The dim is a foreground rewrite, never a fill.** That is not a stylistic choice — §B13 asks
/// for zero background fills and `tests/no_background_fill.rs` walks every cell with the overlay
/// up. A `bg`-based scrim would fail it, and would also be the full-cell repaint §B12 forbids.
///
/// **The scrim colour was a hard-coded `Color::DarkGray` until M3 F2**, in all three overlays. It
/// is `Ink::Dimmer` now — foreground weight 3, the palette's own "recede so the eye goes
/// elsewhere" — which is what it was already trying to be. A literal here is a colour chosen
/// outside the palette: it does not move with `MARLOWE_ACCENT`, it is grey on a screen whose
/// recessive tone is violet-tinted, and it is invisible to every check that reads the palette.
/// `tests/palette_subset.rs` walks a buffer with each overlay up and refuses a colour it cannot
/// name — which is how this one was found.
pub fn draw_approval(app: &App, theme: &Theme, area: Rect, buf: &mut Buffer) {
    let Some(radius) = &app.view().approval else {
        return;
    };

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

    let w = 64.min(area.width.saturating_sub(4));
    let lines: Vec<Line> = vec![
        Line::from(Span::styled(radius.headline(), theme.bright())),
        Line::from(""),
        Line::from(Span::styled(
            radius.consequence(),
            Ink::of_tone(radius.tier.tone()).style(theme),
        )),
        Line::from(""),
        Line::from(Span::styled(radius.why(), Ink::Dim.style(theme))),
        Line::from(""),
        Line::from(
            radius
                .keys()
                .iter()
                .flat_map(|(k, what)| {
                    let key = match k {
                        '\n' => "↵".to_string(),
                        '\u{1b}' => "esc".to_string(),
                        c => c.to_string(),
                    };
                    [
                        Span::styled(
                            format!("{key} "),
                            Ink::of_tone(radius.tier.tone()).style(theme),
                        ),
                        Span::styled(format!("{what}    "), theme.normal()),
                    ]
                })
                .collect::<Vec<_>>(),
        ),
    ];
    let h = lines.len() as u16 + 2;
    let overlay = Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h.min(area.height),
    };
    Clear.render(overlay, buf);
    let tier = Ink::of_tone(radius.tier.tone()).style(theme);
    let block = Block::bordered()
        .border_type(BorderType::Double)
        .border_style(tier)
        .title(Span::styled("approval", tier));
    let text = inner(overlay);
    block.render(overlay, buf);
    Paragraph::new(lines).render(text, buf);
}

/// The **live** approval window: yes, no, or no-with-a-reason.
///
/// # Why this is not `draw_approval`
///
/// That one renders §B9's [`marlowe_view::approval::BlastRadius`], which the live path cannot
/// build: `Effect` has no fetch variant and `Ceiling` has no producer until the trust ledger at
/// M6. See `PendingApproval`'s header. Rather than invent an effect and a ceiling to reuse the
/// richer overlay, this draws what the permission layer actually knows and **prints what it does
/// not** — `ceiling unknown` is on screen, not omitted.
///
/// **There is no dismiss key.** The daemon is blocked on the answer, so a window that could be
/// closed without sending one would hang the turn with nothing explaining why.
pub fn draw_pending_approval(app: &App, theme: &Theme, area: Rect, buf: &mut Buffer) {
    let Some(p) = &app.view().pending_approval else {
        return;
    };

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

    // **Wider, and it WRAPS.** The first version was a fixed 64 with no wrap, so
    // `web · https://a-long-host.example.com/some/path` lost its tail off the right edge — and
    // the scope line is the one thing the user is actually deciding about. Silently truncating
    // it is the same defect as `blast_radius` dropping a target it could not stringify, one
    // layer up: an approval prompt that does not show the whole target is a prompt you cannot
    // trust, and it looks complete either way.
    let w = 76.min(area.width.saturating_sub(4));
    let mut lines: Vec<Line> = vec![
        Line::from(Span::styled(p.headline(), theme.bright())),
        Line::from(""),
        Line::from(Span::styled(p.detail(), Ink::Dim.style(theme))),
        Line::from(""),
    ];

    match &app.decline_reason {
        // Typing a reason. The cursor is a block so it is obvious the window is taking input
        // rather than waiting on one of the three keys.
        Some(text) => {
            lines.push(Line::from(Span::styled("declining — why?", theme.normal())));
            lines.push(Line::from(Span::styled(format!("{text}\u{2588}"), theme.bright())));
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled(format!("{} ", crate::chrome::KEYCAP_ENTER), theme.bright()),
                Span::styled("send    ", theme.normal()),
                Span::styled("esc ", theme.bright()),
                Span::styled("back to the question", theme.normal()),
            ]));
        }
        None => lines.push(Line::from(vec![
            Span::styled("y ", theme.bright()),
            Span::styled("yes    ", theme.normal()),
            Span::styled("n ", theme.bright()),
            Span::styled("no    ", theme.normal()),
            Span::styled("o ", theme.bright()),
            Span::styled("other — no, with a reason", theme.normal()),
        ])),
    }

    let h = lines.len() as u16 + 2;
    let overlay = Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h.min(area.height),
    };
    Clear.render(overlay, buf);
    // Amber, not red: this is a question, not a failure. Red is for a conflict or something
    // irreversible, and a fetch that has not happened yet is neither.
    let tone = if p.reversible { marlowe_view::Tone::Amber } else { marlowe_view::Tone::Red };
    let border = Ink::of_tone(tone).style(theme);
    let block = Block::bordered()
        .border_type(BorderType::Double)
        .border_style(border)
        .title(Span::styled("approval", border));
    let text = inner(overlay);
    block.render(overlay, buf);
    Paragraph::new(lines)
        .wrap(ratatui::widgets::Wrap { trim: false })
        .render(text, buf);
}

/// **Cancelling a run, with the orphan policy stated plainly.** `M3-DESIGN.md` §6.2.
///
/// # Why this is modal at all, when a window has a `^x` in its footer
///
/// §6.5 says closing a window **detaches** and never cancels; `^x` is the one key in a run window
/// that stops work. A key that ends a run and a key that closes a window sitting one row apart with
/// no confirmation between them is a mis-key that costs somebody an hour, so this is the wall.
///
/// # Three facts, and each one is here because the alternative is a confirmation nobody read
///
/// 1. **What happens to the children**, in a sentence rather than a variant name.
///    `OrphanPolicyLabel::plainly` is the single definition; "Detach" is not plain, and a policy
///    the user did not understand is a policy they clicked through.
/// 2. **The cost, from the harness's own record** — how long it has run, and what it has spent.
///    §3.5 makes the same argument about TERMINATE: those are facts the harness holds and they are
///    exactly what a compromised agent would lie about. Nothing here is asked of the run.
/// 3. **What cancelling does not undo.** Killing a run is clean; files it wrote and mail it sent
///    are not. Saying so is the difference between an honest confirmation and a reassuring one.
///
/// **No byte of it comes from the run**, except the id — which is a hex short form the daemon
/// assigns. There is no arm here that interpolates model text, which is the property §3.4 asks for
/// where it matters most.
pub fn draw_window_cancel(
    app: &crate::window::WindowApp,
    theme: &Theme,
    area: Rect,
    buf: &mut Buffer,
) {
    use marlowe_view::run::{elapsed, micros_usd};

    let v = app.view();

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

    let lines: Vec<Line> = vec![
        Line::from(Span::styled(format!("cancel run {}?", v.id), theme.bright())),
        Line::from(""),
        Line::from(Span::styled(v.orphan_policy.plainly(), theme.normal())),
        Line::from(Span::styled(
            format!(
                "it has run for {} and spent {}",
                elapsed(v.elapsed_ms),
                micros_usd(v.spend_micros_usd)
            ),
            Ink::Dim.style(theme),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "anything it already wrote, sent or pushed stays written, sent and pushed.",
            Ink::Dim.style(theme),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled(format!("{} ", crate::chrome::KEYCAP_ENTER), theme.bright()),
            Span::styled("cancel it    ", theme.normal()),
            Span::styled("esc ", theme.bright()),
            Span::styled("leave it running", theme.normal()),
        ]),
    ];

    let w = 68.min(area.width.saturating_sub(4));
    let h = lines.len() as u16 + 2;
    let overlay = Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h.min(area.height),
    };
    Clear.render(overlay, buf);
    // Red: ending a run is irreversible in the sense §B9 means it — the work already done is not
    // coming back. Amber would be a question; this is a consequence.
    let border = Ink::Red.style(theme);
    let block = Block::bordered()
        .border_type(BorderType::Double)
        .border_style(border)
        .title(Span::styled("cancel", border));
    let text = inner(overlay);
    block.render(overlay, buf);
    Paragraph::new(lines)
        .wrap(ratatui::widgets::Wrap { trim: false })
        .render(text, buf);
}
