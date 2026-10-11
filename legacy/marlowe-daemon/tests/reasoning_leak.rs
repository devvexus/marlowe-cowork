//! **Reasoning must never render in the response colour.**
//!
//! Observed live on 2026-08-09, asking Marlowe for the weather in Cass Lake. The model thought
//! (1688 characters, collapsed), called `web` twice, got a failure it could not read, and then
//! kept reasoning — and the transcript rendered that reasoning as Marlowe speaking, ending with a
//! literal `</think>` printed to the user.
//!
//! Two failures from one cause:
//!
//! 1. **Reasoning shown as speech.** The channel split was made in the provider, and content that
//!    was still inside a think block went out as `TextDelta`.
//! 2. **The turn ended.** The loop completes on prose-with-no-tool-call — the correct rule — and
//!    leaked reasoning *is* prose as far as the loop can see. So the run stopped mid-thought and
//!    the user got a chain of thought as the answer.
//!
//! The fix is in one place, `marlowe_provider::ThinkSplitter`, and the second failure is a
//! consequence of the first: with reasoning routed correctly the turn's prose is **empty**, which
//! is not a completion.

use marlowe_daemon::{apply_events, view_from_status, Event, StatusReport};
use marlowe_view::{Entry, Speech};

fn view() -> marlowe_view::SessionView {
    view_from_status(&StatusReport {
        version: "0.1.0".into(),
        workspace: "/ws".into(),
        model: "qwen3.5:9b".into(),
        model_disclosure: "local".into(),
        degraded: None,
        rerank_provider: "cpu".into(),
        model_provider: "ollama".into(),
        live_runs: 0,
        models: Vec::new(),
        // Nothing has been announced into this fixture and nothing has been up.
        announcements: Vec::new(),
        uptime_ms: 0,
    })
}

fn spoken(v: &marlowe_view::SessionView) -> String {
    v.transcript
        .iter()
        .filter_map(|e| match e {
            Entry::Said(Speech::Model(t)) => Some(t.as_str()),
            _ => None,
        })
        .collect()
}

