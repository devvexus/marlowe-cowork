//! The palette. §B2: **one accent, three state colours, three foreground weights, and never a
//! background fill.**
//!
//! # There is no API here that can produce a background
//!
//! §B13 asks for zero background fills used to signal focus, and `tests/no_background_fill.rs`
//! walks every cell of a rendered buffer to assert it. That test is the backstop; this module is
//! the reason it passes. Nothing in [`Theme`] returns a `Style` with `bg` set, and the acceptance
//! suite greps this crate for `.bg(` and `Style::reset`. **The terminal's own background is the
//! background** — which is also why the accent has to survive both a dark and a light one.
//!
//! # Two load-time errors, both deliberate
//!
//! A malformed `MARLOWE_ACCENT` is a **refusal**, not a fall-back to the default violet. A user who
//! sets `MARLOWE_ACCENT=#gg7ede` and gets the default has been told nothing, and will conclude the
//! variable does not work. CLAUDE.md: *prefer a load-time error to a sensible default.*
//!
//! The colour-depth tier is **chosen by a documented rule, recorded in the startup record, and
//! overridable by an explicit flag.** §B2 requires the 256-colour and 16-colour fallbacks, so a
//! probe cannot be avoided — but a silent probe is the mismatch-hiding default this project has
//! shipped four bugs behind. Here the probe's answer is printed rather than assumed.

use marlowe_view::Tone;
use ratatui::style::{Color, Modifier, Style};

use crate::region::FocusLevel;

/// §B2's accent: matte violet.
pub const ACCENT_RGB: (u8, u8, u8) = (0x9B, 0x7E, 0xDE);

/// The 256-colour fallback named in §B2.
pub const ACCENT_ANSI256: u8 = 141;

/// How much colour the terminal can carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorDepth {
    /// 24-bit. The accent renders as its exact RGB.
    TrueColor,
    /// 256 colours. §B2's first fallback: ANSI 141.
    Ansi256,
    /// 16 colours. §B2's second fallback: magenta.
    Ansi16,
}

impl ColorDepth {
    /// The documented rule, in one place so the startup record can quote it.
    ///
    /// # This rule was too conservative and it cost a whole screenshot
    ///
    /// The first version demanded `COLORTERM`, `WT_SESSION`, or `256` in `TERM`, and fell to
    /// **16 colours** otherwise. A real xterm launched with the default `TERM=xterm` hit that
    /// floor, every `Color::Rgb` collapsed to its nearest ANSI-16 neighbour, and the violet accent
    /// rendered as bright magenta with the muted state colours as terminal green/yellow/red. The
    /// design was invisible and the mechanism was working exactly as written.
    ///
    /// The startup record said so — `color_depth 16`, reason `no COLORTERM, no WT_SESSION, TERM
    /// lacks 256`. **The record is only worth having if it is read**, which is the actual lesson.
    ///
    /// The rule now recognises the emulator families by name. Every one of them supports at least
    /// 256 colours and has for over a decade; demoting them to 16 was strictly wrong, not cautious.
    /// **The floor is still a floor** — an unrecognised `TERM` still gets 16, because guessing
    /// truecolor on a terminal that cannot do it produces garbage escape sequences rather than a
    /// slightly wrong colour.
    pub fn detect(env: impl Fn(&str) -> Option<String>) -> (Self, &'static str) {
        if let Some(ct) = env("COLORTERM") {
            if ct == "truecolor" || ct == "24bit" {
                return (ColorDepth::TrueColor, "COLORTERM=truecolor");
            }
        }
        if env("WT_SESSION").is_some() {
            return (ColorDepth::TrueColor, "WT_SESSION (Windows Terminal)");
        }
        let term = env("TERM").unwrap_or_default();
        // `-direct` terminfo entries are the standardised way to say 24-bit.
        if term.contains("direct") || term.contains("truecolor") {
            return (ColorDepth::TrueColor, "TERM names a direct-colour entry");
        }
        // Emulators that are 24-bit in every shipping version. Named individually rather than
        // assumed, so the list is auditable and an unknown terminal still lands on the floor.
        for (name, why) in [
            ("kitty", "TERM names kitty"),
            ("alacritty", "TERM names alacritty"),
            ("foot", "TERM names foot"),
            ("wezterm", "TERM names wezterm"),
            ("contour", "TERM names contour"),
        ] {
            if term.contains(name) {
                return (ColorDepth::TrueColor, why);
            }
        }
        if term.contains("256") {
            return (ColorDepth::Ansi256, "TERM contains 256");
        }
        for (name, why) in [
            ("xterm", "TERM is an xterm family entry (256-colour capable)"),
            ("screen", "TERM is a screen entry (256-colour capable)"),
            ("tmux", "TERM is a tmux entry (256-colour capable)"),
            ("rxvt", "TERM is an rxvt entry (256-colour capable)"),
            ("vte", "TERM is a vte entry (256-colour capable)"),
            ("konsole", "TERM is a konsole entry (256-colour capable)"),
            ("linux", "TERM is the Linux console"),
        ] {
            if term.contains(name) {
                return (ColorDepth::Ansi256, why);
            }
        }
        (
            ColorDepth::Ansi16,
            "TERM is unrecognised — falling to the 16-colour floor",
        )
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ColorDepth::TrueColor => "truecolor",
            ColorDepth::Ansi256 => "256",
            ColorDepth::Ansi16 => "16",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "truecolor" | "24bit" => Some(ColorDepth::TrueColor),
            "256" => Some(ColorDepth::Ansi256),
            "16" => Some(ColorDepth::Ansi16),
            _ => None,
        }
    }
}

