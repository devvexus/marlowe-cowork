//! **What a thought cost, from Ollama, whose stream cannot say it directly.**
//!
//! # The measurement this file exists for
//!
//! `daemon.rs` asserted *"one delta is one token — Ollama sends one frame per token"*, and
//! `STATE.md` recorded that nothing measured it. Measured on 2026-08-31 against `llama-server`'s
//! `/tokenize` on the same GGUF blob Ollama was serving:
//!
//! | prompt | thinking frames | exact tokens | frames are |
//! |---|---|---|---|
//! | `Say hi.` | 116 | 120 | −3.3% |
//! | `What is 2+2?` | 179 | 200 | −10.5% |
//! | `Name one primary colour.` | 236 | 250 | −5.6% |
//!
//! Identical across three repetitions of one prompt, so it is not a slow client: Ollama's own
//! thinking parser buffers a whitespace-leading token and flushes it joined to the next one. The
//! **content** channel is exactly one token per frame — 60 frames against an `eval_count` of 61,
//! the extra being the stop token — which is what makes the arithmetic below possible at all.
//!
//! So a frame count is a lower bound and the engine's `eval_count` is the truth. The rule, from
//! [`marlowe_loop::ModelDriver::call_streaming_split`] and applied identically by all three
//! adapters: **a token the engine counted and never streamed belongs to the channel that was
//! open.** No template constant is subtracted anywhere — `</think>` costs a different number of
//! tokens in a different vocabulary, and a constant measured on one model is not a fact about
//! another.
//!
//! # Why a real socket
//!
//! This driver has no transport seam, and inventing one to test it would test the seam. A listener
//! on a loopback port drives the real `request_body`, the real HTTP client, the real NDJSON
//! decoder, the real fold and the real `parse_step` — everything except the model.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;

use marlowe_contract::TrustClass;
use marlowe_loop::{
    Assembler, Block, CallLimits, ContextView, ModelDriver, SessionId, SessionState, SourceKind,
};
use marlowe_provider::ollama::OllamaDriver;
use marlowe_provider::{LocalEndpoint, Routing};
use marlowe_tools::{builtin_registry, ExposedSet, ToolId};

fn view() -> ContextView {
    let mut state = SessionState::new(SessionId::new(), "Marlowe.");
    state.push(Block::new(SourceKind::History, "what is 2+2", TrustClass::UserAsserted));
    Assembler::new(8_192, 1_024).assemble(&state)
}

fn tools() -> ExposedSet {
    ExposedSet::new(vec![ToolId::new("read")]).expect("one tool fits")
}

fn limits() -> CallLimits {
    CallLimits { max_output_tokens: 512, route: marlowe_loop::ModelRoute::Orchestrator }
}

/// Serve one canned NDJSON response on a loopback port and hand back the endpoint.
///
/// The thread ends with the connection, so nothing outlives the test.
fn serving(ndjson: &'static str) -> LocalEndpoint {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
    let port = listener.local_addr().expect("bound").port();
    std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("the driver connects");
        // Read past the request head and its body, so the client's write never blocks.
        let mut reader = BufReader::new(stream.try_clone().expect("clone"));
        let mut length = 0usize;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                break;
            }
            if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                length = v.trim().parse().unwrap_or(0);
            }
            if line.trim().is_empty() {
                break;
            }
        }
        let mut body = vec![0u8; length];
        reader.read_exact(&mut body).ok();

        let mut stream = stream;
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nConnection: close\r\n\r\n",
            )
            .expect("head");
        stream.write_all(ndjson.as_bytes()).expect("body");
        stream.flush().ok();
    });
    LocalEndpoint::new("127.0.0.1", port).expect("loopback is a local endpoint")
}

fn driver(ndjson: &'static str) -> OllamaDriver {
    OllamaDriver::new(
        serving(ndjson),
        Routing::uniform("marlowe-red:9b").expect("a uniform route"),
        builtin_registry().expect("builtins"),
    )
}

