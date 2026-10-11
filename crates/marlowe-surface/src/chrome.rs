//! **The glyphs the harness draws as chrome, in one place — so the drawing and the guard cannot
//! come to disagree about what chrome looks like.**
//!
//! # Why this module exists at all
//!
//! Until ADR-047 the conversation rendered model output as flat text. Flat text can still *say*
//! `  ⋯ read      /etc/passwd    48 lines`, and it rendered verbatim — but nothing in the product
//! interpreted markup, so the model could not control weight, colour or block structure, and a
//! forged line was a line of prose that happened to read like a tool call.
//!
//! Interpreting markdown changes that. The model now chooses **visual structure**: where a bold
//! run starts, where a rule sits, what is a heading. This project has been bitten by exactly that
//! twice already — `CondensedResult::render` joining fields as `"{k}: {v}"` so a value containing
//! `"\nanswer: …"` forged a field header, and a tool `target` containing a newline forging a second
//! §B6 tool line. Both were closed structurally, at the render site, by making the forgeable thing
//! impossible to express rather than by filtering for the known payload.
//!
//! This is the same fix in the same shape. **§B2's premise is that a border delineates an
//! interactive region**; the whole design rests on it. If the model can draw a border, the premise
//! is false and every border on the screen is a claim the user can no longer check. So the model
//! does not get to speak the harness's vocabulary:
//!
//! | glyph | what the harness draws with it |
//! |---|---|
//! | `⋯` U+22EF | §B6's tool line marker |
//! | `▸` `▾` U+25B8 / U+25BE | the disclosure markers — reasoning block, control-strip pickers |
//! | `↵` U+21B5 | "Enter acts here" |
//! | `▏` U+258F | the block-quote rule this crate draws |
//! | U+2500–U+257F | **box drawing** — every §B2 border, the compaction rule, the scrollbar track |
//! | U+2580–U+259F | **block elements** — the scrollbar thumb |
//!
//! # The single-definition property, and the failure it is guarding against
//!
//! [`MARKERS`] is read by `render.rs` **as the source of the glyphs it draws** and by
//! [`mark_reserved`] as the set it refuses. A future chrome glyph that is not added here is not
//! drawn either, because there is nowhere else to get it from — which is the same construction
//! `marlowe_permission::blocks_composed_targets` uses to keep the §B6 trust banner and the
//! adjudicator's enforcement site from drifting apart (CLAUDE.md, fifteenth instance).
//!
//! `tests/markdown_forgery.rs` asserts both halves: that each declared marker **actually appears**
//! in a drawn frame (or the reservation is guarding nothing), and that none of them survives in
//! model-authored prose.
//!
//! # What is marked, and not dropped
//!
//! The house style is `marlowe_contract::text::sanitize`'s: *"the marker names the codepoint
//! rather than swallowing it."* A refused glyph becomes `<U+22EF>`. A silently dropped glyph and a
//! clean string are indistinguishable to the reader, and the reader is the person who has to
//! decide whether the line in front of them came from the harness.
//!
//! **The marker is not provenance.** A model can write the literal text `<U+22EF>` and there is no
//! way to tell it from one this module wrote. That asymmetry is the safe direction and it is the
//! same one `marlowe-contract` records: the forgeable claim is *"something was refused here"*,
//! whose worst case is a reader looking harder at a line that was in fact clean.
//!
//! # The cost, named rather than absorbed
//!
//! A model that draws a directory tree with `├──` gets `<U+251C><U+2500><U+2500>`. That is ugly and
//! it is the intended behaviour: the alternative is quietly rewriting the model's characters into
//! ASCII look-alikes, which is a mangle the reader cannot detect. Markdown **tables** are the
//! common case and are unaffected — `markdown.rs` lays them out in aligned columns with no rules at
//! all, precisely so the frequent thing does not need the refused vocabulary.

use std::borrow::Cow;

use marlowe_view::Tone;
use ratatui::style::{Color, Style};

use crate::theme::Theme;

/// §B6's tool line marker: `  ⋯ read      Dockerfile                       48 lines`.
pub const TOOL_MARKER: char = '⋯';

/// A collapsed disclosure — the reasoning block, and the control-strip pickers' `▾`.
pub const DISCLOSURE_CLOSED: char = '▸';