/// A refusal to start, with the reason and the fix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThemeError {
    Accent { value: String },
    Depth { value: String },
}

impl std::fmt::Display for ThemeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ThemeError::Accent { value } => write!(
                f,
                "MARLOWE_ACCENT={value:?} is not a colour. Expected #RRGGBB, for example #9B7EDE. \
                 Refusing rather than falling back to the default accent: a fallback would leave \
                 you looking at the default and concluding the variable does not work."
            ),
            ThemeError::Depth { value } => write!(
                f,
                "--color-depth {value:?} is not one of truecolor, 256, 16."
            ),
        }
    }
}

impl std::error::Error for ThemeError {}

/// The resolved palette.
///
/// # The budget, and how the structural colours stay inside it
///
/// §B13: *≤ 1 accent + 3 state + 3 foreground weights.* §B2's table puts **structure** under the
/// accent role, so an unfocused border is not a new colour — it is the accent at low luminance,
/// and it is **derived from the accent by scaling** rather than being a second hard-coded violet.
/// `MARLOWE_ACCENT` therefore moves the borders with it, and there is provably one accent.
///
/// # Why the foreground weights are explicit colours and **not** SGR 2
///
/// The first version expressed *dim* as `Modifier::DIM` over the terminal's own foreground. **SGR 2
/// is widely unimplemented** — plenty of emulators ignore it entirely — and where it is ignored,
/// all three foreground weights collapse into one. §B2's dimming is load-bearing: an event needing
/// nothing from the user is dimmed to near-invisible *so the eye goes to the two that do*. A
/// no-op dim does not fail loudly; it just quietly stops telling the user where to look.
///
/// The second version kept the explicit values **and** added `DIM` on top, "for terminals that
/// honour it". That had the reasoning backwards, and it shipped a visible bug: an explicit
/// foreground colour is supported everywhere, and SGR 2 is the unreliable half — so the modifier
/// could only ever *double-apply* where it worked and do nothing where it did not. Windows Terminal
/// honours it, so `#4a4460` was rendered faint on top of already being the palette's darkest
/// value, and the third weight became genuinely unreadable rather than merely recessive. The
/// mockup, which has no SGR at all, looked lighter — and the mockup was right.
///
/// **So the weights are explicit colours and carry no modifier.** What the mockup shows is what the
/// terminal shows, on every terminal, which is also the only version of this that is checkable
/// against the reference by eye.
///
/// The remaining cost is named rather than absorbed: on a **light** terminal background these greys
/// read as ordinary text rather than as recessive, because dimming on light means going lighter,
/// not darker. §B13's legibility row covers the accent, not the weights.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theme {
    accent: Color,
    /// The accent at ~30% luminance. Unfocused borders. Derived, never hard-coded.
    structure: Color,
    /// The accent at ~18% luminance. Inactive borders.
    structure_dim: Color,
    /// The accent at ~72% luminance — the mockup's `--accent-dim: #6f5aa3`. Hovered borders and
    /// labels. Derived rather than hard-coded, so `MARLOWE_ACCENT` moves hover with it and there is
    /// still provably one accent.
    hover: Color,
    /// **Marlowe's own prose.** The mockup's `--violet: #b39ae8` — the accent tinted toward white.
    ///
    /// The user's words stay in the terminal's foreground (weight 1) and Marlowe's take this, which
    /// is the one place in the design where colour marks *who is speaking* rather than state. That
    /// is not a §B2 violation: it is the accent role, not a fourth state colour, and it is derived
    /// from the accent so `MARLOWE_ACCENT` carries the voice with it.
    ///
    /// White prose was the default and read as terminal output rather than as somebody talking —
    /// which is precisely the impression §B0 says the design exists to avoid.
    speech: Color,
    green: Color,
    amber: Color,
    red: Color,
    /// Foreground weight 2 of 3. Body text is weight 1 and is the terminal's own (`Reset`).
    dim: Color,
    /// Foreground weight 3 of 3.
    dimmer: Color,
    depth: ColorDepth,
    /// Why the depth is what it is. Printed by `--timing-probe` and `doctor`.
    depth_reason: String,
    accent_source: String,
    /// The observed value of `NO_COLOR`, if it is set to anything non-empty.
    ///
    /// # This field exists because its absence cost four rounds of screenshots
    ///
    /// `NO_COLOR` (<https://no-color.org/>) is honoured by crossterm at the *formatter* level: when
    /// it is set, `SetForegroundColor(..)` emits `ESC[m` — an empty SGR, which is a full reset —
    /// instead of the colour. Every cell is then painted in the terminal's default foreground.
    ///
    /// Nothing about that is detectable from this crate's own output. The frame renders, the
    /// layout is right, the styles are right, `ColorDepth::detect` correctly reports `truecolor`,
    /// and the screen is uniformly achromatic. **The startup record asserted a tier the process was
    /// not emitting, and the two disagreed silently.** That is CLAUDE.md's standing warning almost
    /// verbatim: a probe answering the wrong question makes a mismatch unobservable.
    ///
    /// The fix is not to stop honouring `NO_COLOR` — it is a legitimate user preference and
    /// overriding it silently would be the same sin inverted. The fix is that the record reports
    /// what will actually be *emitted* rather than what the terminal is *capable of*.
    no_color: Option<String>,
}

