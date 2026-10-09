//! `marlowe doctor` — the one-time honest check ADR-021 pays for braille with.
//!
//! **There is no way to detect whether a font renders U+2800–U+28FF.** The terminal reports no
//! glyph coverage; a missing glyph surfaces as tofu, a blank, or a double-width box, and none of
//! those are distinguishable from a correctly drawn dim frame by anything the program can measure.
//!
//! ADR-021 refuses to add a fallback, because a silent block-glyph fallback means two users see
//! two different indicators with nothing observing the divergence — the mismatch-hiding default
//! this project has shipped four bugs behind. So the check is given to the only instrument that
//! can actually read it: **the user's eye, once, at a moment they are looking.**
//!
//! The colour rows are the same idea. §B13's accent criterion is *verified by eye on each*
//! background, and a contrast ratio cannot replace that — but it can make sure the eye check
//! arrives with a number attached instead of as an unaided opinion.

use marlowe_view::{SessionView, LEVELS, SAMPLES};

use crate::theme::{self, ColorDepth, Theme, ACCENT_RGB};

/// A dark terminal background — the mockup's, and roughly what a default dark theme gives.
pub const REFERENCE_DARK: (u8, u8, u8) = (0x0f, 0x0e, 0x14);

/// A light terminal background. White is the worst case, which is the case worth measuring.
pub const REFERENCE_LIGHT: (u8, u8, u8) = (0xff, 0xff, 0xff);

/// The report, one line per row. Rendered by both surfaces, because `doctor` is in the one command
/// registry and §B11 gives it parity.
pub fn report(view: &SessionView) -> Vec<String> {
    let mut out = Vec::new();

    let (depth, why) = ColorDepth::detect(|k| std::env::var(k).ok());
    let theme = Theme::resolve(|k| std::env::var(k).ok(), None);

    out.push("terminal".into());
    match crossterm::terminal::size() {
        Ok((w, h)) => {
            let ok = w >= crate::app::MIN_COLS && h >= crate::app::MIN_ROWS;
            out.push(format!(
                "  size            {w}x{h}   required {}x{}   {}",
                crate::app::MIN_COLS,
                crate::app::MIN_ROWS,
                if ok { "ok" } else { "TOO SMALL — the TUI will refuse and offer --classic" }
            ));
        }
        Err(e) => out.push(format!("  size            unavailable ({e})")),
    }
    out.push(format!("  colour depth    {}   ({why})", depth.as_str()));
    match &theme {
        Ok(t) => out.push(format!("  accent          {}", t.accent_source())),
        Err(e) => out.push(format!("  accent          REFUSED — {e}")),
    }

    out.push(String::new());
    out.push("accent legibility — §B13 asks for an eye check on BOTH; these are the numbers behind it".into());
    // **The colour MEASURED is the colour RESOLVED.** Reporting `MARLOWE_ACCENT=#33CC88` on one
    // line and the default violet's contrast on the next would be this project's signature defect:
    // two sides silently disagreeing while the output looks fine.
    let measured = resolved_accent_rgb();
    if measured != ACCENT_RGB {
        out.push(format!(
            "  measuring #{:02X}{:02X}{:02X} — your override, not the default",
            measured.0, measured.1, measured.2
        ));
    }
    let dark = theme::contrast(measured, REFERENCE_DARK);
    let light = theme::contrast(measured, REFERENCE_LIGHT);
    out.push(format!(
        "  on dark  #0F0E14  {dark:.2}:1   {}",
        verdict(dark)
    ));
    out.push(format!(
        "  on light #FFFFFF  {light:.2}:1   {}",
        verdict(light)
    ));
    out.push(
        "  Violet is the accent most likely to fail the light background. If it is not comfortably"
            .into(),
    );
    out.push("  readable on yours, set MARLOWE_ACCENT=#RRGGBB — a malformed value is refused, not ignored.".into());

    out.push(String::new());
    out.push("braille — ADR-021. There is no way to probe font coverage, so this is the check.".into());
    out.push("  You should see a smooth ramp filling left to right, in two rows:".into());
    let mut ramp = [0u8; SAMPLES];
    for (i, c) in ramp.iter_mut().enumerate() {
        *c = ((i + 1) * LEVELS as usize / SAMPLES) as u8;
    }
    for row in crate::meter::rows(&ramp) {
        out.push(format!("    {row}"));
    }
    out.push("  and a full block, then an empty one:".into());
    let full = crate::meter::rows(&[LEVELS; SAMPLES]);
    let empty = crate::meter::rows(&[0; SAMPLES]);
    out.push(format!("    {}  {}", full[0], empty[0]));
    out.push(format!("    {}  {}", full[1], empty[1]));
    out.push(
        "  Boxes, blanks or question marks mean your terminal font lacks U+2800–U+28FF. Marlowe"
            .into(),
    );
    out.push(
        "  does NOT fall back to block characters: two users seeing two different indicators with"
            .into(),
    );
    out.push("  nothing observing it is worse than one honest failure. Use Cascadia Code, DejaVu".into());
    out.push("  Sans Mono, JetBrains Mono, Fira Code or any Nerd Font.".into());

    out.push(String::new());
    out.push("keys".into());
    match crate::keys::KeyRegistry::build(view) {
        Ok(reg) => out.push(format!(
            "  {} bindings, no conflicts",
            reg.all_keys().len()
        )),
        Err(e) => out.push(format!("  CONFLICT — {e}")),
    }

    out
}