/// Four thinking frames, one answer frame, and the engine says it generated eight tokens.
///
/// The three it never streamed are the ones its parser merged plus the closing delimiter and the
/// stop token. Nothing else was produced, so they are thinking: the count is **7**, which is what
/// the engine said minus the answer, not **4**, which is what the frames said.
const MERGED_THINKING: &str = concat!(
    r#"{"message":{"role":"assistant","thinking":"Two"},"done":false}"#,
    "\n",
    r#"{"message":{"role":"assistant","thinking":" plus"},"done":false}"#,
    "\n",
    r#"{"message":{"role":"assistant","thinking":" two"},"done":false}"#,
    "\n",
    r#"{"message":{"role":"assistant","thinking":" is"},"done":false}"#,
    "\n",
    r#"{"message":{"role":"assistant","content":"4"},"done":false}"#,
    "\n",
    r#"{"message":{"role":"assistant","content":""},"done":true,"done_reason":"stop","#,
    r#""prompt_eval_count":13,"eval_count":8,"total_duration":1000000}"#,
    "\n",
);

#[test]
fn the_thinking_count_is_the_engines_eval_count_minus_the_answer_not_a_count_of_frames() {
    let mut reasoning = String::new();
    let mut reasoning_tokens = 0u64;
    let mut speech = String::new();
    let call = driver(MERGED_THINKING)
        .call_streaming_split(
            &view(),
            &tools(),
            limits(),
            &mut |d| speech.push_str(d),
            &mut |r, t| {
                reasoning.push_str(r);
                reasoning_tokens += t;
            },
            &mut |_| {},
        )
        .expect("the canned stream decodes");

    // The premise: four thinking frames and one content frame reached the callbacks.
    assert_eq!(reasoning, "Two plus two is");
    assert_eq!(speech, "4");
    assert_eq!(call.usage.completion_tokens, 8, "the engine's own figure is on the wire");

    // The claim. `8 - 1` — everything generated that was not the answer. **Not 4**, which is the
    // frame count and what a "one delta is one token" reading gives, and not 15, which is
    // `text.len()` and the number this line reported before it reported tokens.
    assert_eq!(
        reasoning_tokens, 7,
        "the settlement must charge what the engine counted, not what it streamed"
    );
}

/// **The control: a call that never reasoned must not invent a thought.**
///
/// The engine still reports a token it never streamed — the stop token — and the settlement is
/// deliberately silent when there was no reasoning, because a thought of length zero is not a
/// thought. Without this guard a head line would appear under every answer reading `thought
/// 1 token`.
#[test]
fn a_call_that_never_reasoned_settles_nothing() {
    const NO_THINKING: &str = concat!(
        r#"{"message":{"role":"assistant","content":"4"},"done":false}"#,
        "\n",
        r#"{"message":{"role":"assistant","content":""},"done":true,"done_reason":"stop","#,
        r#""prompt_eval_count":13,"eval_count":2,"total_duration":1000000}"#,
        "\n",
    );
    let mut reasoning_calls = 0u32;
    let mut reasoning_tokens = 0u64;
    driver(NO_THINKING)
        .call_streaming_split(
            &view(),
            &tools(),
            limits(),
            &mut |_| {},
            &mut |_, t| {
                reasoning_calls += 1;
                reasoning_tokens += t;
            },
            &mut |_| {},
        )
        .expect("decodes");

    assert_eq!(reasoning_calls, 0, "no reasoning arrived, so none may be reported");
    assert_eq!(reasoning_tokens, 0);
}

/// **A frame is charged once even when it carries two spellings of the same channel.**
///
/// The adapter reads `thinking`, `reasoning` and `reasoning_content` because Ollama has used more
/// than one. A frame carrying two of them is still one token; charging per field would inflate the
/// count on exactly the models that spell it twice.
#[test]
fn one_frame_carrying_two_spellings_of_reasoning_is_charged_once() {
    const TWO_SPELLINGS: &str = concat!(
        r#"{"message":{"role":"assistant","thinking":"a","reasoning":"b"},"done":false}"#,
        "\n",
        r#"{"message":{"role":"assistant","content":""},"done":true,"done_reason":"length","#,
        r#""prompt_eval_count":13,"eval_count":1,"total_duration":1000000}"#,
        "\n",
    );
    let mut reasoning_tokens = 0u64;
    let mut reasoning = String::new();
    driver(TWO_SPELLINGS)
        .call_streaming_split(
            &view(),
            &tools(),
            limits(),
            &mut |_| {},
            &mut |r, t| {
                reasoning.push_str(r);
                reasoning_tokens += t;
            },
            &mut |_| {},
        )
        .expect("decodes");

    assert_eq!(reasoning, "ab", "both spellings reach the channel");
    assert_eq!(reasoning_tokens, 1, "one frame, one token, however many fields carried it");
}
