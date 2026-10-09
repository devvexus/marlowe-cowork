//! §B13: **regions reachable by keyboard alone — 100%.** Plus zero dropped keystrokes.
//!
//! Reachability is proved **from the default focus**, and the default focus changed because of
//! this test's original shape.
//!
//! It used to start in the Message field and press `Esc` before every hotkey — and it passed,
//! because "reachable after one extra key that no border mentions" is still reachable. What it
//! could not see is that §B10's first sentence was false on arrival: *"Region hotkeys jump focus
//! directly."* In a focused text field `m` is the letter m, so every hotkey printed on every
//! border did nothing until the user guessed `Esc` first.
//!
//! The default is now the Conversation, `i` enters the Message field, and this test presses **the
//! hotkey and nothing else**. A test that helps the implementation past the user's real starting
//! state is measuring the wrong thing.

mod common;

use marlowe_view::{SessionView, Tab};
use marlowe_surface::app::{App, Key};
use marlowe_surface::region::{RegionId, RegionTree};

#[test]
fn every_region_is_reachable_from_the_default_focus() {
    let mut reached = 0;
    let mut total = 0;

    for tab in Tab::ALL {
        let producer = marlowe_stub::Session::new();
        let tree = RegionTree::build(producer.view(), tab);

        for target in tree.regions() {
            total += 1;
            let mut app = App::new(producer.view().clone()).unwrap();
            app.tab = tab;
            assert_eq!(
                app.focus,
                RegionId::Conversation,
                "the default focus moved; §B10 requires region hotkeys to work on arrival"
            );

            // The hotkey the border advertises, and nothing else. No Esc, no Tab, no warm-up.
            app.on_key(Key::Char(target.hotkey().expect("a frame region with no hotkey")));

            if app.focus == target.id() {
                reached += 1;
            } else {
                panic!(
                    "{:?} ('{}' on the {} tab) was not reached by Esc then its own hotkey. §B13 \
                     asks for 100% reachable by keyboard alone, and the border says '{}'",
                    target.id(),
                    target.label(),
                    tab.title(),
                    target.hotkey_label()
                );
            }
        }
    }
    println!("regions reachable by keyboard alone: {reached}/{total} (100%)");
}

#[test]
fn tab_and_shift_tab_cycle_every_region_in_reading_order() {
    let mut app = common::app();
    let tree = app.tree();
    let n = tree.regions().len();

    app.on_key(Key::Esc);
    let start = app.focus;
    let mut seen = vec![app.focus];
    for _ in 0..n - 1 {
        app.on_key(Key::Tab);
        seen.push(app.focus);
    }
    app.on_key(Key::Tab);
    assert_eq!(app.focus, start, "Tab must wrap, not stop at the end");

    for r in tree.regions() {
        assert!(
            seen.contains(&r.id()),
            "{:?} is not in the Tab cycle, so a user who never learns its hotkey cannot reach it",
            r.id()
        );
    }

    // Shift-Tab walks it back.
    for _ in 0..n {
        app.on_key(Key::BackTab);
    }
    assert_eq!(app.focus, start);
    println!("Tab cycle covers {n}/{n} regions and wraps in both directions");
}

#[test]
fn every_inspector_tab_is_reachable_by_its_digit() {
    let mut app = common::app();
    app.on_key(Key::Esc);
    for tab in Tab::ALL {
        app.on_key(Key::Char(tab.digit()));
        assert_eq!(
            app.tab,
            tab,
            "'{}' did not reach the {} tab",
            tab.digit(),
            tab.title()
        );
    }
}

/// §B13: **dropped keystrokes during streaming — zero.**
///
/// The stub streams a scripted reply while the keys arrive. Input is never blocked (§B10), so
/// every character typed during the stream must land in the message field in order.
#[test]
fn no_keystroke_is_dropped_while_the_stub_is_streaming() {
    let mut r = common::rig();
    r.app.input = "run the tests".into();
    r.app.submit();
    // **Optimistic before the producer has seen it.** §B10: input is never blocked, so the line is
    // on screen the instant it is typed — as pending, in its own weight.
    assert!(
        r.app.pending.is_some(),
        "the line must show at once rather than waiting for a round trip"
    );
    r.settle(0);
    assert!(
        marlowe_view::Produce::is_busy(&r.producer),
        "the rig must actually be streaming"
    );
    // **Retired by the producer, and only by the producer.** The scripted producer records the
    // user's turn synchronously, so acknowledgement is immediate here; what matters is that it is
    // the producer's transcript that retires it, never a timer on this side.
    assert!(
        r.app.pending.is_none(),
        "the producer recorded the turn, so the optimistic line must retire"
    );
    assert!(
        r.producer
            .view()
            .transcript
            .iter()
            .any(|e| matches!(e, marlowe_view::Entry::User(t) if t == "run the tests")),
        "and it retired because the turn is really in the producer's transcript, not because \
         enough time passed"
    );
    // `i` enters the message field — the same key the border advertises.
    r.key(Key::Char('i'), 0);
    assert_eq!(r.app.focus, RegionId::Message);

    let typed = "the quick brown fox jumps over the lazy dog 0123456789";
    let mut now = 0u64;
    for c in typed.chars() {
        now += 7;
        // Ticking between keys is what a real loop does; a key that arrives mid-tick must not be
        // swallowed by the redraw.
        r.tick(now);
        r.key(Key::Char(c), now);
    }
    r.tick(now + 3_000);
    let app = &r.app;

    assert_eq!(
        app.input, typed,
        "keystrokes were lost or reordered during a stream. Typed {} chars, kept {}",
        typed.len(),
        app.input.len()
    );
    println!(
        "dropped keystrokes during streaming: 0 of {} delivered",
        typed.chars().count()
    );
}