/// The accent actually in force. Falls back to the default only when the variable is unset — a
/// *malformed* value is refused by `Theme::resolve` and reported on the line above, never silently
/// measured as the default.
fn resolved_accent_rgb() -> (u8, u8, u8) {
    match std::env::var("MARLOWE_ACCENT") {
        Ok(v) => parse_hex(&v).unwrap_or(ACCENT_RGB),
        Err(_) => ACCENT_RGB,
    }
}

fn parse_hex(s: &str) -> Option<(u8, u8, u8)> {
    let h = s.strip_prefix('#').unwrap_or(s);
    if h.len() != 6 || !h.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some((
        u8::from_str_radix(&h[0..2], 16).ok()?,
        u8::from_str_radix(&h[2..4], 16).ok()?,
        u8::from_str_radix(&h[4..6], 16).ok()?,
    ))
}

fn verdict(ratio: f64) -> &'static str {
    // WCAG's thresholds, named rather than assumed. They are a floor for *text*; the accent here
    // is mostly borders and labels, so 3.0 is the line that matters and 4.5 is comfortable.
    if ratio >= 4.5 {
        "comfortable (AA for body text)"
    } else if ratio >= 3.0 {
        "adequate (AA for large text and UI borders)"
    } else {
        "MARGINAL — check by eye, and consider MARLOWE_ACCENT"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_report_states_both_backgrounds_and_the_braille_row() {
        let out = report(marlowe_stub::Session::new().view()).join("\n");
        assert!(out.contains("on dark"));
        assert!(out.contains("on light"));
        assert!(out.contains('\u{28FF}'), "the full braille cell must be printed for the eye check");
    }

    #[test]
    fn the_shipped_accent_is_measured_and_the_numbers_are_recorded_here() {
        // Not a threshold gate — §B13's criterion is the eye. This pins the measurement so a
        // change to ACCENT_RGB has to move a number in a test rather than pass silently.
        let dark = theme::contrast(ACCENT_RGB, REFERENCE_DARK);
        let light = theme::contrast(ACCENT_RGB, REFERENCE_LIGHT);
        assert!(
            (dark - 5.8843).abs() < 0.01,
            "accent on dark measured {dark:.4}:1, was 5.8843"
        );
        // 3.2645 clears WCAG's 3.0 line for UI components and large text, and is BELOW the 4.5
        // line for body text. Labels and hotkeys are small text, so on a light terminal the accent
        // is legible rather than comfortable. Recorded as a number so §B13's eye check argues with
        // a measurement rather than with an impression.
        assert!(
            (light - 3.2645).abs() < 0.01,
            "accent on light measured {light:.4}:1, was 3.2645"
        );
    }
}