fn thought(v: &marlowe_view::SessionView) -> String {
    v.transcript
        .iter()
        .filter_map(|e| match e {
            Entry::Reasoning { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// **The Cass Lake transcript, replayed.** Reasoning streamed as speech, then retracted.
#[test]
fn retracted_speech_moves_into_the_thinking_block_and_leaves_the_transcript() {
    let mut v = view();
    apply_events(
        &mut v,
        &[
            Event::Reasoning { delta: "The user wants weather. I will search.".into(), tokens: 1 },
            Event::Tool {
                id: 1,
                verb: "web".into(),
                target: "cass lake mn weather".into(),
                state: "failed".into(),
                summary: String::new(),
                detail: None,
            },
            // The model kept reasoning, and it arrived in `content`.
            Event::Text { delta: "I see the tool output shows just \"tool\" -- possibly ".into() },
            Event::Text { delta: "indicating some kind of error or limitation.".into() },
            // …and then closed the block it had opened before `content` began.
            Event::SpeechRetracted { tokens: 2 },
        ],
    );

    assert_eq!(
        spoken(&v),
        "",
        "reasoning was left in the transcript as Marlowe's speech: {:?}",
        spoken(&v)
    );
    assert!(
        thought(&v).contains("I see the tool output shows just"),
        "the retracted text must land in the thinking block, not vanish: {:?}",
        thought(&v)
    );
    assert!(
        !thought(&v).contains("</think>") && !spoken(&v).contains("</think>"),
        "a closing tag must never be text anywhere"
    );
}

/// **The event order the provider actually produces**, which is not the one the first version of
/// this file tested.
///
/// A turn interleaves: native reasoning arrives in `message.thinking` and opens a block, content
/// then leaks as speech, and the retraction lands **after** a tool line and a second reasoning
/// block have already been appended. The first projection looked only at `transcript.last()`, so
/// it found reasoning where it expected speech, did nothing, and left the leak on screen — with a
/// verbatim copy of itself in a thought block underneath.
///
/// That is what the Cass Lake screenshot showed on the *second* attempt: `thought for 265
/// characters`, a purple block of reasoning, then `thought for 1071 characters` containing the
/// same words. Two copies is the signature of a retraction that fired and was ignored.
#[test]
fn a_retraction_is_honoured_when_it_is_not_the_last_event() {
    let mut v = view();
    apply_events(
        &mut v,
        &[
            Event::Reasoning { delta: "The user wants weather. I will search.".into(), tokens: 1 },
            Event::Tool {
                id: 1,
                verb: "web".into(),
                target: "weather.gov".into(),
                state: "failed".into(),
                summary: String::new(),
                detail: None,
            },
            Event::Text { delta: "including scheme like https://weather.com ".into() },
            Event::Text { delta: "I need to format this properly.".into() },
            Event::Reasoning { delta: " Trying the National Weather Service.".into(), tokens: 1 },
            Event::SpeechRetracted { tokens: 2 },
        ],
    );

    assert_eq!(
        spoken(&v),
        "",
        "the retraction was ignored because it was not the last event: {:?}",
        spoken(&v)
    );
    let thoughts: Vec<&str> = v
        .transcript
        .iter()
        .filter_map(|e| match e {
            Entry::Reasoning { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        thoughts.iter().filter(|t| t.contains("including scheme")).count(),
        1,
        "the retracted text must appear exactly once, not once as speech and once as a second \
         thought block: {thoughts:?}"
    );
}

/// A real answer after the retraction survives it. The correction is scoped to what was
/// outstanding, not to everything the turn ever said.
#[test]
fn speech_after_a_retraction_is_kept() {
    let mut v = view();
    apply_events(
        &mut v,
        &[
            Event::Text { delta: "still thinking about this".into() },
            Event::SpeechRetracted { tokens: 2 },
            Event::Text { delta: "It is 12°C in Cass Lake.".into() },
        ],
    );
    assert_eq!(spoken(&v), "It is 12°C in Cass Lake.");
    assert_eq!(thought(&v), "still thinking about this");
}

/// A retraction with nothing outstanding is ordinary, not an error: every nested
/// `<think>…</think>` in content produces one without any speech having been emitted.
#[test]
fn a_retraction_with_nothing_outstanding_is_harmless() {
    let mut v = view();
    apply_events(&mut v, &[Event::Reasoning { delta: "weighing".into(), tokens: 1 }, Event::SpeechRetracted { tokens: 2 }]);
    assert_eq!(spoken(&v), "");
    assert_eq!(thought(&v), "weighing");
}

/// **The split happens in the provider, so the loop sees prose only when there is prose.**
///
/// This is the second half of the bug: a turn whose entire content was reasoning must not look
/// like an answer. Asserted on the splitter directly, because that is where the loop's input is
/// decided.
#[test]
fn a_turn_that_was_entirely_reasoning_yields_no_prose_for_the_loop() {
    use marlowe_provider::{Segment, ThinkSplitter};

    let mut s = ThinkSplitter::new();
    let mut prose = String::new();
    let mut retracted = false;
    for chunk in [
        "I see the tool output shows just \"tool\" -- possibly indicating",
        " some kind of error or limitation with how `use` works here.",
        "\n\n[your reasoning continues]\n</thi",
        "nk>",
    ] {
        let split = s.feed(chunk);
        if split.retract_speech {
            prose.clear();
            retracted = true;
        }
        for seg in &split.segments {
            if let Segment::Speech(t) = seg {
                prose.push_str(t);
            }
        }
    }
    for seg in &s.finish().segments {
        if let Segment::Speech(t) = seg {
            prose.push_str(t);
        }
    }

    assert!(retracted, "the closing tag must retract the reasoning that preceded it");
    assert_eq!(
        prose, "",
        "the loop would read this as an answer and end the run mid-thought: {prose:?}"
    );
}

/// **The prompt may only name tools that exist.**
///
/// `--dev`'s outbound dump on the Cass Lake run showed the system prompt still saying *"Answer
/// with `done` when the task is complete"* — long after `done` was removed from the vocabulary
/// because a small model cannot be trusted to emit a terminator reliably. So every turn told the
/// model to finish by calling a tool that would be routed to a tool host with no executor for it.
///
/// Nothing could catch this: the loop was right, the provider was right, and the prompt is a
/// string. This is the guard for it — a claim about a name needs a test that the name still
/// exists.
/// Every backticked lowercase word in `prompt` that is not a registered tool.
fn unknown_tools_named_in(prompt: &str) -> Vec<String> {
    let registry = marlowe_tools::builtin_registry().unwrap();
    prompt
        .split('`')
        .skip(1)
        .step_by(2)
        .map(str::trim)
        .filter(|w| !w.is_empty() && w.chars().all(|c| c.is_ascii_lowercase()))
        .filter(|w| registry.get(&marlowe_tools::ToolId::new(*w)).is_none())
        .map(str::to_string)
        .collect()
}

#[test]
fn the_system_prompt_names_no_tool_that_does_not_exist() {
    let prompt = marlowe_daemon::governance_prompt();
    assert!(
        unknown_tools_named_in(prompt).is_empty(),
        "the system prompt names {:?}, which are not registered tools. Prompt was:\n{prompt}",
        unknown_tools_named_in(prompt)
    );
}

/// **The negative control, and it is the whole reason the test above is worth having.**
///
/// The current prompt contains no backticks at all, so the assertion passes over an empty list —
/// it would go green against a prompt naming anything. That is a vacuous test, and shipping one
/// here would have been the same mistake in a smaller box. This pins the checker against the
/// exact string that shipped broken.
#[test]
fn the_guard_catches_the_prompt_that_actually_shipped() {
    let shipped = "Use a tool when the user asks for something a tool can do. Answer with `done` \
                   when the task is complete.";
    assert_eq!(
        unknown_tools_named_in(shipped),
        vec!["done".to_string()],
        "the checker must reject the prompt that told the model to call a removed tool"
    );

    // …and it must not reject a prompt that names a real one.
    assert!(unknown_tools_named_in("Call `read` to read a file.").is_empty());
}

/// Names that WERE tools and are not any more.
///
/// **A retired name is worse than an invented one**, because it reads as correct to everyone who
/// remembers it — including the model, whose memories from earlier sessions can still say "use
/// `find`". `done` was removed when a small model proved unable to emit a terminator reliably;
/// `find` became `grep` at ADR-059.
const RETIRED_TOOLS: [&str; 2] = ["done", "find"];

/// **The guard above is the right instrument pointed at ONE string.**
///
/// `the_system_prompt_names_no_tool_that_does_not_exist` runs against `governance_prompt()` only.
/// The `<workspace>` map named `find` twice, and every tool description is outside its reach —
/// which is exactly where the ADR-059 mis-cue lived: `SHELL_DESCRIPTION` asserted both readings of
/// the word `find` in one paragraph, and nothing could see it.
///
/// **It is a narrower check than the one above and deliberately so.** Pointing
/// `unknown_tools_named_in` at descriptions does not work: they are full of backticked lowercase
/// words that are not tools and never were — `ls`, `head`, `sed`, `awk`, `cwd`, `pattern`, `path`.
/// A guard that flagged those would be turned off within a week. What is checkable, and what
/// actually goes wrong, is a RETIRED name surviving in live prose.
#[test]
fn no_live_prose_names_a_retired_tool() {
    let registry = marlowe_tools::builtin_registry().unwrap();
    let mut surfaces: Vec<(String, String)> =
        vec![("governance_prompt".into(), marlowe_daemon::governance_prompt().to_string())];

    let dir = std::env::temp_dir().join(format!("marlowe-retired-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("README.md"), "hello").unwrap();
    if let Some(map) = marlowe_daemon::workspace_map(&dir) {
        surfaces.push(("workspace_map".into(), map));
    }
    let _ = std::fs::remove_dir_all(&dir);

    for tool in marlowe_tools::BUILTIN_TOOLS {
        let id = marlowe_tools::ToolId::new(tool);
        surfaces.push((
            format!("{tool} description"),
            registry.get(&id).expect("registered").description.text().to_string(),
        ));
        for p in registry.manifest(&id).unwrap().params() {
            surfaces.push((
                format!("{tool}.{}", p.name),
                p.description.clone().unwrap_or_default(),
            ));
        }
    }

    for (where_, text) in &surfaces {
        for retired in RETIRED_TOOLS {
            assert!(
                !text.split('`').skip(1).step_by(2).any(|w| w.trim() == retired),
                "`{retired}` is not a tool any more and `{where_}` still names it as one:
{text}"
            );
        }
    }
    assert!(surfaces.len() > 20, "the sweep must actually have surfaces in it: {}", surfaces.len());
}

/// The negative control for the sweep above: it must reject the prose that shipped, and it must
/// not reject prose that names a live tool.
#[test]
fn the_retired_name_sweep_rejects_the_description_that_shipped() {
    let shipped = "`glob` lists files, `find` searches inside them";
    assert!(RETIRED_TOOLS.iter().any(|r| shipped
        .split('`')
        .skip(1)
        .step_by(2)
        .any(|w| w.trim() == *r)));
    let fixed = "`glob` lists files by name, `grep` searches inside them";
    assert!(!RETIRED_TOOLS.iter().any(|r| fixed
        .split('`')
        .skip(1)
        .step_by(2)
        .any(|w| w.trim() == *r)));
}

/// **The conversation survives the turn.**
///
/// `ask_streaming` built a fresh `SessionState` on every request, so Marlowe was handed turn 1
/// every time and could not summarize a conversation he was in. Asserted on the daemon's own
/// store, driven through two turns on one session name.
#[test]
fn a_second_turn_on_the_same_session_can_see_the_first() {
    let scratch = std::env::temp_dir().join(format!("marlowe-session-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch).unwrap();

    let mut config = marlowe_daemon::DaemonConfig::new(scratch.clone(), scratch.clone());
    config.port = 0;
    let Ok(mut daemon) = marlowe_daemon::Daemon::open(config) else {
        // No profile/scope on this platform; the store is still covered by the unit tests.
        return;
    };

    let turns = daemon.session_turn_count("cli");
    assert_eq!(turns, 0, "a session that has never spoken has no history");

    daemon.remember_user_turn("cli", "my favourite colour is green");
    assert_eq!(
        daemon.session_turn_count("cli"),
        1,
        "the first turn must be stored, or the second turn starts from nothing"
    );

    daemon.remember_user_turn("cli", "what did I just say?");
    assert_eq!(
        daemon.session_turn_count("cli"),
        2,
        "the second turn must ADD to the first, not replace it — this is the amnesia bug"
    );

    let _ = std::fs::remove_dir_all(&scratch);
}

/// **Narration that accompanies a tool call is thinking, not speech — and it carries no tag.**
///
/// The splitter catches reasoning the model fences with `</think>`. It cannot catch reasoning the
/// model simply writes into `content` unfenced, which is what a mid-chain turn produces:
///
/// ```text
///   ⋯ bash   echo hello marlowe   blocked
///   But wait — I keep seeing "[bash blocked]" responses before any of my attempts succeeded...
///   Let me try one more time with cwd="" and command="echo hello world":
///   ⋯ bash   …
/// ```
///
/// Reported live, in the response colour, with more tool calls after it.
///
/// The discriminator needs no tag: **the model called a tool.** Completion is the absence of an
/// action, so a call ending in `tool_calls` is not the reply. Asserted here on the projection,
/// which is where "in the response colour" is actually decided.
#[test]
fn narration_that_precedes_a_tool_call_does_not_stay_in_the_transcript() {
    let mut v = view();
    apply_events(
        &mut v,
        &[
            Event::Reasoning { delta: "The user asked me to run a bash echo.".into(), tokens: 1 },
            Event::Tool {
                id: 1,
                verb: "bash".into(),
                target: "echo hello marlowe".into(),
                state: "failed".into(),
                summary: "blocked".into(),
                detail: None,
            },
            Event::Text { delta: "But wait - I keep seeing \"[bash blocked]\" responses ".into() },
            Event::Text { delta: "before any of my attempts succeeded.".into() },
            // The call ended with another tool call, so none of that was the answer.
            Event::SpeechRetracted { tokens: 2 },
            Event::Tool {
                id: 2,
                verb: "bash".into(),
                target: "echo hello world".into(),
                state: "failed".into(),
                summary: "blocked".into(),
                detail: None,
            },
            // …and the turn finally answers.
            Event::Text { delta: "bash is refused in this build.".into() },
        ],
    );

    assert_eq!(
        spoken(&v),
        "bash is refused in this build.",
        "mid-chain narration must not survive as Marlowe's speech: {:?}",
        spoken(&v)
    );
    assert!(
        thought(&v).contains("But wait"),
        "it belongs in the thinking block, not nowhere: {:?}",
        thought(&v)
    );
}

/// **A blocked tool call must reach the transcript.**
///
/// Reported live: `bash` was refused, nothing appeared on screen, and the user only learned a
/// call had happened by asking the model. The daemon emits the events — captured over the socket:
///
/// ```text
/// tool      verb=bash state=failed summary='blocked'
/// approval  verb=bash scope='echo hello marlowe · .'
/// tool      verb=bash state=failed summary='declined'
/// ```
///
/// So if the screen is empty, the projection is dropping them. This replays that exact sequence.
#[test]
fn a_blocked_tool_call_reaches_the_transcript() {
    use marlowe_view::ToolLineState;

    let mut v = view();
    apply_events(
        &mut v,
        &[
            Event::Degraded {
                what: "read untrusted content".into(),
                remedy: "see `marlowe --status`".into(),
            },
            Event::Reasoning { delta: "The user wants a shell command.".into(), tokens: 1 },
            Event::Tool {
                id: 1,
                verb: "bash".into(),
                target: "echo hello marlowe".into(),
                state: "failed".into(),
                summary: "blocked".into(),
                detail: None,
            },
            Event::Approval {
                decision: 0,
                verb: "bash".into(),
                scope: "echo hello marlowe · .".into(),
                reversible: false,
                // §B9 wants a novelty reason. There is no producer for one yet, and `None`
                // renders as absent rather than as "routine" — a default here would be a claim
                // about promotion logic nobody has written.
                novelty: None,
            },
            Event::Tool {
                id: 2,
                verb: "bash".into(),
                target: "echo hello marlowe · .".into(),
                state: "failed".into(),
                summary: "declined".into(),
                detail: None,
            },
            Event::Text { delta: "bash is refused here.".into() },
            Event::Done {
                outcome: "completed".into(),
                detail: String::new(),
                spend_micros_usd: 0,
                elapsed_ms: 10310,
            },
        ],
    );

    let calls: Vec<&marlowe_view::ToolCall> = v
        .transcript
        .iter()
        .filter_map(|e| match e {
            Entry::Tools(c) => Some(c),
            _ => None,
        })
        .flatten()
        .collect();

    assert_eq!(
        calls.len(),
        2,
        "both refused calls must be in the transcript; found {}: {:?}",
        calls.len(),
        v.transcript
    );
    assert!(
        calls.iter().all(|c| matches!(c.state, ToolLineState::Failed(_))),
        "a refused call must read as a failure"
    );
    assert!(calls.iter().all(|c| c.verb == "bash"));

    // …and the run must not still be claiming an answer is owed once it has finished.
    assert_ne!(
        v.status.state,
        marlowe_view::StatusState::Waiting,
        "the band still says `approval needed` after Done: {:?}",
        v.status.detail
    );
}

/// **A daemon can be stopped, and refuses while a run is live.**
///
/// There was no shutdown path at all. Closing the TUI left a process listening, and the next
/// launch reconnected to it — three times in one session a fixed build was tested against a stale
/// one that way. Invariant 6 says a RUN survives the client that started it, not that the daemon
/// is immortal.
#[test]
fn shutdown_is_reachable_and_the_wire_carries_it() {
    use marlowe_daemon::Request;

    // The request exists on the wire in the same shape as every other op.
    let json = serde_json::to_string(&Request::Shutdown).expect("serializes");
    assert_eq!(json, r#"{"op":"shutdown"}"#, "the wire form is stable: {json}");

    let back: Request = serde_json::from_str(&json).expect("round-trips");
    assert_eq!(back, Request::Shutdown);
}

/// Asking a daemon that is not there is not an error the user needs to see.
#[test]
fn shutting_down_an_absent_daemon_fails_quietly() {
    let client = marlowe_daemon::Client::new("t").with_port(1);
    assert!(
        client.shutdown().is_err(),
        "no daemon on that port, so this must fail rather than appear to succeed"
    );
}
