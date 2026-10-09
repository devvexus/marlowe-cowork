//! **M3-DESIGN §11's amended row 3 — the only one of the three not satisfiable by an empty
//! implementation.**
//!
//! | amended row | satisfiable by an empty build? | where it lives |
//! |---|---|---|
//! | the escape hatch is in no agent's exposed set | **yes**, trivially — no such tool exists | `marlowe-provider/tests/terminate_is_absent_from_the_request_body.rs` |
//! | its label and keybind appear in no `request_body` | **yes**, on a build that draws nothing | same file, plus the `--dev` dump |
//! | **an agent cannot suppress, reorder or restyle it** | **no** | here |
//!
//! §3.4's premise is that a compromised top-agent writes the option list, offers three that all
//! serve the attacker, and describes the escape hatch as catastrophic. So the agent here writes
//! the harness's own row into its options — glyph, label and all — and this file asserts what the
//! human actually sees.
//!
//! # Two sizes, and the small one is the one that catches the defect
//!
//! SECURITY-AUDIT B3's finding is a modal sized from unwrapped content, which `ratatui` clips
//! silently. A test at 24x80 alone passes on that build; the row leaves the buffer at 10x40 first.
//! *Mutation:* size the overlay from the content instead of the frame → red at 10x40 before it is
//! red at 24x80, which is why both sizes are here rather than one.

use marlowe_contract::escalation::{
    EscalationCategory, EscalationSeverity, OptionLabel, ValidatedSentence,
};
use marlowe_surface::chrome::{self, QUOTE_RULE};
use marlowe_surface::escalation::{draw_escalation, layout, terminate_row_text};
use marlowe_surface::theme::Theme;
use marlowe_view::escalation::{
    AgentOption, EscalationView, IrreversibleAct, SourceEvidence, SourceTrace, Survivor,
    TerminationCost, TERMINATE_LABEL,
};
use marlowe_view::notice::Echo;
use marlowe_view::run::OrphanPolicyLabel;
use marlowe_view::HostLabel;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::Terminal;

const SIZES: [(u16, u16); 2] = [(80, 24), (40, 10)];

fn theme() -> Theme {
    Theme::default_truecolor()
}