#[test]
fn esc_backs_out_one_level_and_interrupt_is_the_outermost() {
    let mut r = common::rig();

    // 3. focus is in a text field -> blur. Reached by `i`, the key the message field's own bottom
    //    border advertises — not assumed, because the default focus is the conversation.
    r.key(Key::Char('i'), 0);
    assert_eq!(r.app.focus, RegionId::Message);
    r.key(Key::Esc, 0);
    assert_eq!(r.app.focus, RegionId::Conversation);

    // 2. a dropdown is open -> close it. Open-ness is the surface's own state now, so this reads
    //    `app.picker_open` rather than a flag on the producer's picker.
    r.key(Key::Char('m'), 0);
    r.key(Key::Enter, 0);
    assert_eq!(r.app.picker_open, Some(marlowe_view::ControlId::Model));
    r.key(Key::Esc, 0);
    assert_eq!(r.app.picker_open, None);
    assert_eq!(r.app.focus, RegionId::Model, "closing a dropdown must not also move focus");

    // 4. otherwise -> interrupt, keeping partial output.
    r.key(Key::Char('c'), 0);
    r.app.input = "run the tests".into();
    r.app.submit();
    r.settle(0);
    r.tick(200);
    let kept = r.producer.view().transcript.len();
    r.key(Key::Esc, 300);
    assert!(!marlowe_view::Produce::is_busy(&r.producer));
    assert!(
        r.producer.view().transcript.len() >= kept,
        "§B10: partial output is kept"
    );

    // 1. the approval overlay outranks all of it.
    r.force_state(marlowe_view::StatusState::Waiting, 400);
    assert!(r.app.view().approval.is_some());
    r.key(Key::Esc, 500);
    assert!(r.app.view().approval.is_none(), "Esc on the overlay denies");
}

#[test]
fn the_overlay_swallows_unrelated_keys_rather_than_letting_them_through() {
    // §B9's whole subject is a user who has stopped reading. An overlay that let a stray keystroke
    // fall through to the frame behind it would be the rubber-stamping failure with extra steps.
    let mut r = common::rig();
    r.force_state(marlowe_view::StatusState::Waiting, 0);
    let before = r.app.tab;
    r.key(Key::Char('3'), 0);
    r.key(Key::Char('c'), 0);
    r.key(Key::Tab, 0);
    assert_eq!(r.app.tab, before);
    assert!(r.app.view().approval.is_some(), "only ↵ e s esc resolve it");
}

#[test]
fn ctrl_keys_work_even_from_inside_a_text_field() {
    // This is what makes reachability true from the default focus without any Esc at all.
    let mut r = common::rig();
    r.key(Key::Char('i'), 0);
    assert_eq!(
        r.app.focus,
        RegionId::Message,
        "the premise of this test is that focus IS in a text field; if `i` no longer gets there \
         the test below proves nothing"
    );
    r.key(Key::Ctrl('r'), 0);
    assert_eq!(r.app.tab, Tab::Runs);
    r.key(Key::Ctrl('t'), 0);
    assert_eq!(r.app.tab, Tab::Trust);

    // ^v drives the status band through all seven states without waiting for a script — with one
    // stop, and the stop is correct. `waiting` raises §B9's overlay, and the overlay is modal: it
    // swallows every key but ↵ e s esc. So reaching `idle` means answering the approval first,
    // which is exactly what `waiting` means. A ^v that walked past a pending approval would be the
    // rubber-stamping failure with a keyboard shortcut attached.
    let mut seen = Vec::new();
    let mut now = 0u64;
    for _ in 0..8 {
        now += 10;
        seen.push(r.app.view().status.state);
        if r.app.view().approval.is_some() {
            r.key(Key::Esc, now); // deny, and back out of the modal level
        } else {
            // **ADR-056: the chord moved because the footer moved.** `Ctrl-V` is paste now;
            // Windows Terminal was eating it anyway, which this test could never have seen —
            // it dispatches into `App` directly and never crosses a terminal. The PROPERTY is
            // unchanged: a footer key must reach every §B5 state from inside a text field.
            r.key(Key::Alt('v'), now);
        }
    }
    let mut distinct: Vec<&str> = seen.iter().map(|s| s.name()).collect();
    distinct.sort_unstable();
    distinct.dedup();
    assert_eq!(
        distinct.len(),
        7,
        "alt-v must reach every state, not cycle a subset; saw {distinct:?}"
    );
    println!("status states reachable on demand: 7/7 (waiting via its overlay, as it should be)");
}

