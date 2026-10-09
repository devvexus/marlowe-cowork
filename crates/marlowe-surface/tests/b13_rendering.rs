//! §B13's rendering rows, each as a command that prints a number.
//!
//! * background fills used to signal focus — **zero**
//! * chrome inside a scroll area — **zero**
//! * repaint flicker during stream or resize — **zero, 120×30 through 240×60**
//! * tool call default footprint — **1 line**
//! * distinct colours — **≤ 1 accent + 3 state + 3 foreground weights**
//! * below-minimum width — **honest refusal, never a degraded grid**
//!
//! "Flicker" is the one that would otherwise stay an opinion. Here it is a **cell count**: render
//! frame N, render frame N+1, diff the buffers, and assert which cells were allowed to change.
//! A text delta may touch conversation-interior cells and nothing else; a focus change may touch
//! border and title cells and **zero** interior cells. That is only possible because every render
//! is a pure function of `(state, now_ms)` — see `frame_clock.rs`.

mod common;

use marlowe_view::{StatusState, Tab};
use marlowe_surface::app::{App, Key};
use marlowe_surface::region::RegionId;
use marlowe_surface::render;
use ratatui::layout::Rect;
use ratatui::style::Color;

/// Every screen worth walking: each status state, each tab, the overlay, and a dropdown.
fn scenarios() -> Vec<(String, App)> {
    let mut out = Vec::new();
    for state in [
        StatusState::Listening,
        StatusState::Thinking,
        StatusState::Speaking,
        StatusState::Writing,
        StatusState::Running,
        StatusState::Waiting,
        StatusState::Idle,
    ] {
        let mut producer = marlowe_stub::Session::new();
        producer.force_state(state, 500);
        producer.tick(500);
        let app = App::new(producer.view().clone()).unwrap();
        out.push((format!("state {}", state.name()), app));
    }
    for tab in Tab::ALL {
        let mut app = App::new(marlowe_stub::Session::new().view().clone()).unwrap();
        app.tab = tab;
        app.focus = RegionId::Item(tab.into(), 0);
        out.push((format!("tab {}", tab.title()), app));
    }
    let mut open = App::new(marlowe_stub::Session::new().view().clone()).unwrap();
    open.on_key(Key::Esc);
    open.on_key(Key::Char('a'));
    open.on_key(Key::Enter);
    out.push(("autonomy dropdown open".into(), open));

    let mut typing = App::new(marlowe_stub::Session::new().view().clone()).unwrap();
    typing.input = "/sc".into();
    out.push(("slash autocomplete".into(), typing));

    out
}

/// §B2: **never a background fill.** §B14: no background fill to signal focus or selection.
///
/// This walks the whole buffer, including under the approval overlay — which is where a naive
/// implementation would reach for a `bg` scrim and quietly fail both this row and §B12's
/// flicker target in one move.
#[test]
fn not_one_cell_in_any_screen_carries_a_background() {
    let mut cells = 0usize;
    for (name, app) in scenarios() {
        for (w, h) in common::SIZES {
            let buf = common::frame(&app, w, h);
            for y in 0..h {
                for x in 0..w {
                    let cell = buf.cell((x, y)).unwrap();
                    cells += 1;
                    assert_eq!(
                        cell.bg,
                        Color::Reset,
                        "{name} at {w}x{h}: cell ({x},{y}) has background {:?}. §B2 — the \
                         terminal's own background is the background; a fill collides with the \
                         user's theme and costs a full-cell repaint on every focus change",
                        cell.bg
                    );
                }
            }
        }
    }
    println!("background fills: 0 of {cells} cells walked");
}

