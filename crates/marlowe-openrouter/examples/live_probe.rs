//! **The one command that closes ADR-046 §9.**
//!
//! Six things in ADR-046 are behaviours of openrouter.ai and cannot be settled by any test in this
//! repository: whether the top-level `provider` field arrives and on which chunk, whether
//! `usage.cost` is present and in what unit, how the keepalive is spelled, whether the upstream
//! pin holds, and whether the endpoint accepts the exact request shape sent. This makes one real
//! call and **prints what actually arrived, including which fields were absent**.
//!
//! ```text
//! export OPENROUTER_API_KEY=sk-or-v1-...
//! cargo run -p marlowe-openrouter --example live_probe -- anthropic/claude-sonnet-4.5
//! ```
//!
//! # Why an example rather than an `#[ignore]`d test
//!
//! An ignored test that nobody runs is indistinguishable from one that does not exist, and a test
//! that *is* run costs money on every `cargo test --workspace`. More importantly this is not a
//! pass/fail question — it is a **reading**. The interesting output is the field-by-field presence
//! table, which a green tick would throw away.
//!
//! It exits non-zero when the call fails, so CI *could* run it where a key exists.

use marlowe_contract::TrustClass;
use marlowe_loop::{
    Assembler, Block, CallLimits, ModelDriver, ModelStep, SessionId, SessionState, SourceKind,
};
use marlowe_openrouter::{ApiKey, OpenRouterDriver, TlsTransport};
use marlowe_tools::{builtin_registry, ExposedSet, ToolId};

fn main() {
    let model = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!(
            "usage: cargo run -p marlowe-openrouter --example live_probe -- <MODEL_SLUG> [UPSTREAM]\n\
             \n\
             There is no default slug: nothing on OpenRouter has been measured by this project.\n\
             Example: anthropic/claude-sonnet-4.5   ·   https://openrouter.ai/models"
        );
        std::process::exit(2);
    });
    let pin = std::env::args().nth(2);

    let key = match ApiKey::from_environment() {
        Ok(k) => k,
        Err(e) => {
            eprintln!("SKIP: {e}");
            // **A skip, not a failure.** A probe that needs a credential and does not have one has
            // measured nothing; reporting that as a failure would make a missing key look like a
            // broken adapter, which is the confusion `capability_for`'s disclosure exists to
            // prevent in the other direction.
            std::process::exit(0);
        }
    };
    println!("key: {key:?}");
    println!("model requested: {model}");
    println!("upstream pin: {}", pin.clone().unwrap_or_else(|| "none (fallbacks allowed)".into()));
    println!();

    let mut state = SessionState::new(SessionId::new(), "Marlowe.");
    state.push(Block::new(
        SourceKind::History,
        // Deliberately trivial and deliberately NOT a tool-call prompt: this probe is about the
        // wire, and a model that decides to call `read` would produce a different frame shape and
        // a less legible reading. Tool-call behaviour is measured separately, per model, and is
        // reported as NOT MEASURED until it is.
        "Reply with exactly the word OK and nothing else.",
        TrustClass::UserAsserted,
    ));
    let view = Assembler::new(8_192, 1_024).assemble(&state);
    let tools = ExposedSet::new(vec![ToolId::new("read")]).expect("one tool fits");

    let mut driver = OpenRouterDriver::new(
        Box::new(TlsTransport::new()),
        key,
        &model,
        builtin_registry().expect("the builtins are compiled in"),
    );
    if let Some(p) = pin {
        driver = driver.with_pinned_upstream(vec![p]);
    }

    // Every raw chunk, so the presence table below is derived from what arrived rather than from
    // what the parser chose to keep.
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<serde_json::Value>::new()));
    let sink = std::sync::Arc::clone(&seen);
    let mut driver = driver.with_raw_frames(Box::new(move |f| {
        sink.lock().expect("frames").push(f.clone());
    }));

    let mut streamed = 0usize;
    let result = driver.call_streaming(
        &view,
        &tools,
        CallLimits { max_output_tokens: 64, route: marlowe_loop::ModelRoute::Orchestrator },
        &mut |d| {
            streamed += 1;
            print!("{d}");
            use std::io::Write;
            let _ = std::io::stdout().flush();
        },
    );
    println!();
    println!();

    let frames = seen.lock().expect("frames").clone();
    println!("── WHAT ARRIVED ─────────────────────────────────────────────");
    println!("chunks: {}", frames.len());
    println!("delta callbacks: {streamed}   (1 would mean it did NOT stream)");
    for (field, path) in [
        ("provider (top level)", "/provider"),
        ("model (top level)", "/model"),
        ("id (top level)", "/id"),
        ("usage.prompt_tokens", "/usage/prompt_tokens"),
        ("usage.completion_tokens", "/usage/completion_tokens"),
        ("usage.cost", "/usage/cost"),
    ] {
        let first = frames.iter().position(|f| f.pointer(path).is_some());
        match first {
            Some(i) => println!(
                "  {field:<26} PRESENT, first on chunk {i}: {}",
                frames[i].pointer(path).map(|v| v.to_string()).unwrap_or_default()
            ),
            // **ABSENT is the reading, not a failure.** ADR-046 §3's rule applied to the probe
            // itself: a field that did not arrive must not be rendered the same as one that did.
            None => println!("  {field:<26} ABSENT from every chunk"),
        }
    }
    println!();

    match result {
        Ok(call) => {
            println!("── STEP ─────────────────────────────────────────────────────");
            match &call.step {
                ModelStep::Say(t) => println!("  Say({t:?})"),
                other => println!("  {other:?}"),
            }
            println!(
                "  usage: {}+{} tok · {} µUSD · {} ms",
                call.usage.prompt_tokens,
                call.usage.completion_tokens,
                call.usage.micros_usd,
                call.usage.wall_ms
            );
            println!();
            println!("── ATTRIBUTION (ADR-046 §3) ─────────────────────────────────");
            for c in &driver.attribution().calls {
                println!("  {}", c.disclosure());
            }
            println!("  run: {}", driver.attribution().disclosure());
            println!();
            println!(
                "  reproducible-enough-to-publish: {}",
                driver.attribution().calls.iter().all(|c| c.upstream_is_recorded())
            );
            println!();
            println!("  DISCLOSURE: {}", driver.capability().disclosure());
        }
        Err(e) => {
            println!("── FAILED ───────────────────────────────────────────────────");
            println!("  {}", e.detail);
            println!("  retriable: {}", e.retriable);
            // The one assertion this probe makes about itself: whatever went wrong, the
            // credential is not in the message.
            assert!(
                !e.detail.contains(
                    std::env::var(marlowe_openrouter::KEY_VAR).unwrap_or_default().trim()
                ) || std::env::var(marlowe_openrouter::KEY_VAR).unwrap_or_default().trim().is_empty(),
                "THE API KEY REACHED AN ERROR STRING"
            );
            std::process::exit(1);
        }
    }
}