/// An expanded disclosure.
pub const DISCLOSURE_OPEN: char = '▾';

/// "Enter acts here." Drawn on the reasoning header and on §B2 hotkey affordances.
pub const KEYCAP_ENTER: char = '↵';

/// The rule this crate draws down the left of a block quote. Reserved for the same reason as the
/// rest: a glyph the harness draws is a glyph the model does not get to draw.
pub const QUOTE_RULE: char = '▏';

/// The compaction rule's glyph — `─ compacted · 47 turns → summary ─`. Also every §B2 border, by
/// way of `ratatui::widgets::Block`.
pub const RULE: char = '─';

/// §B6's *"a visible scroll position is required"*, drawn on the right edge of the transcript.
///
/// U+2502, not ratatui's default U+2551: a doubled rule reads as a second border inside the region,
/// and §B9's overlay is the only doubled border in the design because it means modality.
pub const SCROLL_TRACK: char = '│';

/// U+2588 FULL BLOCK, which tiles edge to edge. A partial block leaves gaps between rows and the
/// thumb reads as segmented.
pub const SCROLL_THUMB: char = '█';

/// Every glyph the harness draws as chrome inside the conversation. **The drawing reads this, and
/// so does the guard.**
pub const MARKERS: [char; 8] = [
    TOOL_MARKER,
    DISCLOSURE_CLOSED,
    DISCLOSURE_OPEN,
    KEYCAP_ENTER,
    QUOTE_RULE,
    RULE,
    SCROLL_TRACK,
    SCROLL_THUMB,
];

/// Whether `c` belongs to the harness rather than to the model.
///
/// The two ranges are ranges rather than a list of the glyphs currently in use, and that is
/// deliberate: blocking `─` alone would leave `━`, `═`, `┏` and sixty others, each of which draws a
/// box just as convincingly. §B2's premise is about **borders**, not about one codepoint.
pub fn is_reserved(c: char) -> bool {
    let cp = c as u32;
    // Box Drawing, and Block Elements.
    (0x2500..=0x259F).contains(&cp) || MARKERS.contains(&c)
}

/// Replace every reserved glyph with a visible marker naming its codepoint.
///
/// Returns [`Cow::Borrowed`] when nothing needed replacing, which is the overwhelming majority of
/// model replies — this sits on the render path of every frame, so the clean case does not
/// allocate.
///
/// **Called once, on the raw string, before parsing and before wrapping.** Marking after wrapping
/// would silently widen a line that had already been fitted to the pane, and the column arithmetic
/// the §B13 flicker rows measure to the cell would be wrong by seven columns per marker.
pub fn mark_reserved(s: &str) -> Cow<'_, str> {
    if !s.chars().any(is_reserved) {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if is_reserved(c) {
            out.push_str(&format!("<U+{:04X}>", c as u32));
        } else {
            out.push(c);
        }
    }
    Cow::Owned(out)
}

/// The full model-text pipeline's first stage: the project's display predicate, then the chrome
/// reservation.
///
/// # Why the TUI now sanitises when `tests/display_sanitiser.rs` records that it deliberately did
/// not
///
/// That file's header names the two things that would change the trade, and **markdown is both**:
///
/// * *"the marker substitution would change wrapping and column arithmetic that §B13's flicker
///   rows measure to the cell."* True while wrapping happened on the raw string. The markdown
///   renderer wraps **after** substitution, so the arithmetic sees the final text — that objection
///   is answered by the order of operations rather than argued away.
/// * The TUI's defence was ratatui discarding control characters on their way into a `Buffer`.
///   **That was measured rather than assumed, and the first draft of this paragraph was wrong.** It
///   claimed ratatui says nothing about U+2028, the BiDi overrides or the zero-width block; it
///   discards all of them, along with `ESC`, `BEL`, `TAB` and C0/C1 generally.
///
///   What it passes through is the **tag block, U+E0000–U+E007F** — a full invisible ASCII
///   alphabet, and the documented channel for smuggling instructions past a human reader.
///   `marlowe_contract::text::is_renderable` refuses it by name, so the two layers are
///   **complementary rather than redundant**. That is a better reason to run both than the one the
///   draft gave, and it is recorded here rather than quietly corrected because the mistake is the
///   family this project keeps logging: a claim about a mechanism, made from reading it.
///
///   `marlowe-surface/tests/display_sanitiser.rs` now carries the measurement as a test, on
///   `Entry::User` — the path where ratatui is still the only thing in the way.
///
/// So `marlowe_contract::text::sanitize_prose` runs first — one predicate, shared with the
/// contract-value check, rather than a second idea of what a safe character is.
///
/// Returns [`Cow::Borrowed`] when the reply needed neither pass, which is the overwhelmingly common
/// case. This runs on every entry on every frame, so an unconditional copy of the whole transcript
/// would be a per-frame cost paid for nothing.
pub fn prepare_model_text(s: &str) -> Cow<'_, str> {
    match marlowe_contract::text::sanitize_prose(s) {
        Cow::Borrowed(b) => mark_reserved(b),
        Cow::Owned(o) => Cow::Owned(mark_reserved(&o).into_owned()),
    }
}