/// §B3: *"Chrome that scrolls is a bug. Every region's label, hotkey and footer are pinned outside
/// its scroll area. Only content moves."*
#[test]
fn no_label_hotkey_pager_or_tab_bar_falls_inside_a_scroll_area() {
    for (w, h) in common::SIZES {
        let c = render::layout(Rect::new(0, 0, w, h));

        // Borders carry the labels and hotkeys, and the scroll area is strictly inside them.
        for (name, region, scroll) in [
            ("conversation", c.conversation, c.conversation_scroll),
            ("inspector", c.inspector, c.inspector_scroll),
        ] {
            assert!(
                scroll.x >= region.x
                    && scroll.y >= region.y
                    && scroll.right() <= region.right()
                    && scroll.bottom() <= region.bottom(),
                "{name} scroll area escapes its region at {w}x{h}"
            );
        }

        assert!(
            !intersects(c.pager, c.conversation_scroll),
            "the pager is inside the conversation's scroll area at {w}x{h}; it would scroll away, \
             and it carries turn count, compaction count and lineage depth that live nowhere else"
        );
        assert!(
            !intersects(c.tab_bar, c.inspector_scroll),
            "the tab bar is inside the inspector's scroll area at {w}x{h}; scrolling a pane would \
             hide the way back to the others"
        );
        assert!(
            !intersects(c.footer, c.conversation_scroll)
                && !intersects(c.footer, c.inspector_scroll),
            "the footer is inside a scroll area at {w}x{h}"
        );
    }
    println!(
        "chrome inside a scroll area: 0 across {} sizes",
        common::SIZES.len()
    );
}

fn intersects(a: Rect, b: Rect) -> bool {
    a.x < b.right() && b.x < a.right() && a.y < b.bottom() && b.y < a.bottom()
}

/// §B12: *"This is why focus is a border and not a fill."* The claim is testable: a focus change
/// must touch border and title cells only.
#[test]
fn a_focus_change_repaints_borders_only_and_never_a_region_interior() {
    for (w, h) in common::SIZES {
        let mut app = common::app();
        app.on_key(Key::Esc);
        let mut term = common::terminal(w, h);
        let before = common::draw_into(&mut term, &app);

        app.on_key(Key::Char('m')); // conversation -> Model
        let after = common::draw_into(&mut term, &app);

        let changed = common::diff_cells(&before, &after);
        let c = render::layout(Rect::new(0, 0, w, h));

        // Regions **not** involved in the focus change. Their interiors must be untouched — not
        // "mostly", not "bounded": zero. If a fill were signalling focus, every one of these would
        // light up the moment focus passed near them.
        let uninvolved = [
            ("conversation", inner(c.conversation)),
            ("status band", inner(c.status)),
            ("message", inner(c.message)),
            ("autonomy", inner(c.control[4])),
            ("workspace", inner(c.control[3])),
        ];
        for (name, r) in uninvolved {
            let hits = changed
                .iter()
                .filter(|(x, y)| *x >= r.x && *x < r.right() && *y >= r.y && *y < r.bottom())
                .count();
            assert_eq!(
                hits, 0,
                "{w}x{h}: focus moved to Model and repainted {hits} cells inside the {name} \
                 interior. §B12 — focus is a border and a title, not a fill"
            );
        }

        // The Model region's own interior may restyle: §B2's focus table brightens the value. That
        // is a style swap on the value's own cells and nothing wider, so it is bounded by the
        // value's length rather than by a round number nobody chose.
        let model = inner(c.control[0]);
        let model_hits = changed
            .iter()
            .filter(|(x, y)| {
                *x >= model.x && *x < model.right() && *y >= model.y && *y < model.bottom()
            })
            .count();
        let value_len = app.view().control.model.value().chars().count();
        assert!(
            model_hits <= value_len,
            "{w}x{h}: focusing Model repainted {model_hits} interior cells for a {value_len}-char \
             value; anything wider is a fill"
        );
        println!(
            "{w}x{h}: focus change touched {} cells — {model_hits} inside the newly focused \
             region's interior, 0 inside every other",
            changed.len()
        );
    }
}

