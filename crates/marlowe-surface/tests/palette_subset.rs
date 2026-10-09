//! **The palette is checked where it renders: on the cells of a drawn `Buffer`.**
//!
//! # Why this file exists
//!
//! §B13's colour budget counts colours *within* one surface: one accent, three state colours,
//! three weights. It bounds how many hues one surface spends, never whether two surfaces spend the
//! *same* ones -- which is how a second surface once grew a hue that exists nowhere near its
//! neighbour while every per-surface test stayed green. The human's words after using it: *"Too
//! many colours. SAME STYLE."*
//!
//! # Where it asserts, and why not on the constants
//!
//! `chrome.rs`'s own unit tests check the declarations. That is the value of a constant, and a
//! test on a declaration is worth little: nothing reads the field. So every assertion here walks a
//! **drawn `Buffer`** and maps each cell's foreground back through [`chrome::Ink::of_color`]. What
//! is being asserted is the fate of a pixel.
//!
//! The surfaces drawn here are the ones this crate still owns: rendered markdown, the approval
//! overlays and the meter. Whatever layout is built on them is expected to add its own frames to
//! `drawn_frames` and keep the same two assertions.

mod common;

use std::collections::BTreeSet;

use common::Pane;
use marlowe_surface::chrome::{self, Ink};
use marlowe_surface::{meter, overlay};
use marlowe_view::approval::PendingApproval;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;

/// Every foreground role that reached a non-blank cell.
fn inks(buf: &Buffer) -> BTreeSet<Ink> {
    let theme = common::theme();
    let mut out = BTreeSet::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            let Some(cell) = buf.cell((x, y)) else { continue };
            // A blank cell in the terminal's own colours is not a colour choice; counting it would
            // put `Body` in every set for free and make the assertions weaker than they look.
            if cell.symbol().trim().is_empty() {
                continue;
            }
            match Ink::of_color(&theme, cell.fg) {
                Some(ink) => {
                    out.insert(ink);
                }
                None => panic!(
                    "a cell at {x},{y} is painted {:?}, which is not in the palette at all -- \
                     `chrome::Ink` is meant to be the only way to choose a colour, and something \
                     reached around it",
                    cell.fg
                ),
            }
        }
    }
    out
}

fn names(set: &BTreeSet<Ink>) -> String {
    set.iter().map(|i| i.name()).collect::<Vec<_>>().join(", ")
}

/// A markdown reply that reaches for every construct the renderer knows.
const RICH: &str = "\
# Heading

Some **strong**, *emphasised* and ~~struck~~ prose with `code` and a [link](https://example.invalid/x).

1. first
2. second

> a quotation

| a | b |
|---|---|
| 1 | 2 |

```rust
let x = 1;
```

Maths: $\\alpha \\times \\beta^2$.

---

done
";

fn pending(reversible: bool) -> PendingApproval {
    PendingApproval {
        decision: 7,
        verb: "fetch".into(),
        scope: "web · https://a-long-host.example.com/some/path".into(),
        reversible,
        novelty: Some("first time on this host".into()),
    }
}

/// Every frame this crate can draw, across the surfaces it owns.
fn drawn_frames() -> Vec<Buffer> {
    let theme = common::theme();
    let mut out = Vec::new();

    // Rendered markdown, in the model's voice and in the user's.
    for (w, h) in common::SIZES {
        out.push(common::frame(&Pane::saying(RICH), w, h));
        out.push(common::frame(&Pane::user("what does the gate do?"), w, h));
    }

    // The approval overlays, both tiers, and the decline-reason state.
    for (reversible, reason) in [(true, None), (false, None), (true, Some("not today"))] {
        let area = Rect::new(0, 0, 120, 30);
        let mut buf = Buffer::empty(area);
        overlay::draw_pending_approval(&pending(reversible), reason, &theme, area, &mut buf);
        out.push(buf);
    }

    // The cancel overlay, which is the one place a hard-coded grey once survived: it scrimmed every
    // cell to `Color::DarkGray`, a colour picked outside the palette and invisible to any check
    // that reads the declaration.
    let area = Rect::new(0, 0, 120, 30);
    let mut buf = Buffer::empty(area);
    overlay::draw_window_cancel(&common::run_view(), &theme, area, &mut buf);
    out.push(buf);

    // The meter, at every level.
    for level in 0..=marlowe_view::LEVELS {
        let mut buf = Buffer::empty(Rect::new(0, 0, 12, 4));
        meter::render(
            &[level; marlowe_view::SAMPLES],
            Rect::new(0, 0, meter::CELLS_WIDE, meter::CELLS_TALL),
            &mut buf,
            Style::default().fg(Ink::Accent.color(&theme)),
        );
        out.push(buf);
    }

    out
}

fn observed(frames: &[Buffer]) -> BTreeSet<Ink> {
    frames.iter().flat_map(inks).collect()
}

/// **The property.** Every foreground on every drawn cell is a palette role, and the roles used
/// stay inside the run-window vocabulary -- which excludes the voice band's green, the pointer's
/// hover and the inactive-border dim, none of which anything here has a reason to spend.
#[test]
fn drawn_cells_paint_only_roles_the_palette_declares() {
    let frames = drawn_frames();
    let seen = observed(&frames);

    let declared: BTreeSet<Ink> = chrome::RUN_WINDOW.iter().copied().collect();
    let extra: Vec<_> = seen.difference(&declared).copied().collect();
    assert!(
        extra.is_empty(),
        "these surfaces paint {} -- outside the shared vocabulary. A hue that exists nowhere near \
         its neighbours reads as a second application.\n  seen:     {}\n  declared: {}",
        extra.iter().map(|i| i.name()).collect::<Vec<_>>().join(", "),
        names(&seen),
        names(&declared),
    );
}

/// **The control, without which the property above passes on an empty sweep.** A fixture that drew
/// nothing, or only body text, would satisfy "no foreign colour" for the wrong reason.
#[test]
fn the_sweep_actually_reaches_the_roles_it_claims_to_check() {
    let seen = observed(&drawn_frames());
    for ink in [Ink::Accent, Ink::Speech, Ink::Dim, Ink::Amber, Ink::Red] {
        assert!(
            seen.contains(&ink),
            "no drawn cell used {}, so the fixture is too thin to be evidence about it: {}",
            ink.name(),
            names(&seen),
        );
    }
}

/// The declaration has a reader, and the reader is the test above: `RUN_WINDOW` is a subset of
/// `CONVERSATION`, so narrowing the vocabulary can never widen the palette.
#[test]
fn the_narrow_vocabulary_is_inside_the_wide_one() {
    for ink in chrome::RUN_WINDOW {
        assert!(chrome::permits(chrome::CONVERSATION, *ink), "{} is outside CONVERSATION", ink.name());
    }
}