impl Theme {
    /// Resolve the palette from the environment and an optional explicit depth override.
    pub fn resolve(
        env: impl Fn(&str) -> Option<String>,
        depth_override: Option<&str>,
    ) -> Result<Self, ThemeError> {
        let (depth, reason) = match depth_override {
            Some(s) => (
                ColorDepth::parse(s).ok_or_else(|| ThemeError::Depth {
                    value: s.to_string(),
                })?,
                format!("--color-depth {s}"),
            ),
            None => {
                let (d, why) = ColorDepth::detect(&env);
                (d, why.to_string())
            }
        };

        let (accent_rgb, accent_source) = match env("MARLOWE_ACCENT") {
            Some(v) => (
                parse_hex(&v).ok_or(ThemeError::Accent { value: v.clone() })?,
                format!("MARLOWE_ACCENT={v}"),
            ),
            None => (ACCENT_RGB, "default #9B7EDE".to_string()),
        };

        Ok(Self {
            accent: match depth {
                ColorDepth::TrueColor => {
                    Color::Rgb(accent_rgb.0, accent_rgb.1, accent_rgb.2)
                }
                // §B2's fallbacks are named for the *default* accent. An overridden accent at 256
                // colours is quantized to the nearest cube entry rather than silently becoming
                // violet — the user asked for a colour, not for ANSI 141.
                ColorDepth::Ansi256 => {
                    if env("MARLOWE_ACCENT").is_some() {
                        Color::Indexed(quantize_256(accent_rgb))
                    } else {
                        Color::Indexed(ACCENT_ANSI256)
                    }
                }
                ColorDepth::Ansi16 => Color::Magenta,
            },
            // Derived, so one accent stays one accent. At 30% of #9B7EDE this is #2E2643, which is
            // the mockup's #2e2a3d to within a shade — and it tracks MARLOWE_ACCENT for free.
            structure: pick(depth, scale(accent_rgb, 0.30), 237, Color::DarkGray),
            structure_dim: pick(depth, scale(accent_rgb, 0.18), 235, Color::DarkGray),
            // 0.72 of #9B7EDE is #705BA0, which is the mockup's #6f5aa3 to within one shade in each
            // channel. Deriving costs that shade and buys the guarantee that hover cannot drift
            // away from the accent when the accent is overridden.
            hover: pick(depth, scale(accent_rgb, 0.72), 97, Color::Magenta),
            // 30% of the way from the accent to white: #B9A5E8, the mockup's #b39ae8 to within a
            // shade. Tinted rather than scaled, because Marlowe's voice should read as *lighter*
            // than the accent, not dimmer than it.
            speech: pick(depth, tint(accent_rgb, 0.30), 147, Color::Magenta),
            green: pick(depth, (0x8F, 0xD4, 0xA8), 114, Color::Green),
            amber: pick(depth, (0xD6, 0xA9, 0x5F), 179, Color::Yellow),
            red: pick(depth, (0xDD, 0x7F, 0x85), 174, Color::Red),
            dim: pick(depth, (0x6A, 0x64, 0x80), 242, Color::DarkGray),
            dimmer: pick(depth, (0x4A, 0x44, 0x60), 238, Color::DarkGray),
            depth,
            depth_reason: reason,
            accent_source,
            no_color: env("NO_COLOR").filter(|v| !v.is_empty()),
        })
    }