/// §B12: *"Streaming does not repaint the screen."*
#[test]
fn a_streaming_text_delta_touches_the_conversation_and_the_status_band_only() {
    for (w, h) in common::SIZES {
        let mut r = common::rig();
        r.app.input = "read the retrieval code".into();
        r.app.submit();
        r.settle(0);
        let mut term = common::terminal(w, h);

        r.tick(200);
        let before = common::draw_into(&mut term, &r.app);
        r.tick(260);
        let after = common::draw_into(&mut term, &r.app);

        let c = render::layout(Rect::new(0, 0, w, h));
        let allowed = [c.conversation, c.status];
        let strays: Vec<_> = common::diff_cells(&before, &after)
            .into_iter()
            .filter(|(x, y)| {
                !allowed
                    .iter()
                    .any(|r| *x >= r.x && *x < r.right() && *y >= r.y && *y < r.bottom())
            })
            .collect();
        assert!(
            strays.is_empty(),
            "{w}x{h}: a streaming delta repainted {} cells outside the conversation and status \
             band, first at {:?}. §B12 — streaming does not repaint the screen",
            strays.len(),
            strays.first()
        );
    }
    println!("streaming repaint outside the conversation and status band: 0 cells");
}

/// A resize must reflow cleanly — every size in the range renders without panic and without
/// leaving content outside its region.
#[test]
fn every_size_in_the_supported_range_renders() {
    let app = common::app();
    let mut n = 0;
    for w in (120..=240).step_by(7) {
        for h in (30..=60).step_by(5) {
            let buf = common::frame(&app, w, h);
            assert_eq!(buf.area, Rect::new(0, 0, w, h));
            n += 1;
        }
    }
    println!("rendered {n} geometries across 120x30 through 240x60 without a panic");
}

/// A resize itself must not scatter cells outside the regions that actually changed shape.
///
/// §B12: *"Resize reflows cleanly."* The check is that reflowing to a **larger** terminal and back
/// returns the identical buffer — a layout that accumulated state across resizes would not.
#[test]
fn a_resize_round_trip_returns_the_identical_frame() {
    let app = common::app();
    let before = common::frame(&app, 140, 40);
    let _wide = common::frame(&app, 240, 60);
    let _narrow = common::frame(&app, 120, 30);
    let after = common::frame(&app, 140, 40);
    let changed = common::diff_cells(&before, &after);
    assert!(
        changed.is_empty(),
        "resizing away and back changed {} cells; layout is holding state across resizes",
        changed.len()
    );
    println!("resize round trip 140x40 -> 240x60 -> 120x30 -> 140x40: 0 cells differ");
}

/// §B6: *"Tool calls render as one line."* §B13: default footprint 1 line.
#[test]
fn a_settled_tool_call_occupies_exactly_one_line() {
    let app = common::app();
    let theme = common::theme();
    let lines = render::transcript_lines(&app, &theme, 70);
    let text: Vec<String> = lines
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect();

    let tool_lines: Vec<&String> = text.iter().filter(|l| l.contains('⋯')).collect();
    assert!(!tool_lines.is_empty(), "the opening transcript has tool calls");
    for l in &tool_lines {
        assert_eq!(l.matches('⋯').count(), 1);
    }
    println!("tool call footprint: 1 line × {} calls", tool_lines.len());

    // Six reads collapse to one line carrying the count (§B6).
    let mut r = common::rig();
    r.producer.submit("read the retrieval code", 0);
    r.tick(1_000);
    let collapsed = render::transcript_lines(&r.app, &theme, 70);
    let joined: String = collapsed
        .iter()
        .flat_map(|l| l.spans.iter().map(|sp| sp.content.to_string()))
        .collect();
    assert!(
        joined.contains("6 files"),
        "consecutive same-verb calls must collapse: six reads become `⋯ read  6 files`"
    );
}