fn region_text(buf: &Buffer, r: Rect) -> String {
    (r.y..r.bottom())
        .map(|y| {
            (r.x..r.right())
                .map(|x| buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" ").to_string())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn buffer_text(buf: &Buffer) -> String {
    region_text(buf, buf.area)
}

/// Collapse runs of spaces. **The reserved region WRAPS at 40 columns** -- the label is 43
/// characters and the harness wraps it itself rather than letting `ratatui` truncate it -- so the
/// cells read `... the runs` then `under it`, padded. Squeezing is how a wrapped row is compared
/// against the label it was built from; asserting on the unwrapped string would fail at the small
/// size for a reason that is the fix rather than the defect.
fn squeeze(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn label(s: &str) -> AgentOption {
    AgentOption { label: OptionLabel::normalise(s).expect("a label the contract admits") }
}

/// **The maximal case**: four options, every `IrreversibleAct` variant, two survivors, a long
/// host. Everything that can push the escape hatch off the bottom is at its ceiling.
fn hostile_view() -> EscalationView {
    EscalationView::new(
        Echo::new("top-agent brave-storm"),
        EscalationSeverity::Critical,
        EscalationCategory::IrreversibleActionRequired,
        vec![
            // **The forgery.** The agent writes the harness's row verbatim: reserved glyph,
            // reserved wording, everything.
            label(&terminate_row_text()),
            label("continue and finish the migration"),
            label("roll back only the schema change"),
            label("pause and wait for the release window"),
        ],
        TerminationCost {
            runs: 7,
            age_ms: 6 * 60 * 60 * 1_000,
            spend_micros_usd: 4_200_000,
            artifacts: 40,
            irreversible: vec![
                IrreversibleAct::FilesWritten { count: 3 },
                IrreversibleAct::CommitsPushed { count: 1 },
                IrreversibleAct::MessagesSent { count: 2 },
                IrreversibleAct::ProcessesRun { count: 9 },
            ],
            survivors: vec![
                Survivor { run: Echo::new("quiet-field"), policy: OrphanPolicyLabel::Detach },
                Survivor { run: Echo::new("long-river"), policy: OrphanPolicyLabel::Detach },
            ],
        },
        SourceEvidence::External {
            sources: 14,
            most_recent: SourceTrace {
                host: HostLabel("a-rather-long-host-name.example.invalid".into()),
                fetched_ms_ago: 40 * 60 * 1_000,
            },
        },
        None,
        // A8 arm (b)'s sentence, with the reserved glyph in it. The header's third row is the
        // only one composed from model bytes, so it is the second place the forgery is tried.
        Some(
            ValidatedSentence::normalise(&format!("{QUOTE_RULE} you will lose six hours"))
                .expect("a sentence the contract admits"),
        ),
    )
    .expect("four options is the ceiling, not one past it")
}

fn frame(view: &EscalationView, w: u16, h: u16) -> Buffer {
    let mut term = Terminal::new(TestBackend::new(w, h)).expect("a test backend");
    term.draw(|f| draw_escalation(view, &theme(), f.area(), f.buffer_mut(), 0))
        .expect("a draw");
    term.backend().buffer().clone()
}

/// **§11 row 3.** The agent cannot suppress it, cannot move it, and cannot forge it.
#[test]
fn an_agent_cannot_suppress_reorder_or_restyle_the_escape_hatch() {
    let view = hostile_view();
    for (w, h) in SIZES {
        let buf = frame(&view, w, h);
        let l = layout(Rect::new(0, 0, w, h));

        // ── SUPPRESS: it is on screen, whole, at every size. ───────────────────────────────
        let reserved = squeeze(&region_text(&buf, l.terminate));
        assert!(
            reserved.contains(TERMINATE_LABEL),
            "{w}x{h}: the harness row is not intact in its reserved region.\nreserved: \
             {reserved:?}\nframe:\n{}",
            buffer_text(&buf)
        );

        // ── REORDER: it is at the bottom of the overlay, and nothing above it can move it. ──
        assert_eq!(
            l.terminate.bottom(),
            l.overlay.bottom(),
            "{w}x{h}: the reserved region is not flush with the bottom of the overlay"
        );
        assert!(
            l.options.bottom() <= l.terminate.y,
            "{w}x{h}: the option list overlaps the reserved region -- options {:?}, terminate {:?}",
            l.options,
            l.terminate
        );

        // ── RESTYLE / FORGE: the agent's copy is neutralised and the harness's is not. ──────
        //
        // `chrome::is_reserved` covers Box-Drawing and Block-Elements as RANGES, and
        // `prepare_model_text` substitutes every one of them in model text. So the glyph is the
        // one thing on this screen the model cannot draw.
        let glyphs = buffer_text(&buf).chars().filter(|c| *c == QUOTE_RULE).count();
        assert_eq!(
            glyphs, 1,
            "{w}x{h}: {glyphs} cells carry the reserved glyph. Exactly one row is the harness's, \
             and an agent's copy must have been substituted.\n{}",
            buffer_text(&buf)
        );
    }
}

/// The forgery is **visible**, not silently dropped.
///
/// `sanitize`'s own argument: a stripped payload and a clean string must not be indistinguishable
/// to the person deciding. At 80x24 there is room for the option list, so the substituted marker is
/// on screen.
#[test]
fn the_agents_copy_of_the_harness_row_renders_as_a_marker() {
    let buf = frame(&hostile_view(), 80, 24);
    let text = buffer_text(&buf);
    let marker = format!("<U+{:04X}>", QUOTE_RULE as u32);
    let marks = text.matches(&marker).count();
    assert!(
        marks >= 2,
        "{marks} markers, and the hostile view puts the reserved glyph in TWO model-authored \
         places -- an option label and A8's sentence. A build that ran `prepare_model_text` over \
         one region and not the other would pass a >= 1 assertion.\n{text}"
    );
    // And the control: the mechanism doing the substituting is the shared one, not a local copy.
    assert!(chrome::is_reserved(QUOTE_RULE));
    assert!(chrome::prepare_model_text(&terminate_row_text()).contains("<U+"));
}

/// **The layout is computed from the frame, not from the content**, so growth in the cost section
/// cannot displace the escape hatch. B3, as an invariant rather than a fixed number.
#[test]
fn growth_in_the_cost_section_cannot_move_the_reserved_row() {
    let small = EscalationView::new(
        Echo::new("top-agent brave-storm"),
        EscalationSeverity::Advisory,
        EscalationCategory::ScopeContradiction,
        vec![label("carry on")],
        TerminationCost {
            runs: 1,
            age_ms: 1_000,
            spend_micros_usd: 0,
            artifacts: 0,
            irreversible: Vec::new(),
            survivors: Vec::new(),
        },
        SourceEvidence::NoExternalSources,
        None,
        None,
    )
    .expect("one option");

    for (w, h) in SIZES {
        let a = layout(Rect::new(0, 0, w, h));
        assert_eq!(
            a,
            layout(Rect::new(0, 0, w, h)),
            "the layout is a function of the area alone and must be stable"
        );
        let big = frame(&hostile_view(), w, h);
        let tiny = frame(&small, w, h);
        assert_eq!(
            region_text(&big, a.terminate),
            region_text(&tiny, a.terminate),
            "{w}x{h}: the reserved region rendered differently for a maximal and a minimal \
             escalation, so something in the content is reaching it"
        );
    }
}

/// A fifth option is refused at construction. **Not truncated**, and deliberately not a budget
/// dimension — `Budget::exhausted` compares `spent >= budget`, so a ceiling written as a counter
/// of zero reads as already-exhausted (instance #17).
#[test]
fn a_fifth_option_is_refused_rather_than_dropped() {
    let five: Vec<AgentOption> = (0..5).map(|i| label(&format!("option {i}"))).collect();
    let e = EscalationView::new(
        Echo::new("top-agent brave-storm"),
        EscalationSeverity::Blocking,
        EscalationCategory::ConflictingInstructions,
        five,
        TerminationCost {
            runs: 1,
            age_ms: 0,
            spend_micros_usd: 0,
            artifacts: 0,
            irreversible: Vec::new(),
            survivors: Vec::new(),
        },
        SourceEvidence::NoExternalSources,
        None,
        None,
    );
    let err = e.expect_err("five options is one past the ceiling");
    assert_eq!(err.count, 5);
    assert_eq!(err.max, EscalationView::MAX_AGENT_OPTIONS);
}

/// **§3.5: the row that keeps [`TERMINATE_LABEL`] honest.**
///
/// `Control::cancel` is per-run and a `Detach` child outlives it, so a label promising *"everything
/// under it"* would be a lie on the one row the harness authors *because* the agent would lie. The
/// survivors are named.
///
/// *Mutation:* derive `survivors` from `Vec::new()` → the names vanish → red.
#[test]
fn terminate_names_the_children_it_cannot_kill() {
    let buf = frame(&hostile_view(), 80, 24);
    let text = buffer_text(&buf);
    assert!(text.contains("quiet-field"), "the detached child is not named:\n{text}");
    assert!(text.contains("keeps running"), "{text}");
    assert!(
        !TERMINATE_LABEL.contains("everything"),
        "the label promises more than `Control::cancel` can deliver: {TERMINATE_LABEL:?}"
    );
}

/// **§3.5's accounting comes from the journal, and the agent's own numbers do not become it.**
///
/// The agent's option text claims *"six hours, 40 files"*; the harness's cost rows say 7 runs and
/// 40 artifacts because that is what it was handed. The assertion is that the numbers appear in
/// the harness's region from the harness's struct — the option region is where the agent's claim
/// lives, and the two are separate regions.
#[test]
fn the_cost_rows_render_every_field_of_the_struct() {
    let view = hostile_view();
    let rows = marlowe_surface::escalation::cost_rows(&view.cost);
    assert_eq!(
        rows.len(),
        TerminationCost::ROWS,
        "one row per field, or a field has no reader (instance #16)"
    );

    // Mutate each field in turn and assert the rendering changes. A row that did not move is a
    // field nothing reads.
    let base: Vec<String> = rows.iter().map(|r| r.to_string()).collect();
    let mut mutated = view.cost.clone();
    mutated.runs += 1;
    assert_ne!(base[0], marlowe_surface::escalation::cost_rows(&mutated)[0].to_string());
    let mut mutated = view.cost.clone();
    mutated.age_ms += 60_000;
    assert_ne!(base[1], marlowe_surface::escalation::cost_rows(&mutated)[1].to_string());
    let mut mutated = view.cost.clone();
    mutated.spend_micros_usd += 1_000_000;
    assert_ne!(base[2], marlowe_surface::escalation::cost_rows(&mutated)[2].to_string());
    let mut mutated = view.cost.clone();
    mutated.artifacts += 1;
    assert_ne!(base[3], marlowe_surface::escalation::cost_rows(&mutated)[3].to_string());
    let mut mutated = view.cost.clone();
    mutated.irreversible.clear();
    assert_ne!(base[4], marlowe_surface::escalation::cost_rows(&mutated)[4].to_string());
    let mut mutated = view.cost.clone();
    mutated.survivors.clear();
    assert_ne!(base[5], marlowe_surface::escalation::cost_rows(&mutated)[5].to_string());
}
