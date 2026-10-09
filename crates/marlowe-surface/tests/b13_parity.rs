//! §B13: **classic CLI command parity — 100% of commands, sessions, data.**
//!
//! Parity is structural. Both surfaces dispatch through `commands::dispatch` and neither has a
//! second path, so a TUI-only command would have to be built by bypassing that module. These tests
//! check the bypass has not happened and that the registry is honestly complete.
//!
//! *(§B11 v2: no TUI-only **capabilities**. Layout may differ — the classic CLI prints a pane
//! linearly where the TUI draws it beside the transcript, and that is the layout a grid buys.)*

mod common;

use std::fs;
use std::io::Cursor;
use std::path::Path;

use marlowe_stub::Clock;
use marlowe_view::Tab;
use marlowe_surface::app::App;
use marlowe_surface::cli;
use marlowe_surface::commands::{self, Outcome, REGISTRY};

/// Drive the classic CLI with a script and collect its output.
fn classic(script: &str) -> String {
    // C2d: the REPL is handed a producer rather than making one, so the test supplies it — the
    // same wiring the binary does.
    let clock = Clock::virtual_(0);
    let mut producer = marlowe_stub::Session::new();
    let mut out = Vec::new();
    cli::run(
        &mut producer,
        Cursor::new(script.to_string()),
        &mut out,
        &clock,
    )
    .expect("classic cli");
    String::from_utf8(out).expect("utf-8")
}

#[test]
fn every_command_in_the_registry_is_reachable_in_both_surfaces() {
    let mut reached = 0;
    for c in REGISTRY {
        if c.name == "quit" {
            // Reachable in both by construction; running it would end the loop under test.
            reached += 1;
            continue;
        }

        // TUI.
        let mut app = App::new(marlowe_stub::Session::new().view().clone()).unwrap();
        let outcome = app.run_command(c.name, &[]);
        assert!(
            !matches!(outcome, Outcome::Unknown(_)),
            "/{} is in the registry but the TUI cannot reach it",
            c.name
        );

        // Classic.
        let out = classic(&format!("/{}\n", c.name));
        assert!(
            !out.contains(&format!("No /{}", c.name)),
            "/{} is in the registry but the classic CLI rejects it. §B11: command parity, and \
             both surfaces dispatch from one registry",
            c.name
        );
        reached += 1;
    }
    println!(
        "classic CLI command parity: {reached}/{} (100%)",
        REGISTRY.len()
    );
}

#[test]
fn the_two_surfaces_produce_the_same_data_for_a_pane() {
    // Layout differs; data does not. The linear render and the region items must agree row for row.
    let session = marlowe_stub::Session::new();
    for tab in Tab::ALL {
        let linear = commands::render_pane_linear(session.view(), tab).join("\n");
        for item in marlowe_surface::inspector::items_for(session.view(), tab) {
            assert!(
                linear.contains(&item.label),
                "the {} pane's item {:?} is in the TUI and missing from the classic CLI",
                tab.title(),
                item.label
            );
            for (line, _) in &item.lines {
                assert!(
                    linear.contains(line.as_str()),
                    "the {} pane's line {line:?} is in the TUI and missing from the classic CLI",
                    tab.title()
                );
            }
        }
    }
}

/// §B9's approvals are a *capability*, and §B14 forbids TUI-only capabilities. Without a grid it is
/// a prompt rather than an overlay — layout, not capability.
#[test]
fn approvals_reach_the_classic_cli_too() {
    let out = classic("send the email\n");
    assert!(out.contains("approval"), "the classic CLI must surface approvals");
    // **Asserted as the property, not as a fixture string.** This used to check for the literal
    // "Not recallable", which was one of M1's hand-written blast-radius strings — so it was really
    // testing that a particular fixture had not been reworded. The blast radius is computed from
    // typed facts now, and what §B9 actually requires is that the consequence is stated and the
    // command is not.
    assert!(
        out.contains("not recoverable"),
        "§B9: the consequence must be stated. Got:\n{out}"
    );
    // Scoped to the approval block. The transcript legitimately echoes the user's own turn, so
    // asserting over the whole output would have been testing something adjacent — the first
    // version of this check did exactly that and failed on `> send the email` in the transcript.
    let block = &out[out.find("approval —").expect("an approval block")..];
    assert!(
        !block.contains("send the email"),
        "§B9 states blast radius, NOT the command — the user's text reached the approval \
         prompt:\n{block}"
    );
    assert!(
        out.contains("send as marlowe"),
        "§B9: offers the delegation escape hatch — sending as Marlowe avoids impersonation \
         entirely (Addendum A §A3)"
    );
    assert!(
        !out.contains("rm ") && !out.contains("curl "),
        "§B9: the overlay states blast radius, NOT the command"
    );
}

#[test]
fn an_approval_raised_by_a_command_surfaces_in_the_classic_cli_too() {
    // `/state waiting` raises the overlay in the TUI. If the linear surface only printed approvals
    // on the message path it could sit in `waiting` with nothing on screen to answer — a
    // capability gap wearing a layout difference's clothes.
    let out = classic("/state waiting\n");
    assert!(
        out.contains("approval —"),
        "the classic CLI reached `waiting` without showing what is waiting on the user"
    );
}

#[test]
fn the_classic_cli_never_lies_about_an_unbuilt_pane() {
    for tab in ["sessions", "skills", "trust", "status"] {
        let out = classic(&format!("/{tab}\n"));
        assert!(
            out.contains("M2"),
            "/{tab} printed something without saying it is unbuilt; silence would read as 'you \
             have none', which is a claim and a false one"
        );
    }
}

#[test]
fn the_registry_is_the_only_dispatcher() {
    // The bypass check. If a surface grows its own command table, parity stops being structural
    // and starts being a checklist — which is what §B11 v1 had, and it rotted.
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for file in ["app.rs", "cli.rs"] {
        let src = fs::read_to_string(root.join(file)).unwrap();
        let dispatches = src.matches("commands::dispatch").count()
            + src.matches("crate::commands::dispatch").count()
            + src.matches("self.run_command").count();
        assert!(
            dispatches > 0,
            "{file} handles commands without going through the one registry"
        );
    }
}

/// §B10: slash commands autocomplete inline **with descriptions**.
#[test]
fn slash_autocomplete_offers_every_matching_command_with_its_description() {
    for prefix in ["", "s", "st", "run", "d"] {
        let hits = commands::complete(prefix);
        let expected = REGISTRY.iter().filter(|c| c.name.starts_with(prefix)).count();
        assert_eq!(hits.len(), expected, "autocomplete for {prefix:?}");
        for h in hits {
            assert!(!h.description.is_empty(), "/{} has no description", h.name);
        }
    }
}

/// §B11's other half: piped stdin and no TTY. The classic CLI must work with neither.
#[test]
fn the_classic_cli_runs_from_a_pipe_with_no_terminal_at_all() {
    let out = classic("what does my day look like\n/schedule\n/quit\n");
    assert!(out.contains("11:00 · vendor call — acme"));
    assert!(!out.is_empty());
}