// -----------------------------------------------------------------------------------------------
// THE PALETTE
// -----------------------------------------------------------------------------------------------

/// Every colour the harness paints with, named by role.
///
/// # Why this is in `chrome` and not in `theme`
///
/// [`Theme`] owns the **values** — what violet is, what happens at 256 colours, how the structural
/// tones are derived from the accent. This owns the **vocabulary**: which of those a given surface
/// is allowed to reach for. They are different questions, and the second one had no home at all
/// until M3 F2, which is the whole of the finding this type exists to close.
///
/// `window.rs` reached for `Tone::Green`. `render.rs` never does. Nothing caught it, because
/// M3-DESIGN §6.4's *"do not reimplement the look — use `chrome.rs`"* was followed to the letter
/// and this module defined **glyphs only**. Each surface was individually inside §B13's budget —
/// one accent, three state colours, three weights — and the budget bounds how many colours *one*
/// surface uses, never whether two surfaces use the *same* ones. So the window grew a hue that
/// exists nowhere near it and every per-surface test stayed green.
///
/// This is the glyph construction applied to colour, and deliberately the same shape: [`MARKERS`]
/// is the set the drawing reads *and* the set the guard refuses; [`CONVERSATION`] and
/// [`RUN_WINDOW`] are the sets the drawing reads *and* the sets `tests/palette_subset.rs` checks a
/// rendered `Buffer` against. A colour that is not here cannot be drawn, because there is nowhere
/// else to get one from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Ink {
    /// Foreground weight 1: the terminal's own. Body text, and the user's own words.
    Body,
    /// Foreground weight 2.
    Dim,
    /// Foreground weight 3: near-invisible, for what needs nothing from the reader.
    Dimmer,
    /// §B2's one accent. Focus, hotkeys, labels.
    Accent,
    /// The accent at low luminance: an unfocused border, the scrollbar track.
    Structure,
    /// The accent lower still: an inactive border.
    StructureDim,
    /// The accent under the pointer.
    Hover,
    /// **Marlowe's own prose** — the accent tinted toward white. The one place colour marks who is
    /// speaking rather than state.
    Speech,
    /// State: needs attention, approaching a limit, degraded.
    Amber,
    /// State: conflict, failure, irreversible.
    Red,
    /// State: healthy, live, running normally.
    ///
    /// **In the vocabulary and out of the run window**, which is the distinction F2 turns on. §B2
    /// lists green as a legal state colour and §B5's voice band spends it — so removing it from the
    /// product would be answering a complaint about *this window* by deleting something else's
    /// colour. What was wrong was a second surface reaching for a hue its neighbour never uses.
    Green,
}