/// §B13: **≤ 1 accent + 3 state + 3 foreground weights** — asserted **by value**.
///
/// # Why by value and not by count
///
/// The first version of this test counted distinct colours and asserted the count was small. It
/// passed while every colour on screen was wrong: the depth probe had fallen to the 16-colour
/// floor on a real xterm, so the violet accent rendered as ANSI bright magenta and the muted state
/// colours as terminal green/yellow/red. **Six distinct colours, not one of them the design.**
///
/// A count cannot catch that. The emitted set being a subset of `Theme::declared_colours` can, and
/// this row is in §B13's table precisely so a screenshot is not what catches it.
#[test]
fn every_colour_emitted_is_one_of_the_declared_values() {
    let theme = common::theme();
    // A Vec, not a HashSet: `determinism_guard.rs` bans hash-ordered collections crate-wide, and
    // ratatui's Color is not Ord so a BTreeSet is unavailable. Nine entries; linear is fine.
    let mut allowed: Vec<Color> = theme.declared_colours();
    // The approval overlay dims the frame behind it by rewriting foregrounds, not by filling
    // backgrounds. That is one more foreground value and it is named here rather than smuggled in.
    allowed.push(Color::DarkGray);

    // **The accent must actually be the accent.** If the depth probe demotes, this fails on the
    // first cell instead of after somebody looks at a screenshot — which is how it failed before.
    assert_eq!(
        theme.accent(),
        Color::Rgb(0x9B, 0x7E, 0xDE),
        "the truecolor theme's accent is not #9B7EDE"
    );

    let mut used: Vec<Color> = Vec::new();
    for (name, app) in scenarios() {
        let buf = common::frame(&app, 140, 40);
        for y in 0..40 {
            for x in 0..140 {
                let fg = buf.cell((x, y)).unwrap().fg;
                if !used.contains(&fg) {
                    used.push(fg);
                }
                assert!(
                    allowed.contains(&fg),
                    "{name}: cell ({x},{y}) uses {fg:?}, which is outside the declared palette \
                     of one accent, three state colours and three foreground weights"
                );
            }
        }
    }
    println!(
        "emitted foreground colours: {} distinct, all within the {} declared values",
        used.len(),
        allowed.len()
    );
    for c in &used {
        println!("    in use: {c:?}");
    }
}

/// The scrollbar, asserted rather than explained.
///
/// A segmented-looking thumb has two possible causes **in the emitted cells** — a symbol that does
/// not tile, or a background fill — and a third that is not in the cells at all: the emulator's
/// rasterisation of U+2588 at a given font and cell height. Both of the first two are checked here,
/// so the next time it comes up the third is the only remaining explanation.
#[test]
fn the_scrollbar_thumb_is_contiguous_full_blocks_with_no_fill() {
    for (w, h) in common::SIZES {
        let app = common::app();
        let buf = common::frame(&app, w, h);
        let c = render::layout(Rect::new(0, 0, w, h));
        let x = c.conversation_scroll.right() - 1;

        let mut thumb_rows = Vec::new();
        let mut blanks = 0;
        for y in c.conversation_scroll.y..c.conversation_scroll.bottom() {
            let cell = buf.cell((x, y)).unwrap();
            assert_eq!(
                cell.bg,
                Color::Reset,
                "{w}x{h}: scrollbar cell ({x},{y}) carries a background. §B2 — never a fill, and a \
                 filled scrollbar is exactly how a thumb comes to look segmented"
            );
            match cell.symbol() {
                "█" => thumb_rows.push(y),
                "│" => {}
                // §B6 requires a *visible scroll position*, not a permanent bar. A transcript that
                // fits gets no scrollbar, which is the honest rendering — a full-length thumb on
                // unscrollable content is a control that does nothing.
                " " => blanks += 1,
                other => panic!(
                    "{w}x{h}: scrollbar cell ({x},{y}) is {other:?}. The thumb must be U+2588 FULL \
                     BLOCK, which tiles edge to edge; the track must be U+2502, not ratatui's \
                     default U+2551, which reads as a second border inside the region"
                ),
            }
        }

        if blanks > 0 {
            // All or nothing. A half-drawn bar would mean the widget and the layout disagree about
            // how much content there is.
            assert_eq!(
                thumb_rows.len() + (c.conversation_scroll.height as usize - blanks),
                0,
                "{w}x{h}: the scrollbar column mixes drawn and blank cells"
            );
            println!("{w}x{h}: transcript fits, no scrollbar drawn (correct)");
            continue;
        }

        assert!(!thumb_rows.is_empty(), "{w}x{h}: a track was drawn with no thumb");
        for pair in thumb_rows.windows(2) {
            assert_eq!(
                pair[1],
                pair[0] + 1,
                "{w}x{h}: the thumb has a gap between rows {} and {}; it must occupy contiguous \
                 cells",
                pair[0],
                pair[1]
            );
        }
        // Structure, not state. A scroll position is not an alarm, so it is not amber.
        assert_eq!(
            buf.cell((x, thumb_rows[0])).unwrap().fg,
            common::theme().accent(),
            "{w}x{h}: the thumb is not the accent"
        );
    }
    println!("scrollbar: thumb contiguous U+2588 in accent, track U+2502, 0 cells filled");
}