    /// `Some(value)` when `NO_COLOR` is set and colour will therefore be suppressed downstream.
    ///
    /// The caller decides what to do about it; this type only reports. An explicit `--color-depth`
    /// is an explicit request and is allowed to override — see `tui.rs`.
    pub fn no_color(&self) -> Option<&str> {
        self.no_color.as_deref()
    }

    /// What the startup record should say about colour — **what will be emitted**, not what the
    /// terminal can carry. These are different questions and conflating them hid a bug.
    pub fn emission_report(&self) -> String {
        match &self.no_color {
            Some(v) => format!(
                "{} detected, but NO_COLOR={v} is set — crossterm emits ESC[m for every colour, \
                 so NOTHING will be coloured. Pass --color-depth to override.",
                self.depth.as_str()
            ),
            None => format!("{} ({})", self.depth.as_str(), self.depth_reason),
        }
    }

    /// Every colour this theme can emit, for the acceptance suite's by-value subset assertion.
    ///
    /// **This is the list §B13's colour row is checked against.** It exists so that row can be
    /// asserted by value rather than by counting distinct colours — a count passes happily while
    /// the values are all wrong, which is exactly what shipped before a screenshot caught it.
    pub fn declared_colours(&self) -> Vec<Color> {
        vec![
            Color::Reset, // foreground weight 1: the terminal's own
            self.accent,
            self.structure,
            self.structure_dim,
            self.hover,
            self.speech,
            self.green,
            self.amber,
            self.red,
            self.dim,    // weight 2
            self.dimmer, // weight 3
        ]
    }