impl Ink {
    /// Every role, for a test that has to name them all.
    pub const ALL: [Ink; 11] = [
        Ink::Body,
        Ink::Dim,
        Ink::Dimmer,
        Ink::Accent,
        Ink::Structure,
        Ink::StructureDim,
        Ink::Hover,
        Ink::Speech,
        Ink::Amber,
        Ink::Red,
        Ink::Green,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Ink::Body => "body",
            Ink::Dim => "dim",
            Ink::Dimmer => "dimmer",
            Ink::Accent => "accent",
            Ink::Structure => "structure",
            Ink::StructureDim => "structure-dim",
            Ink::Hover => "hover",
            Ink::Speech => "speech",
            Ink::Amber => "amber",
            Ink::Red => "red",
            Ink::Green => "green",
        }
    }

    /// The colour, **from the theme**. This type does not know what violet is and must not learn:
    /// a second definition of a border colour is the two-sides-silently-disagree shape applied to
    /// pixels, which is the thing §6.4 was already trying to prevent.
    pub fn color(self, theme: &Theme) -> Color {
        match self {
            Ink::Body => Color::Reset,
            Ink::Dim => theme.dim_color(),
            Ink::Dimmer => theme.dimmer_color(),
            Ink::Accent => theme.accent(),
            Ink::Structure => theme.structure(),
            Ink::StructureDim => theme.structure_dim(),
            Ink::Hover => theme.hover_color(),
            Ink::Speech => theme.speech(),
            Ink::Amber => theme.tone(Tone::Amber),
            Ink::Red => theme.tone(Tone::Red),
            Ink::Green => theme.tone(Tone::Green),
        }
    }

    /// The colour as a `Style`. No background, ever — see [`Theme`]'s header.
    pub fn style(self, theme: &Theme) -> Style {
        Style::default().fg(self.color(theme))
    }

    /// The ink a producer-supplied [`Tone`] paints in.
    ///
    /// **The one crossing between the data vocabulary and the drawing vocabulary.** `Tone` is what
    /// a view model carries — a run's state, an item's border — and it is deliberately narrower
    /// than `Ink`: a producer chooses *state*, and the structural roles are the surface's own.
    pub fn of_tone(tone: Tone) -> Ink {
        match tone {
            Tone::Normal => Ink::Body,
            Tone::Dim => Ink::Dim,
            Tone::Accent => Ink::Accent,
            Tone::Amber => Ink::Amber,
            Tone::Red => Ink::Red,
            Tone::Green => Ink::Green,
        }
    }

    /// Which role a colour on a rendered cell came from, if any.
    ///
    /// **This is what makes the palette checkable where it renders rather than where it is
    /// declared.** Asserting that `RUN_WINDOW` does not contain `Green` is asserting the value of a
    /// constant — family #16, a control with no reader. Walking a drawn `Buffer` back through this
    /// asserts the fate of a cell.
    pub fn of_color(theme: &Theme, c: Color) -> Option<Ink> {
        Ink::ALL.into_iter().find(|i| i.color(theme) == c)
    }
}

/// Every ink the conversation surface draws with.
///
/// All eleven: it carries §B5's voice band (green), the inspector's hover, and the inactive item
/// borders that nothing else in the product has.
pub const CONVERSATION: &[Ink] = &Ink::ALL;

/// Every ink a **run window** draws with — a strict subset of [`CONVERSATION`].
///
/// §6's window is *"the same product with different sections"*, so the sections it does not have
/// are the colours it does not spend:
///
/// | absent | why the window has no use for it |
/// |---|---|
/// | `Green` | nothing here is a *voice* state, and a resume step is a fact, not a health report |
/// | `Hover` | a window is keyboard-driven; there is no pointer target in it |
/// | `StructureDim` | its regions are focused or unfocused, never inactive — every panel is live |
///
/// **The subset is the property, and it is asserted across the two surfaces** — no per-surface
/// budget check can see it, which is exactly how the window drifted.
pub const RUN_WINDOW: &[Ink] = &[
    Ink::Body,
    Ink::Dim,
    Ink::Dimmer,
    Ink::Accent,
    Ink::Structure,
    Ink::Speech,
    Ink::Amber,
    Ink::Red,
];