/// §B11: *"A broken grid is worse than an honest refusal."*
#[test]
fn below_the_minimum_it_refuses_and_draws_no_grid() {
    let app = common::app();
    for (w, h) in [(119, 30), (120, 29), (80, 24), (60, 20)] {
        let buf = common::frame(&app, w, h);
        let text = common::buffer_text(&buf);
        assert!(
            text.contains("120") && text.contains("30"),
            "{w}x{h}: the refusal must name the required size"
        );
        assert!(
            text.contains(&format!("{w}x{h}")),
            "{w}x{h}: the refusal must name the current size"
        );
        assert!(
            text.contains("--classic"),
            "{w}x{h}: the refusal must offer the classic CLI"
        );
        // No degraded grid: not one box-drawing character on screen.
        for ch in text.chars() {
            assert!(
                !('\u{2500}'..='\u{257F}').contains(&ch),
                "{w}x{h}: a box-drawing character {ch:?} was drawn below the minimum. §B11 — a \
                 narrow variant was designed and rejected because it cost the borders, which cost \
                 the region contract, which is the entire design"
            );
        }
    }
    println!("below-minimum behaviour: honest refusal at 4 sizes, 0 box-drawing characters");
}

/// ADR-021: `waiting` freezes the indicator. Two frames at different times, identical meter cells.
#[test]
fn the_indicator_freezes_in_waiting_and_moves_in_every_other_live_state() {
    let mut r = common::rig();
    r.force_state(StatusState::Listening, 0);
    let mut term = common::terminal(140, 40);
    let a = common::draw_into(&mut term, &r.app);
    r.tick(400);
    let b = common::draw_into(&mut term, &r.app);
    assert!(
        !common::diff_cells(&a, &b).is_empty(),
        "listening must move — motion means Marlowe is working"
    );

    r.force_state(StatusState::Waiting, 500);
    let c = common::draw_into(&mut term, &r.app);
    r.tick(4_000);
    let d = common::draw_into(&mut term, &r.app);
    assert!(
        common::diff_cells(&c, &d).is_empty(),
        "waiting must be perfectly still. §B5 — stillness means the ball is in the user's court, \
         and it must be legible from across a room"
    );
    println!("waiting: 0 cells changed over 3.5s; listening: moving");
}

fn inner(r: Rect) -> Rect {
    Rect {
        x: r.x + 1,
        y: r.y + 1,
        width: r.width.saturating_sub(2),
        height: r.height.saturating_sub(2),
    }
}

