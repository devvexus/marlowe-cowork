//! **The thinking block's head line reports tokens, and the count is carried rather than derived.**
//!
//! It reported `text.len()` — characters. Characters are the number a renderer can compute for
//! itself, and that is exactly what made them the wrong unit: what a person wants to know is what
//! the model spent, and a renderer has no tokenizer. Dividing by four would have replaced a number
//! that was honestly the wrong quantity with one that looked like the right quantity and was an
//! estimate.
//!
//! So the count comes from the provider, which is the only component that can see the engine's own
//! counter, and everything below it **adds**. These tests pin the two rules that makes possible:
//!
//! 1. A reasoning chunk carries a token delta, and the block accumulates it.
//! 2. **An empty chunk with a non-zero count settles a block; it never opens one.** That is how a
//!    provider reports what the engine generated and never streamed — the merged tokens, the
//!    closing delimiter — which arrives after the last text and often after the block has been
//!    closed by an answer token.

use marlowe_daemon::{apply_events, view_from_status, Event, StatusReport};
use marlowe_view::{Entry, Speech};

fn view() -> marlowe_view::SessionView {
    view_from_status(&StatusReport {
        version: "0.1.0".into(),
        workspace: "/ws".into(),
        model: "marlowe-red:9b".into(),
        model_disclosure: "local".into(),
        degraded: None,
        rerank_provider: "cpu".into(),
        model_provider: "ollama".into(),
        live_runs: 0,
        models: Vec::new(),
        announcements: Vec::new(),
        uptime_ms: 0,
    })
}

/// Every reasoning block in the transcript, as `(text, tokens, done)`.
fn blocks(v: &marlowe_view::SessionView) -> Vec<(String, u64, bool)> {
    v.transcript
        .iter()
        .filter_map(|e| match e {
            Entry::Reasoning { text, tokens, done } => Some((text.clone(), *tokens, *done)),
            _ => None,
        })
        .collect()
}

#[test]
fn a_blocks_token_count_is_the_sum_of_what_the_provider_reported() {
    let mut v = view();
    apply_events(
        &mut v,
        &[
            Event::Reasoning { delta: "Two ".into(), tokens: 1 },
            Event::Reasoning { delta: "plus two".into(), tokens: 2 },
        ],
    );

    // Two chunks, three tokens — the case that exists because a chunk is not a token on every
    // engine. Summing is the only reading that survives it.
    assert_eq!(blocks(&v), vec![("Two plus two".to_string(), 3, false)]);
}

#[test]
fn the_settlement_joins_the_block_it_settles_even_after_an_answer_closed_it() {
    let mut v = view();
    apply_events(
        &mut v,
        &[
            Event::Reasoning { delta: "Two plus two".into(), tokens: 3 },
            // The first answer token closes the block.
            Event::Text { delta: "4".into() },
            // …and the engine's own count arrives after that, at the end of the call.
            Event::Reasoning { delta: String::new(), tokens: 4 },
        ],
    );

    assert_eq!(
        blocks(&v),
        vec![("Two plus two".to_string(), 7, true)],
        "the settlement must land on the closed block, not open a second one"
    );
    // And it must not have disturbed the answer.
    assert!(
        v.transcript.iter().any(|e| matches!(e, Entry::Said(Speech::Model(t)) if t == "4")),
        "the reply is still in the transcript: {:?}",
        v.transcript
    );
}

/// **The control, and it is the reason the settlement is a special case at all.**
///
/// A call that produced no reasoning still generates a token it never streams — the stop token.
/// If an empty chunk opened a block, every answer in the product would grow a thinking line under
/// it reading `thought 1 token`, describing a thought that never happened.
#[test]
fn an_empty_chunk_never_opens_a_thinking_block() {
    let mut v = view();
    apply_events(
        &mut v,
        &[
            Event::Text { delta: "4".into() },
            Event::Reasoning { delta: String::new(), tokens: 1 },
        ],
    );

    assert!(blocks(&v).is_empty(), "a thought was invented: {:?}", v.transcript);
}

