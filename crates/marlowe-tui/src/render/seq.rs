//! One "initializing mainframe" line, shared by the boot, the shutdown and the in-transcript
//! sequences: a health glyph, the label, leader dots, then the value.

use marlowe_surface::chrome::Ink;
use marlowe_surface::Theme;
use ratatui::text::{Line, Span};

use super::ink;
use crate::seq::SeqLine;
use crate::text;
use crate::wake::Health;

/// `●  label ····· value`. `label_w` is the label column, dots included; the value is cut to
/// `value_w` columns. A line with no value yet shows `…`.
pub fn line(s: &SeqLine, label_w: usize, value_w: usize, theme: &Theme) -> Line<'static> {
    let (glyph, gstyle) = match s.health {
        Health::Pending => ("○", ink(Ink::Dimmer, theme)),
        Health::Ok => ("●", ink(Ink::Green, theme)),
        Health::Degraded => ("●", ink(Ink::Amber, theme)),
        Health::Down => ("●", ink(Ink::Red, theme)),
    };
    let dots = "·".repeat(label_w.saturating_sub(s.label.len() + 2));
    let value = if s.value.is_empty() { "…".to_string() } else { s.value.clone() };
    let vstyle = match s.health {
        Health::Pending => ink(Ink::Dimmer, theme),
        Health::Ok => theme.normal(),
        Health::Degraded => ink(Ink::Amber, theme),
        Health::Down => ink(Ink::Red, theme),
    };
    Line::from(vec![
        Span::styled(format!("{glyph}  "), gstyle),
        Span::styled(format!("{} ", s.label), ink(Ink::Dim, theme)),
        Span::styled(format!("{dots} "), ink(Ink::Dimmer, theme)),
        Span::styled(text::truncate(&value, value_w), vstyle),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The boot's own line, as it was drawn before the helper was shared.
    fn boot_line(health: Health, label: &str, value: &str, label_w: usize, value_w: usize, theme: &Theme) -> Line<'static> {
        let (glyph, gstyle) = match health {
            Health::Pending => ("○", ink(Ink::Dimmer, theme)),
            Health::Ok => ("●", ink(Ink::Green, theme)),
            Health::Degraded => ("●", ink(Ink::Amber, theme)),
            Health::Down => ("●", ink(Ink::Red, theme)),
        };
        let dots = "·".repeat(label_w.saturating_sub(label.len() + 2));
        let value = if value.is_empty() { "…".to_string() } else { value.to_string() };
        let vstyle = match health {
            Health::Pending => ink(Ink::Dimmer, theme),
            Health::Ok => theme.normal(),
            Health::Degraded => ink(Ink::Amber, theme),
            Health::Down => ink(Ink::Red, theme),
        };
        Line::from(vec![
            Span::styled(format!("{glyph}  "), gstyle),
            Span::styled(format!("{label} "), ink(Ink::Dim, theme)),
            Span::styled(format!("{dots} "), ink(Ink::Dimmer, theme)),
            Span::styled(text::truncate(&value, value_w), vstyle),
        ])
    }

    #[test]
    fn the_shared_line_is_cell_for_cell_the_boots_own() {
        let theme = Theme::default_truecolor();
        for health in [Health::Pending, Health::Ok, Health::Degraded, Health::Down] {
            for (label, value) in [("memory", "390 beliefs · recall online"), ("daemon", ""), ("a very long label here", "x")] {
                for value_w in [4, 12, 40] {
                    let s = SeqLine { health, label: label.into(), value: value.into(), done: health != Health::Pending };
                    assert_eq!(line(&s, 13, value_w, &theme), boot_line(health, label, value, 13, value_w, &theme));
                }
            }
        }
    }

    #[test]
    fn each_health_has_its_own_palette_ink_and_no_background() {
        let theme = Theme::default_truecolor();
        let ink_of = |h| line(&SeqLine { health: h, label: "x".into(), value: "v".into(), done: true }, 13, 20, &theme).spans[0].style;
        let (ok, degraded, down, pending) = (ink_of(Health::Ok), ink_of(Health::Degraded), ink_of(Health::Down), ink_of(Health::Pending));
        let fgs = [ok.fg, degraded.fg, down.fg, pending.fg];
        for (i, a) in fgs.iter().enumerate() {
            assert!(a.is_some() && fgs.iter().enumerate().all(|(j, b)| i == j || a != b), "{fgs:?}");
        }
        for st in [ok, degraded, down, pending] {
            assert!(st.bg.is_none(), "no background fills");
        }
    }
}