    /// The palette used by tests and by `doctor`, with no environment involved.
    pub fn default_truecolor() -> Self {
        Self::resolve(|_| None, Some("truecolor")).expect("the default palette is valid")
    }

    pub fn depth(&self) -> ColorDepth {
        self.depth
    }

    pub fn depth_reason(&self) -> &str {
        &self.depth_reason
    }

    pub fn accent_source(&self) -> &str {
        &self.accent_source
    }

    pub fn accent(&self) -> Color {
        self.accent
    }

    /// Marlowe's own prose. The user's words keep the terminal's foreground.
    pub fn speech(&self) -> Color {
        self.speech
    }

    /// The hover tone — used for borders, labels and dropdown options under the pointer.
    pub fn hover_color(&self) -> Color {
        self.hover
    }

    /// The structural border colour — the accent at low luminance. Unfocused borders and the
    /// scrollbar track.
    pub fn structure(&self) -> Color {
        self.structure
    }

    /// The accent at 18% luminance — an inactive border. Named so [`crate::chrome::Ink`] can
    /// reach every colour this theme can emit; without it the palette vocabulary would have a
    /// hole exactly where `declared_colours` does not, and the two would disagree.
    pub fn structure_dim(&self) -> Color {
        self.structure_dim
    }

    /// Foreground weight 2 of 3, as a colour rather than a `Style`.
    pub fn dim_color(&self) -> Color {
        self.dim
    }

    /// Foreground weight 3 of 3, as a colour rather than a `Style`.
    pub fn dimmer_color(&self) -> Color {
        self.dimmer
    }

    /// The colour for a tone. `Normal` is the terminal's own foreground; `Dim` is an explicit
    /// value, because SGR 2 is too widely unimplemented to carry §B2's dimming on its own.
    pub fn tone(&self, tone: Tone) -> Color {
        match tone {
            Tone::Green => self.green,
            Tone::Amber => self.amber,
            Tone::Red => self.red,
            Tone::Accent => self.accent,
            Tone::Normal => Color::Reset,
            Tone::Dim => self.dim,
        }
    }

    /// A tone as a full style. `DIM` rides along with the explicit colour so terminals that *do*
    /// honour SGR 2 recess it further; terminals that ignore it still get the dimming.
    pub fn style(&self, tone: Tone) -> Style {
        let s = Style::default().fg(self.tone(tone));
        match tone {
            // No SGR 2. The colour IS the weight; see the type doc.
            _ => s,
        }
    }

    /// §B2's three foreground weights.
    pub fn bright(&self) -> Style {
        Style::default().add_modifier(Modifier::BOLD)
    }

    pub fn normal(&self) -> Style {
        Style::default()
    }

    pub fn dim(&self) -> Style {
        Style::default().fg(self.dim)
    }

    /// The third weight: near-invisible. What a calendar event needing nothing from the user gets.
    pub fn dimmer(&self) -> Style {
        Style::default().fg(self.dimmer)
    }