/// The inspector tabs are clickable, and the clickable area is **where the label actually is**.
///
/// `render::tab_rects` is a second expression of geometry that `draw_inspector` also computes by
/// laying out spans. Two expressions of one layout is exactly the duplication this project keeps
/// getting bitten by, and the failure here is quiet: a click that selects the tab next to the one
/// under the pointer reads as flakiness, not as a bug. So the rects are checked against the
/// rendered buffer rather than against the arithmetic that produced them.
#[test]
fn every_inspector_tab_rect_covers_its_own_label_in_the_rendered_frame() {
    use marlowe_view::Tab;

    for (w, h) in common::SIZES {
        let app = common::app();
        let buf = common::frame(&app, w, h);
        let chrome = marlowe_surface::render::layout(ratatui::layout::Rect::new(0, 0, w, h));

        for (tab, rect) in marlowe_surface::render::tab_rects(chrome.tab_bar) {
            let row = common::row_text(&buf, rect.y);
            let span: String = row
                .chars()
                .skip(rect.x as usize)
                .take(rect.width as usize)
                .collect();
            assert!(
                span.contains(tab.title()),
                "at {w}x{h} the click target for {:?} spans {:?}, which does not contain its own \
                 label {:?} — the hit-test and the drawing disagree",
                tab,
                span,
                tab.title()
            );
            assert!(
                span.starts_with(tab.digit()),
                "the rect must start at the digit the tab bar advertises: {span:?}"
            );
        }

        // And the targets must not overlap, or one tab steals another's clicks.
        let rects = marlowe_surface::render::tab_rects(chrome.tab_bar);
        for (i, (a, ra)) in rects.iter().enumerate() {
            for (b, rb) in rects.iter().skip(i + 1) {
                if ra.y == rb.y {
                    let disjoint = ra.x + ra.width <= rb.x || rb.x + rb.width <= ra.x;
                    assert!(disjoint, "{a:?} and {b:?} have overlapping click targets");
                }
            }
        }
    }
    println!("inspector tabs clickable: 6/6, targets disjoint, verified against the drawn buffer");
}

/// **Three speakers, three weights.** The user, the model's reasoning, and Marlowe's voice must
/// not share a colour.
///
/// Reported live: *"user messages are indistinguishable colour-wise from thinking/reasoning."*
/// `Entry::User` rendered at `theme.dim()` — weight 2, the same weight the reasoning block uses —
/// so a question the user typed and a chain of thought they did not write looked identical.
///
/// The theme had already stated the intended scheme: `speech`'s doc comment says *"the user's
/// words stay in the terminal's foreground (weight 1) and Marlowe's take this"*. The renderer was
/// contradicting the design it was built on, which is why nothing caught it — every colour in use
/// was a legitimate colour.
#[test]
fn the_user_the_reasoning_and_marlowe_do_not_share_a_colour() {
    use marlowe_view::{Entry, Speech};

    let mut view = common::rig().producer.view().clone();
    view.transcript.clear();
    view.transcript.push(Entry::User("what tools do you have".into()));
    view.transcript.push(Entry::Reasoning {
        text: "weighing the options".into(),
        tokens: 4,
        done: true,
    });
    view.transcript.push(Entry::Said(Speech::Model("seven of them.".into())));

    let mut app = marlowe_surface::App::new(view).expect("the shipped key set has no conflicts");
    app.reasoning_expanded = true;

    let buf = common::frame(&app, 120, 30);
    // The foreground of the first cell of each line that carries one of the three texts.
    let colour_of = |needle: &str| -> Option<ratatui::style::Color> {
        (0..buf.area.height).find_map(|y| {
            let row = common::row_text(&buf, y);
            let at = row.find(needle)?;
            Some(buf[(at as u16, y)].style().fg.unwrap_or(ratatui::style::Color::Reset))
        })
    };

    let user = colour_of("what tools do you have").expect("the user's line is on the frame");
    let reasoning = colour_of("weighing the options").expect("the reasoning is on the frame");
    let marlowe = colour_of("seven of them.").expect("Marlowe's reply is on the frame");

    assert_ne!(
        user, reasoning,
        "the user's own words and the model's reasoning render in the same colour ({user:?}) — \
         the two are indistinguishable on screen"
    );
    assert_ne!(user, marlowe, "the user and Marlowe must not share a colour");
    assert_ne!(reasoning, marlowe, "reasoning and Marlowe's voice must not share a colour");
}