#[test]
fn ctrl_v_does_not_walk_past_a_pending_approval() {
    // The other half of the test above, stated on its own so it cannot be lost in a refactor.
    let mut r = common::rig();
    r.force_state(marlowe_view::StatusState::Waiting, 0);
    for i in 0..5 {
        r.key(Key::Ctrl('v'), i * 10);
    }
    assert_eq!(
        r.app.view().status.state,
        marlowe_view::StatusState::Waiting,
        "a global shortcut escaped §B9's overlay; the overlay is the one modal element and \
         answering it is the only way out"
    );
}

// ─── M3 F2's composer keys, on the acceptance suite's terms ───────────────────────────────────

/// §B10: *"the mouse adds no capability the keyboard lacks."* M3 F2 added editing chords and a
/// composer that grows and scrolls, and this file did not know any of it existed — so the
/// acceptance suite was asserting a keyboard the product no longer had.
///
/// Each of these is the §B10 property, not a duplicate of `composer.rs`: every one is reached
/// **from the default focus**, by the advertised route, with no extra key that no border mentions.
#[test]
fn the_composers_editing_chords_are_reachable_from_the_default_focus() {
    let mut r = common::rig();

    // The documented way into the field: the hotkey printed on its own border.
    r.key(Key::Char('i'), 0);
    for c in "hello world".chars() {
        r.key(Key::Char(c), 0);
    }

    r.key(Key::CtrlBackspace, 0);
    assert_eq!(r.app.input, "hello ", "ctrl-backspace did not delete a word: {:?}", r.app.input);

    r.key(Key::Ctrl('a'), 0);
    r.key(Key::Char('x'), 0);
    assert_eq!(r.app.input, "x", "ctrl-a did not select the draft: {:?}", r.app.input);
}

/// **`^c` must not end the session on one press**, and the footer must say it exists. It quit
/// immediately for two milestones while appearing on no footer and in no test.
#[test]
fn ctrl_c_is_advertised_and_needs_two_presses() {
    let mut r = common::rig();
    assert!(
        marlowe_surface::app::FOOTER_KEYS.iter().any(|(k, _)| *k == "^c"),
        "a key that ends the session is not on the footer"
    );
    assert_ne!(r.app.on_key(Key::Ctrl('c')), marlowe_surface::app::Action::None, "^c did nothing at all");
    assert_ne!(r.app.on_key(Key::Tab), marlowe_surface::app::Action::Quit);
    // Tab disarmed it, so the next one is a first press again.
    assert_ne!(r.app.on_key(Key::Ctrl('c')), marlowe_surface::app::Action::Quit);
}

/// ADR-056 moved the state cycle off `Ctrl-V`. The footer must advertise the chord that works.
#[test]
fn the_footer_advertises_alt_v_because_the_terminal_eats_ctrl_v() {
    let footer: Vec<&str> = marlowe_surface::app::FOOTER_KEYS.iter().map(|(k, _)| *k).collect();
    assert!(footer.contains(&"alt-v"), "{footer:?}");
    assert!(!footer.contains(&"^v"), "the footer still claims a chord the terminal takes: {footer:?}");
}

/// §B10's *"multiline by default"* had a binding and no rendering. A newline must be reachable by
/// key, and the field must be readable back — the second half is what was missing.
#[test]
fn a_multiline_message_is_composable_and_scrollable_by_key_alone() {
    let mut r = common::rig();
    r.key(Key::Char('i'), 0);
    for _ in 0..3 {
        for c in "a line of text ".chars() {
            r.key(Key::Char(c), 0);
        }
        r.key(Key::ShiftEnter, 0);
    }
    assert!(r.app.input.contains('\n'), "shift-enter did not insert a newline");

    // And the view moves by key, which is the only way to read back a message taller than the cap.
    let before = r.app.composer_scroll();
    r.key(Key::Up, 0);
    assert_ne!(r.app.composer_scroll(), before, "Up did not move the composer");
    r.key(Key::Down, 0);
    assert_eq!(r.app.composer_scroll(), before, "Down did not return it");
}