    /// `(border, label, hotkey)` for a focus level. §B2's focus table, and the whole of how focus
    /// is signalled: **three style swaps, zero cell repaints, no fill.**
    ///
    /// > Focused: accent border, brightened label, accent hotkey. Unfocused: default border, accent
    /// > label, dim hotkey. Inactive or irrelevant: dim border, dim label.
    ///
    /// "Default border" is read as **structural**, not as the terminal's own foreground: an
    /// unfocused border drawn in the user's body-text colour competes with the body text, and the
    /// mockup draws it recessive. Only the focused region gets accent on its border, which is what
    /// makes focus readable at a glance.
    pub fn region_styles(&self, focus: FocusLevel) -> (Style, Style, Style) {
        match focus {
            FocusLevel::Focused => (
                Style::default().fg(self.accent),
                Style::default().fg(self.accent).add_modifier(Modifier::BOLD),
                Style::default().fg(self.accent),
            ),
            // Below focus, above resting. The border and label brighten; the hotkey does not go
            // accent, because the accent hotkey is what says "this is where the keys are going".
            FocusLevel::Hovered => (
                Style::default().fg(self.hover),
                Style::default().fg(self.hover),
                Style::default().fg(self.dim),
            ),
            FocusLevel::Unfocused => (
                Style::default().fg(self.structure),
                Style::default().fg(self.accent),
                Style::default().fg(self.dim),
            ),
            FocusLevel::Inactive => (
                Style::default().fg(self.structure_dim),
                Style::default().fg(self.dimmer),
                Style::default().fg(self.dimmer),
            ),
        }
    }
}

/// Lift an RGB triple toward white. The complement of `scale`, for tones that must read as lighter
/// than the accent rather than as recessive versions of it.
fn tint((r, g, b): (u8, u8, u8), f: f32) -> (u8, u8, u8) {
    let t = |v: u8| (v as f32 + (255.0 - v as f32) * f).round().clamp(0.0, 255.0) as u8;
    (t(r), t(g), t(b))
}

/// Scale an RGB triple's luminance. Used to derive the structural border colours from the accent,
/// so there is one accent rather than a violet and a second violet that drift apart.
fn scale((r, g, b): (u8, u8, u8), f: f32) -> (u8, u8, u8) {
    let s = |v: u8| (v as f32 * f).round().clamp(0.0, 255.0) as u8;
    (s(r), s(g), s(b))
}