/// **Retracted speech takes its cost with it.**
///
/// A `</think>` arriving late proves text already rendered as speech was reasoning. The transcript
/// moves the text; the tokens have to move too, or the block reports a figure for prose it no
/// longer holds and none for the prose it just gained.
#[test]
fn retracted_speech_carries_its_tokens_into_the_thinking_block() {
    let mut v = view();
    apply_events(
        &mut v,
        &[
            Event::Reasoning { delta: "weighing".into(), tokens: 2 },
            Event::Text { delta: "still weighing, actually".into() },
            Event::SpeechRetracted { tokens: 5 },
        ],
    );

    assert_eq!(
        blocks(&v),
        vec![("weighingstill weighing, actually".to_string(), 7, false)],
        "the retracted speech's tokens must arrive with its text"
    );
}

/// **`▸ thinking… 226 tokens` on a turn that had already answered.** Reported from a live session.
///
/// The head line branches on `done`, and the only thing that set it was `Event::Text` checking
/// `transcript.last_mut()` — so it closed a reasoning block **only while that block was still the
/// last entry**. Every turn with a tool in it has a second one. Off the wire, verbatim:
///
/// ```text
/// R×58  R0  TOOL(running) TOOL(ok)  R×297  T×9  R0  DONE
/// ```
///
/// The tool line lands on the first block, the second model call opens a new one, the answer closes
/// *that*, and the first sits claiming work in progress for the rest of the session.
///
/// **A single-call turn was fine**, which is why nothing caught it: the block was still last when
/// the answer arrived. The unit change did not cause this and does not depend on it — the same turn
/// read `thinking… 1688 characters` before — but a stale count of characters is one more slightly
/// wrong number on a screen, and a stale `thinking…` under a finished answer is a claim.
#[test]
fn a_reasoning_block_a_tool_line_landed_on_is_still_closed_by_the_answer() {
    let tool = |state: &str| Event::Tool {
        id: 1,
        verb: "read".into(),
        target: "README.md".into(),
        state: state.into(),
        summary: "48 lines".into(),
        detail: None,
    };
    let mut v = view();
    apply_events(
        &mut v,
        &[
            Event::Reasoning { delta: "which file".into(), tokens: 58 },
            Event::Reasoning { delta: String::new(), tokens: 4 },
            tool("running"),
            tool("ok"),
            Event::Reasoning { delta: "now the first line".into(), tokens: 297 },
            Event::Text { delta: "It is a heading.".into() },
            Event::Reasoning { delta: String::new(), tokens: 6 },
        ],
    );

    assert_eq!(
        blocks(&v),
        vec![
            ("which file".to_string(), 62, true),
            ("now the first line".to_string(), 303, true),
        ],
        "the block the tool line landed on is still marked as thinking"
    );
}

/// **The control, and without it the line would never say `thinking…` at all.**
///
/// A live turn is folded in batches every 50 ms. The block being written to is legitimately open,
/// and a sweep that closed every block would replace one wrong state with the opposite one — a
/// counter that ticks upward under the word `thought`.
#[test]
fn the_block_still_being_written_to_stays_open() {
    let mut v = view();
    apply_events(&mut v, &[Event::Reasoning { delta: "weigh".into(), tokens: 1 }]);
    apply_events(&mut v, &[Event::Reasoning { delta: "ing".into(), tokens: 1 }]);

    assert_eq!(blocks(&v), vec![("weighing".to_string(), 2, false)], "the turn is still running");
}

/// **A turn that ends without answering closes its thought anyway.**
///
/// The nudge budget runs out, or the user interrupts: reasoning, and no prose after it. Nothing is
/// still thinking once the turn is over, so `Event::Done` closes the last block too — the one case
/// the mid-turn sweep deliberately leaves alone.
#[test]
fn a_turn_that_ends_mid_thought_does_not_keep_claiming_to_be_thinking() {
    let mut v = view();
    apply_events(
        &mut v,
        &[
            Event::Reasoning { delta: "weighing".into(), tokens: 9 },
            Event::Done {
                outcome: "paused".into(),
                detail: String::new(),
                spend_micros_usd: 0,
                elapsed_ms: 400,
            },
        ],
    );

    assert_eq!(blocks(&v), vec![("weighing".to_string(), 9, true)]);
}