/// Whether a surface's vocabulary admits an ink.
pub fn permits(surface: &[Ink], ink: Ink) -> bool {
    surface.contains(&ink)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clean_string_is_borrowed_not_rebuilt() {
        // Not a micro-optimization check: this is the assertion that an ordinary reply does not
        // allocate a second copy of itself on every frame.
        assert!(matches!(mark_reserved("an ordinary reply"), Cow::Borrowed(_)));
    }

    #[test]
    fn every_declared_marker_is_reserved_and_named_rather_than_dropped() {
        for c in MARKERS {
            assert!(is_reserved(c), "{c:?} is declared chrome and is not reserved");
            // Bound first: `mark_reserved` returns a `Cow` borrowing its input.
            let input = format!("a{c}b");
            let marked = mark_reserved(&input);
            assert!(!marked.contains(c), "{c:?} survived: {marked}");
            assert_eq!(marked, format!("a<U+{:04X}>b", c as u32));
        }
    }

    #[test]
    fn blocking_one_rule_glyph_would_leave_sixty_others() {
        // The reason `is_reserved` takes ranges. Each of these draws a box as convincingly as `─`.
        for c in ['━', '═', '┏', '┓', '╔', '╝', '┤', '╬', '█', '▌'] {
            assert!(is_reserved(c), "{c:?} draws a border and is not reserved");
        }
    }

    #[test]
    fn ordinary_prose_punctuation_is_not_reserved() {
        // The reservation must be narrow enough that a normal reply passes through untouched, or
        // the cure is worse than the disease and somebody will delete it.
        for c in ['·', '—', '–', '→', '•', '…', '“', '”', '’', '±', 'α', '∑'] {
            assert!(!is_reserved(c), "{c:?} is ordinary prose and was reserved");
        }
    }

    #[test]
    fn the_pipeline_closes_the_three_families_ratatui_does_not() {
        // U+2028 is the one that matters most for a *block* parser: `str::lines()` does not split
        // on it, so the parser and the eye disagree about where a line ends.
        for (c, why) in [
            ('\u{2028}', "LINE SEPARATOR — a break str::lines() does not see"),
            ('\u{202e}', "RIGHT-TO-LEFT OVERRIDE — moves a rendered indent"),
            ('\u{200b}', "ZERO WIDTH SPACE — width on screen and width in the wrap arithmetic diverge"),
            ('\u{1b}', "ESC — ratatui's job, asserted here too so the order of operations is fixed"),
        ] {
            let input = format!("a{c}b");
            let out = prepare_model_text(&input);
            assert!(!out.contains(c), "{why}: survived `prepare_model_text`");
        }
    }

    #[test]
    fn the_window_vocabulary_is_a_subset_of_the_conversation_one() {
        // The declaration half. `tests/palette_subset.rs` is the half that reads a drawn buffer;
        // this one is here so a future edit to `RUN_WINDOW` fails in the module it edits.
        for ink in RUN_WINDOW {
            assert!(
                permits(CONVERSATION, *ink),
                "{} is in the run window's palette and not in the conversation's; the window \
                 would then be introducing a hue that exists nowhere near it, which is the \
                 finding this constant exists to close",
                ink.name()
            );
        }
        assert!(
            RUN_WINDOW.len() < CONVERSATION.len(),
            "a window vocabulary equal to the conversation's asserts nothing"
        );
    }

    #[test]
    fn every_role_has_a_distinct_colour_so_a_drawn_cell_maps_back_to_one() {
        // `Ink::of_color` is how the cross-surface test reads a buffer. Two roles sharing a value
        // would make it answer the wrong question silently -- and at the 16-colour floor they DO
        // collapse, which is why the palette tests pin truecolor.
        let t = Theme::default_truecolor();
        let mut seen = std::collections::BTreeMap::new();
        for ink in Ink::ALL {
            if let Some(other) = seen.insert(format!("{:?}", ink.color(&t)), ink) {
                panic!("{} and {} render the same colour", other.name(), ink.name());
            }
        }
        for ink in Ink::ALL {
            assert_eq!(Ink::of_color(&t, ink.color(&t)), Some(ink));
        }
    }

    #[test]
    fn every_tone_a_producer_can_send_has_an_ink() {
        // The crossing point. A `Tone` with no ink would have to be handled somewhere else, and
        // "somewhere else" is the second palette this module exists to prevent.
        let t = Theme::default_truecolor();
        for tone in [Tone::Normal, Tone::Dim, Tone::Accent, Tone::Amber, Tone::Red, Tone::Green] {
            assert_eq!(
                Ink::of_tone(tone).color(&t),
                t.tone(tone),
                "{tone:?} paints a different colour through the palette than through the theme"
            );
        }
    }
}