fn pick(depth: ColorDepth, rgb: (u8, u8, u8), ansi256: u8, ansi16: Color) -> Color {
    match depth {
        ColorDepth::TrueColor => Color::Rgb(rgb.0, rgb.1, rgb.2),
        ColorDepth::Ansi256 => Color::Indexed(ansi256),
        ColorDepth::Ansi16 => ansi16,
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

/// Nearest entry in the 6×6×6 colour cube (indices 16–231).
fn quantize_256((r, g, b): (u8, u8, u8)) -> u8 {
    let q = |v: u8| ((v as u16 * 5 + 127) / 255) as u8;
    16 + 36 * q(r) + 6 * q(g) + q(b)
}

/// Relative luminance per WCAG 2.1.
pub fn luminance((r, g, b): (u8, u8, u8)) -> f64 {
    let f = |c: u8| {
        let c = c as f64 / 255.0;
        if c <= 0.03928 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * f(r) + 0.7152 * f(g) + 0.0722 * f(b)
}

/// WCAG contrast ratio between two colours, 1.0 to 21.0.
///
/// §B13: *"accent legible on both dark and light terminal backgrounds — verified by eye on each."*
/// The eye check is the criterion and a number cannot replace it. This exists so the eye check
/// arrives with a measurement attached rather than as an unaided opinion, and so a change to
/// `MARLOWE_ACCENT` can be argued about in numbers.
pub fn contrast(fg: (u8, u8, u8), bg: (u8, u8, u8)) -> f64 {
    let (a, b) = (luminance(fg), luminance(bg));
    let (hi, lo) = if a > b { (a, b) } else { (b, a) };
    (hi + 0.05) / (lo + 0.05)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| (*v).to_string())
        }
    }

    #[test]
    fn a_malformed_accent_refuses_rather_than_falling_back() {
        let err = Theme::resolve(env_of(&[("MARLOWE_ACCENT", "#gg7ede")]), None).unwrap_err();
        assert!(matches!(err, ThemeError::Accent { .. }));
        // The message has to name the fix, or the refusal is just an obstacle.
        assert!(err.to_string().contains("#RRGGBB"));
    }

    #[test]
    fn the_depth_rule_reports_why_it_chose() {
        let (d, why) = ColorDepth::detect(env_of(&[("COLORTERM", "truecolor")]));
        assert_eq!(d, ColorDepth::TrueColor);
        assert_eq!(why, "COLORTERM=truecolor");

        // ADR-002 puts development on Windows Terminal, which sets neither COLORTERM nor a TERM
        // with 256 in it. Demoting it to 16 colours would make the primary platform the worst one.
        let (d, why) = ColorDepth::detect(env_of(&[("WT_SESSION", "abc-123")]));
        assert_eq!(d, ColorDepth::TrueColor);
        assert!(why.contains("Windows Terminal"));

        let (d, _) = ColorDepth::detect(env_of(&[("TERM", "xterm-256color")]));
        assert_eq!(d, ColorDepth::Ansi256);
        let (d, _) = ColorDepth::detect(env_of(&[("TERM", "vt100")]));
        assert_eq!(d, ColorDepth::Ansi16);
    }

    #[test]
    fn no_color_is_reported_rather_than_swallowed() {
        // The regression this exists for: the record said `truecolor` while the process emitted
        // `ESC[m` for every colour, and nothing in the system noticed the two disagreed.
        let t = Theme::resolve(env_of(&[("WT_SESSION", "x"), ("NO_COLOR", "1")]), None).unwrap();
        assert_eq!(t.depth(), ColorDepth::TrueColor, "detection is still correct");
        assert_eq!(t.no_color(), Some("1"));
        let report = t.emission_report();
        assert!(
            report.contains("NO_COLOR") && report.contains("NOTHING will be coloured"),
            "the record must state what is emitted, not what the terminal can carry: {report}"
        );
        assert!(
            report.contains("--color-depth"),
            "a refusal that does not name the fix is just an obstacle: {report}"
        );

        // Empty is not set, per no-color.org.
        let t = Theme::resolve(env_of(&[("WT_SESSION", "x"), ("NO_COLOR", "")]), None).unwrap();
        assert_eq!(t.no_color(), None);
        assert!(!t.emission_report().contains("NO_COLOR"));
    }

    #[test]
    fn no_style_this_module_produces_has_a_background() {
        let t = Theme::default_truecolor();
        let mut styles = vec![t.bright(), t.normal(), t.dim()];
        for tone in [
            Tone::Green,
            Tone::Amber,
            Tone::Red,
            Tone::Accent,
            Tone::Normal,
            Tone::Dim,
        ] {
            styles.push(t.style(tone));
        }
        for focus in [
            FocusLevel::Focused,
            FocusLevel::Hovered,
            FocusLevel::Unfocused,
            FocusLevel::Inactive,
        ] {
            let (a, b, c) = t.region_styles(focus);
            styles.extend([a, b, c]);
        }
        for s in styles {
            assert_eq!(
                s.bg, None,
                "a background reached a style. §B2: the terminal's own background is the \
                 background, and a fill costs a full-cell repaint on every focus change"
            );
        }
    }

    #[test]
    fn the_accent_is_measured_against_both_backgrounds() {
        // Not a pass/fail gate — §B13's criterion is the eye, on each. This prints the numbers the
        // eye check is argued with, and fails only if the accent goes below the point where the
        // question stops being "is it comfortable" and becomes "can you see it at all".
        let on_dark = contrast(ACCENT_RGB, (0x0f, 0x0e, 0x14));
        let on_light = contrast(ACCENT_RGB, (0xff, 0xff, 0xff));
        assert!(
            on_dark > 3.0,
            "accent on dark measured {on_dark:.2}:1, which is not legible for body-adjacent text"
        );
        assert!(
            on_light > 1.5,
            "accent on light measured {on_light:.2}:1; violet is the accent most likely to fail \
             §B13's light-background row and this is the floor below which it certainly does"
        );
    }
}
