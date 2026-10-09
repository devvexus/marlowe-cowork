//! The Marlowe binary.
//!
//! ADR-002: one binary, two roles — `marlowe` (thin client) and `marlowe --serve` (daemon).
//! The daemon does not exist yet. M1 adds the client's two surfaces, driven by a scripted stub:
//! `--tui` (Addendum B v2) and `--classic` (§B11). M0b's eval adapter is unchanged.

mod adapter;
mod agent;
mod dump;
mod elapsed;
mod launcher;
mod profile;
mod tui;
/// A real terminal window per run. `M3-DESIGN.md` §6.
mod keyburst;
mod watch;

use std::io::{self, BufReader};
use std::path::PathBuf;

const USAGE: &str = "\
marlowe --ask <question> [--workspace <DIR>] [--daemon-port <N>] [--dev] [--context <TOKENS>]
marlowe --serve [--workspace <DIR>] [--daemon-port <N>] [--dev] [--context <TOKENS>]
        [--no-thinking]
marlowe --status
marlowe --shutdown [--daemon-port <N>]
marlowe --launch
marlowe --tui [--scripted] [--daemon-port <N>] [--timing-probe] [--color-depth <truecolor|256|16>]
        [--ground] [--provider <ollama|ollama/llama.cpp|openrouter>] [--openrouter-model <SLUG>]
        [--llamacpp-port <N>] [--llamacpp-sampling <ollama|server>]
marlowe --classic
marlowe --doctor
marlowe --eval-adapter --profile-root <DIR> --embedder-model <DIR> --reranking <off|DIR>
        [--embedding-cache <DIR>] [--embedder-workers <N>]
        [--embedder-provider <cpu|cuda|auto>]
        [--dump-gate-features <FILE>] [--fit-mode]
        [--dump-consolidation <FILE>] [--consolidation-dry-run]
        [--profile-retrieval <FILE>]
        [--rerank-threads <N>] [--rerank-batch <on|off>]
        [--rerank-provider <cpu|cuda|auto>] [--tier1-model <NAME>]
        [--tier1-runtime <ollama|llama-server|off>]

  --ask <question>              Ask one question and print the answer. The thin client of
                                ARCHITECTURE §6: it holds no run state, so the run belongs to the
                                daemon and outlives this process. Auto-spawns a daemon if none is
                                listening, and says so.

  --serve                       Run the daemon. It owns the journal, the engine and the runs.
                                Runs survive the client that started them (invariant 6); they do
                                NOT yet survive the daemon itself — that is M3 and K5.

  --status                      What the daemon is, what model it routes to with that model's
                                MEASURED tool-call reliability, which rerank provider is active
                                (announced, never inferred — ADR-029), and whether anything is
                                degraded with the command that fixes it.

  --workspace <DIR>             The directory tools are scoped to. Defaults to the current
                                directory. Every path the model names is resolved relative to it
                                and refused if it escapes.

  --tui                         The terminal interface. Requires at least 120x30; below that it
                                prints one line naming the current and required size and offers
                                --classic. It does NOT render a degraded grid -- a narrow variant
                                was designed and rejected because it cost the borders, which cost
                                the region contract, which is the entire design (Addendum B §B11).

  --diagnostic                  Render raw-mode state, the colour tier and a LIVE KEYSTROKE
                                COUNTER in the titlebar. A running TUI's most important properties
                                are invisible from outside the process, and a separate probe run
                                measures a different process. This makes the session answer for
                                itself: a terminal that renders and animates while ignoring every
                                key looks exactly like one that works.

  --timing-probe                Draw one frame, report time to first frame and time to
                                interactive in milliseconds, and leave. K4 is stated in these
                                numbers, so they are a command that prints them rather than a
                                target nobody measures.

  --color-depth <TIER>          Override the colour-depth probe. §B2 requires the 256- and
                                16-colour fallbacks, so a probe cannot be avoided -- but a silent
                                probe is the mismatch-hiding default this project has shipped four
                                bugs behind. The probe's answer is printed by --timing-probe and
                                --doctor, and this is how it is overridden.

  --classic                     The classic CLI: a readline REPL with COMMAND parity, not layout
                                parity. The narrow, SSH, piped-stdin and no-TTY path. Both
                                surfaces dispatch from one command registry, so parity is a
                                property of the design rather than a checklist that rots.

  --doctor                      Terminal capability report: size, colour depth and why it was
                                chosen, accent contrast against a dark and a light background, and
                                the braille glyph row for you to confirm BY EYE. Font coverage for
                                U+2800-U+28FF cannot be detected, and ADR-021 refuses to add a
                                silent fallback -- so the check is given to the only instrument
                                that can read it.

  Speak CONTRACTS.md section 4 over NDJSON on stdin/stdout.

  --profile-root <DIR>          A fresh, empty directory for this run's journal. Required, and
                                required to be empty: the harness spawns one process per corpus
                                and four more for the clock probe, and each must start from
                                empty state.

  --llamacpp-port <N>           Where `--provider llamacpp` expects a llama-server. Default
                                11437. NOT 11434 (Ollama's), NOT 11435 (Marlowe's own daemon)
                                and NOT 11436 (a second daemon, by convention). A port equal
                                to Ollama's or to this invocation's --daemon-port is REFUSED
                                at load: the two defaults collided once, and no unit test
                                could see it because no single process knew both constants.

  --llamacpp-sampling <S>       Where `--provider llamacpp` takes its sampler from. An EXPLICIT
                                value, deliberately not a bare boolean, for the reason --reranking
                                is one. `ollama` (DEFAULT): the model's own `.params` layer out of
                                Ollama's store, which is what Ollama would have applied --
                                qwen3.5:9b publishes temperature 1, top_k 20, top_p 0.95,
                                presence_penalty 1.5. `server`: llama-server's own defaults
                                (temperature 0.8, top_k 40, presence_penalty 0), a RECORDED choice
                                for a GGUF Ollama never pulled. A forgotten switch would otherwise
                                run the same weights at a different temperature under the same
                                label, with nothing reporting the change.

  --provider <P>                `ollama` (DEFAULT: Ollama stores and runs), `ollama/llama.cpp`
                                (ADR-060's hybrid: Ollama stores and lists, a llama-server Marlowe
                                starts and owns serves -- and falls back to Ollama, saying why, if
                                it cannot; `llamacpp` is the same choice without the slash), or
                                `openrouter`.
                                ADR-046 and ADR-060.
                                **`ollama` is the zero-config path and nothing moves it but this
                                flag** -- not an environment variable, not a config file. K6
                                measures install-to-answer with no configuration at all, and it
                                is a kill criterion.
                                `openrouter` is for BENCHMARK runs, where a stronger or faster
                                model is needed. It requires --openrouter-model and the
                                OPENROUTER_API_KEY environment variable, and refuses by name at
                                startup without either -- it does NOT fall back to the local
                                model, because a benchmark that silently measured a 9B under a
                                frontier model's label is worse than one that would not start.
                                It is not bit-identically reproducible; the serving upstream is
                                recorded per call instead. See ADR-046.
  --openrouter-model <SLUG>     The OpenRouter model, e.g. `anthropic/claude-sonnet-4.5`.
                                REQUIRED with `--provider openrouter` and it has NO DEFAULT: no
                                slug has been measured by this project, and a built-in one would
                                read as a recommendation. https://openrouter.ai/models
  --openrouter-upstream <NAME>  Pin the serving upstream, e.g. `Anthropic`. Sends
                                `provider.order` with `allow_fallbacks: false`. OpenRouter
                                otherwise routes one model name to several upstreams at different
                                quantizations and may change that between two requests -- so two
                                benchmark runs can differ while every label reads identical.
                                Optional; the upstream is RECORDED either way.
  --embedder-model <DIR>        ADR-004's embedding model. Required, no default. The files are
                                verified against digests pinned in the binary, so a swapped or
                                partial model is a refusal rather than a quietly different
                                number. Fetch with `python tools/fetch_model.py`.

  --reranking <off|DIR>         Session H's in-session cross-encoder rerank stage. REQUIRED, no
                                default, and it takes an EXPLICIT value -- either the literal
                                `off` or the directory holding the pinned graph and tokenizer.
                                The pinned graph is the SESSION J FINE-TUNE, f32:
                                models/ms-marco-MiniLM-L-2-v2-ft-session-j (ADR-018, held-out
                                R@1 0.6725 at 214 ms/query). The pre-Session-K int8 directory is
                                refused BY NAME rather than by a missing-file error.
                                Deliberately NOT a bare `--rerank` boolean. That is the mistake
                                the consolidation flag documents below: forget a default-off
                                switch in the harness target string and the run measures the
                                un-reranked system under a reranked label, producing every number
                                with nothing observing the mismatch. Spelling `off` is a choice
                                somebody made and the report records it; omitting the flag is a
                                refusal to start.

  --embedding-cache <DIR>       Content-addressed embedding cache, keyed on the model and
                                vocabulary digests, the embedder version and the sequence
                                length. Optional. NOT inside --profile-root, which is required
                                to be empty per spawn and would make the cache cold every time.

  --embedder-workers <N>        Forward passes to run concurrently. A throughput knob and
                                provably NOT a quality knob: each text is embedded entirely
                                within one worker, asserted bit-for-bit at 1/3/8 workers.
                                Default: available parallelism, capped at 8.

  --embedder-provider <P>       `cpu`, `cuda` or `auto`. DEFAULT `auto` -- ADR-044. `auto` opens
                                CUDA sessions where one constructs and device memory holds it
                                with a spare, narrows the width where it does not, and falls to
                                CPU at the full requested width where CUDA is unavailable. It
                                NEVER fails a run over a busy card. `cuda` is the REFUSAL arm: it
                                errors rather than falling back, which is what makes a CUDA label
                                on a published number mean something, and it is the only value
                                safe to measure under -- `auto` resolves against free VRAM at that
                                instant, so two spawns on one machine can pick two scorers. The
                                RESOLVED provider is printed at startup beside the request, and
                                it is in the embedding cache identity, so vectors cannot cross.

  --dump-gate-features <FILE>   Write one NDJSON row per SCORED CANDIDATE to FILE. A diagnostic
                                side channel: it never changes what goes on the wire. With a
                                gate loaded each row also carries this build's own `score`,
                                `calibrated_precision` and `passes` -- which is what a scoring
                                run reads when the gate abstains and the response therefore
                                carries no injected memories at all.

  --profile-retrieval <FILE>    Write one NDJSON row per §4.2 call with the per-stage breakdown:
                                query embedding, the full-store candidate scan, session scoping,
                                lexical, dense, features, gate, pruning, rerank and assembly, in
                                MICROSECONDS, plus the span they were taken inside and the
                                RESIDUAL between the two. Diagnostic only: the §4.2 response is
                                byte-identical with or without it, and the write happens AFTER
                                `cost.latency_ms` is read, so profiling cannot inflate the number
                                it exists to explain. Read it with `tools/profile_retrieval.py`.

  --rerank-threads <N>          ONNX intra-op threads for the CROSS-ENCODER only. Default 1, the
                                value every published number was measured at, per ADR-003's 1-vCPU
                                target. MEASUREMENT KNOB: raising it may change reduction order
                                inside a matmul, which changes logits, which changes the ranking.
                                A value is adopted only if scored-candidates.ndjson stays
                                byte-identical -- determinism gates this regardless of speed. The
                                value is recorded on every --profile-retrieval row, so a sweep
                                cell cannot claim one thread count and run another.

  --rerank-provider <P>         `cpu`, `cuda` or `auto`. DEFAULT `auto` -- ADR-045. `auto` opens a
                                CUDA session where one constructs and the card has room for it
                                plus a spare, and falls to CPU otherwise. It NEVER fails a run
                                over a busy card. `cuda` is the REFUSAL arm: it errors rather than
                                falling back, which is the only value safe to publish a number
                                under -- `auto` resolves against free VRAM at that instant. The
                                RESOLVED provider is printed at startup beside the request and is
                                stamped on every --profile-retrieval row.

  --rerank-plan <P>             `auto`, `shipped` or `cascade`. DEFAULT `auto`: the cascade runs
                                where it is measured to fit -- when the cross-encoder RESOLVED to
                                CUDA -- and the shipped depth-10 shape runs everywhere else. The
                                cascade draws 30 candidates, narrows them to 10 with the shipped
                                graph and fuses a second digest-pinned opinion over the narrowed
                                set: held-out R@1 0.6987 / R@3 0.8865 against shipped 0.6725 /
                                0.8515 (`runs/session-m0c-m/PREREGISTRATION-CASCADE-HELDOUT.json`).
                                It is GPU-only BY MEASUREMENT (CPU p50 1134 ms against section 5.7's
                                300 ms), which is why `auto` keys on the resolved provider rather
                                than on availability. `shipped` forces the depth-10 path; `cascade`
                                forces the fusion and refuses to start without both graphs. The
                                plan is printed at startup beside the provider and stamped on every
                                --profile-retrieval row.

  --tier1-model <NAME>          The language model that has FIRST CLAIM on device memory
                                (ADR-045 §4). Tier 1 has no CPU fallback and the embedder and
                                reranker do, so they yield: `auto` subtracts this model's size
                                from free VRAM before deciding whether to open a CUDA session.
                                Defaults to this build's routed model. Read from `ollama list`,
                                so a model that is not installed reserves nothing and says so, and
                                one that is already RESIDENT reserves nothing because free memory
                                is already net of it. `marlowe --serve` ignores this and uses its
                                own --model, which is switchable at runtime.

  --tier1-runtime <R>           WHICH PROCESS holds tier 1 -- ADR-060 §3, and it decides whether
                                the two `ollama` reads above are the right questions at all.
                                `ollama` (DEFAULT): ask `ollama ps` and `ollama list`, as before.
                                `llama-server`: a llama-server that is ALREADY LOADED holds the
                                weights, so free device memory is already net of them and the
                                reserve is 0. Passing this when the server is NOT up under-reserves
                                -- which surfaces as a llama-server that will not allocate, loud
                                and actionable, where OVER-reserving surfaces as the embedder
                                silently on CPU with a coherent, false reason.
                                `off`: tier 1 is not on this card at all (a hosted provider), so
                                there is nothing to yield to.

  --rerank-batch <on|off>       Score the whole depth-10 slate in ONE forward pass. Explicit value,
                                no bare boolean. DEFAULT IS DERIVED FROM THE RESOLVED PROVIDER, not
                                from a constant: `off` on CPU, `on` on CUDA. The two measured
                                OPPOSITE (ADR-029) -- CPU sequential 185.8 ms against batched
                                195.6, CUDA batched 3.4 against sequential 15.2 -- so a single
                                global default would be wrong for one of them whichever value it
                                took. Batch invariance is 0.000000000 across sizes 1..10 on CPU
                                and is measured separately on CUDA (ADR-045); it is a COST switch,
                                not a quality one. Recorded per profile row.

  --dump-consolidation <FILE>   Write one NDJSON row per INGESTED SESSION describing what §5.3
                                consolidation merged. A diagnostic side channel: the §4.6
                                response is byte-identical with or without it.

  --consolidation-dry-run       Cluster at every threshold in SWEEP_THRESHOLDS and APPLY NOTHING.
                                This is the pass the frozen merge threshold is chosen from, so it
                                must not itself assume one. Loads no consolidation artifact, and
                                requires --dump-consolidation: a dry run with nowhere to write
                                its sweep changes nothing and produces nothing.

                                WITHOUT this flag consolidation is ON and reads its threshold
                                from the frozen artifact. There is deliberately no flag that
                                turns it off -- a default-off switch is the permissive default
                                that lets a run measure the unconsolidated system under a
                                consolidated label.

  --fit-mode                    Load NO gate. Used only by `tools/fit_gate.py`, to produce the
                                features the gate is fit from before any gate exists. The gate
                                stamp reads `uncalibrated-fit-only`, never `frozen-v2`: this
                                mode calibrates nothing and must not be mistakable for a run
                                that did. Requires --dump-gate-features.
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Modes are mutually exclusive and named. There is deliberately no default mode: a bare
    // `marlowe` becomes the thin client at M2, and guessing one now would mean changing what an
    // existing command does later.
    let modes: Vec<&str> = ["--tui", "--classic", "--doctor", "--eval-adapter", "--launch",
                            "--serve", "--ask", "--status", "--shutdown", "--models",
                            // M3 Session A. The **from-outside** control path §10.1 requires:
                            // another terminal, no TUI, a script. `/runs`, `/steer` and `/watch`
                            // in the TUI reach the same daemon state through the same requests.
                            "--runs", "--steer", "--watch", "--resume", "--cancel"]
        .into_iter()
        .filter(|m| args.iter().any(|a| a == m))
        .collect();
    match modes.len() {
        1 => {}
        0 => {
            eprintln!("{USAGE}");
            eprintln!(
                "error: no mode selected. One of --serve, --ask, --status, --shutdown, --launch, --tui, --watch, --runs, --steer, --classic, --doctor, --eval-adapter."
            );
            std::process::exit(2);
        }
        _ => {
            eprintln!("{USAGE}");
            eprintln!(
                "error: {} were all given. They are different surfaces over the same session, not \
                 layers; pick one.",
                modes.join(" and ")
            );
            std::process::exit(2);
        }
    }

    // ── ARCHITECTURE §6: the two roles ────────────────────────────────────────────────────
    // **`--context` refuses rather than falling back.** A value that cannot be resolved must be a
    // load-time error: Ollama silently applies 2048 to a 262,144-token model when `num_ctx` is
    // omitted, and that permissive default was live for the whole of M2. A typo must not reach it.
    let context: Option<u32> = match flag_value(&args, "--context") {
        None if args.iter().any(|a| a == "--context") => {
            eprintln!("error: --context requires a value in tokens, e.g. `--context 32768`.");
            std::process::exit(2);
        }
        None => None,
        Some(v) => match v.parse::<u32>() {
            Ok(n) if (1_024..=marlowe_provider::MODEL_CONTEXT_CEILING).contains(&n) => Some(n),
            Ok(n) => {
                eprintln!(
                    "error: --context {n} is outside 1024..={}. The upper bound is what the                      pinned model reports it supports; a larger window is a KV-cache commitment                      the model cannot honour.",
                    marlowe_provider::MODEL_CONTEXT_CEILING
                );
                std::process::exit(2);
            }
            Err(_) => {
                eprintln!("error: --context {v:?} is not a number of tokens.");
                std::process::exit(2);
            }
        },
    };

    // **`--no-thinking` turns it off; it is ON unless asked otherwise.** A reasoning model that
    // does not separate its chain of thought inlines it into the answer, so the default is the
    // one that keeps reasoning out of the transcript.
    let thinking = !args.iter().any(|a| a == "--no-thinking");

    // ── ADR-046: which provider, decided ONCE, refused at load ────────────────────────
    //
    // **Every failure here is a load-time exit, never a fallback.** CLAUDE.md's standing rule is
    // to prefer a load-time error to a sensible default, and the "sensible default" available
    // here — quietly using Ollama when the key or the model is missing — is the worst one this
    // project could ship: a benchmark launched at a frontier model would silently measure a local
    // 9B, with every label in the output reading the name that was asked for.
    let model_provider = resolve_provider(&args);

    // What this machine's Ollama actually holds, so `--model` is a choice from a list rather than
    // a guess. **Cloud tags are shown and marked refused** rather than hidden: a user who has one
    // pulled will otherwise try it and get a refusal with no way to have known in advance.
    if modes[0] == "--models" {
        match agent::models() {
            Ok(()) => return,
            Err(e) => {
                eprintln!("marlowe: {e}");
                std::process::exit(1);
            }
        }
    }

    if matches!(
        modes[0],
        "--serve"
            | "--ask"
            | "--status"
            | "--shutdown"
            | "--runs"
            | "--steer"
            | "--watch"
            | "--resume"
            | "--cancel"
    ) {
        let workspace = flag_value(&args, "--workspace")
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        // Cloned before `workspace` is moved into the config below; the reranker search needs the
        // same directory and a second `flag_value` call would be a second source for one fact.
        let workspace_for_rerank = workspace.clone();
        let profile_root = flag_value(&args, "--profile-root")
            .map(PathBuf::from)
            .unwrap_or_else(agent::default_profile_root);

        let result = match modes[0] {
            "--serve" => agent::serve(
                workspace,
                profile_root,
                flag_value(&args, "--daemon-port").and_then(|v| v.parse().ok()),
                args.iter().any(|a| a == "--dev"),
                context,
                thinking,
                // **Optional here, unlike `--eval-adapter`, and the difference has a reason.**
                // A scoring run without it measures a different system under the same label, so
                // there it is a refusal. For the product the failure mode is the opposite: making
                // a 60 MB model an install-time dependency of being able to talk is K6's
                // five-minute target gone. Absent, memory is WRITE-ONLY and says so at startup and
                // on `--status` — announced rather than silently degraded.
                reranking_for_serve(&args, &workspace_for_rerank),
                flag_value(&args, "--model").map(str::to_string),
                model_provider.clone(),
            ),
            // **The port is threaded in, and its absence was the bug.** `--status --daemon-port N`
            // built the client on the DEFAULT port, found nothing, constructed a throwaway daemon
            // and printed its own defaults -- so it reported `qwen3.5:9b / ollama` while the daemon
            // it was describing ran something else. `--shutdown` two arms below always passed the
            // port; `--status` never did. The `get_providers()` family, one line wide.
            "--status" => agent::status(
                workspace,
                profile_root,
                model_provider.clone(),
                flag_value(&args, "--daemon-port").and_then(|v| v.parse().ok()),
            ),
            // `--runs` alone lists every run; `--runs <id>` prints that one in full.
            //
            // **The per-run print used to be `--watch`.** It moved here when `--watch` became the
            // window (§6.6: *"`/watch` opens a window"*), and this is where it belongs anyway: a
            // per-run listing is a listing. Nothing was lost — a script that wants a run's state
            // without a terminal reads `--runs <id>`.
            "--runs" => agent::runs(
                flag_value(&args, "--runs"),
                profile_root,
                flag_value(&args, "--daemon-port").and_then(|v| v.parse().ok()),
            ),
            // **`--watch` opens a WINDOW.** `M3-DESIGN.md` §6, and the one mode here that takes
            // over the terminal rather than printing to it.
            "--watch" => match flag_value(&args, "--watch") {
                Some(run) => watch::run(watch::Options {
                    run: run.to_string(),
                    profile_root,
                    port: flag_value(&args, "--daemon-port").and_then(|v| v.parse().ok()),
                    color_depth: flag_value(&args, "--color-depth").map(str::to_string),
                })
                .map_err(|e| e.to_string()),
                None => Err(
                    "--watch requires a run id. `marlowe --runs` lists them, and `--runs <id>`                      prints one without opening a window"
                        .to_string(),
                ),
            },
            "--cancel" => agent::cancel(
                flag_value(&args, "--cancel"),
                profile_root,
                flag_value(&args, "--daemon-port").and_then(|v| v.parse().ok()),
            ),
            "--resume" => agent::resume(
                flag_value(&args, "--resume"),
                profile_root,
                flag_value(&args, "--daemon-port").and_then(|v| v.parse().ok()),
            ),
            "--steer" => agent::steer(
                flag_value(&args, "--steer"),
                flag_value(&args, "--guidance"),
                profile_root,
                flag_value(&args, "--daemon-port").and_then(|v| v.parse().ok()),
            ),
            "--shutdown" => agent::shutdown(
                flag_value(&args, "--daemon-port").and_then(|v| v.parse().ok()),
                // The profile root decides which token is offered, so `--shutdown` needs it for
                // the same reason `--ask` does: a daemon serving another profile refuses, and
                // saying so beats a silent no-op.
                profile_root,
            ),
            _ => match flag_value(&args, "--ask") {
                Some(message) => agent::ask(
                    message,
                    workspace,
                    profile_root,
                    args.iter().any(|a| a == "--dev"),
                    context,
                    thinking,
                    flag_value(&args, "--daemon-port").and_then(|v| v.parse().ok()),
                    model_provider.clone(),
                ),
                None => Err("--ask requires a question, e.g. `marlowe --ask \"read notes.md\"`"
                    .to_string()),
            },
        };
        if let Err(e) = result {
            eprintln!("marlowe: {e}");
            std::process::exit(1);
        }
        return;
    }

    // §B17. Opens a terminal Marlowe controls, rather than assuming this one is suitable.
    if modes[0] == "--launch" {
        let extra: Vec<String> = args.iter().filter(|a| *a != "--launch").cloned().collect();
        match launcher::launch(&extra) {
            Ok(o) => {
                // Report what was chosen, always. A launcher that silently accepts a terminal it
                // could not configure is how "the font is wrong" becomes a bug against the frame.
                match &o.terminal {
                    Some(t) => println!("terminal   {t}{}", if o.profile_written {
                        " (Marlowe profile)"
                    } else {
                        " (no profile; direct launch)"
                    }),
                    None => println!("terminal   this one"),
                }
                // Global settings are named individually. Windows Terminal keeps window chrome
                // outside profiles, so theming it necessarily reaches past "adds one, modifies
                // none" — which is allowed, and is never silent.
                if !o.globals_changed.is_empty() {
                    println!("globals    {} (chrome is global in Windows Terminal; \
                              settings.json.marlowe-backup holds the previous file)",
                             o.globals_changed.join(", "));
                }
                for d in &o.degraded {
                    println!("degraded   {d}");
                }
                if o.terminal.is_none() {
                    // Nothing suitable was found. Run here rather than refuse, having said what
                    // is degraded — §B17 asks for the note, not for a dead end.
                    if let Err(e) = tui::run(tui::Options {
                        timing_probe: false,
                        diagnostic: false,
                        color_depth: None,
                        ground: true,
                        panic_probe: false,
                        scripted: args.iter().any(|a| a == "--scripted"),
                        daemon_port: None,
                        // `--launch` degraded to running here: same flags, same provider.
                        model_provider: model_provider.clone(),
                    }) {
                        eprintln!("error: {e}");
                        std::process::exit(1);
                    }
                }
            }
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    // **A flag, not an environment variable, and the reason is a measurement that failed.**
    // `wt.exe -w -1` opens the tab from the ALREADY-RUNNING Windows Terminal process, so the new
    // pane inherits that process's environment and never sees one exported alongside the launch.
    // A diagnostic that cannot be turned on where the defect lives is not a diagnostic.
    if let Some(path) = flag_value(&args, "--input-trace") {
        std::env::set_var("MARLOWE_INPUT_TRACE", path);
    }

    if modes[0] == "--tui" {
        let opts = tui::Options {
            timing_probe: args.iter().any(|a| a == "--timing-probe"),
            diagnostic: args.iter().any(|a| a == "--diagnostic"),
            color_depth: flag_value(&args, "--color-depth").map(str::to_string),
            ground: args.iter().any(|a| a == "--ground"),
            panic_probe: args.iter().any(|a| a == "--panic-probe"),
            scripted: args.iter().any(|a| a == "--scripted"),
            daemon_port: flag_value(&args, "--daemon-port").and_then(|v| v.parse().ok()),
            // **ADR-046 reaches the TUI, and it did not until now.**
            //
            // `--serve`, `--ask` and `--status` all took `--provider`; `--tui` did not, and its
            // `ensure_daemon` spawns with a FIXED argv. So `marlowe --tui --provider openrouter`
            // parsed the flag, discarded it, auto-spawned a local daemon, and answered from
            // qwen3.5:9b — the flag accepted and silently ignored, which is worse than refusing it.
            //
            // Same family as the two defects ADR-046 itself records: a control wired into one entry
            // point and not its sibling. Found by a user asking how to select it in the TUI.
            model_provider: model_provider.clone(),
        };
        if args.iter().any(|a| a == "--color-depth") && opts.color_depth.is_none() {
            eprintln!("error: --color-depth requires a value: truecolor, 256 or 16.");
            std::process::exit(2);
        }
        if let Err(e) = tui::run(opts) {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
        return;
    }

    if modes[0] == "--classic" {
        let stdin = io::stdin();
        let clock = marlowe_stub::Clock::real();
        let mut session = marlowe_stub::Session::new();
        if let Err(e) =
            marlowe_surface::cli::run(&mut session, stdin.lock(), io::stdout(), &clock)
        {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
        return;
    }

    if modes[0] == "--doctor" {
        // `--doctor` reports on a scripted view deliberately: it is a TERMINAL capability
        // check, and giving it a `SessionView::default()` would have been the permissive default
        // this project keeps deleting. There is no `Default` on `SessionView` to reach for.
        let doctor_session = marlowe_stub::Session::new();
        for line in marlowe_surface::doctor::report(doctor_session.view()) {
            println!("{line}");
        }
        return;
    }

    let profile_root = match flag_value(&args, "--profile-root") {
        Some(v) => PathBuf::from(v),
        None => {
            eprintln!("{USAGE}");
            // Required rather than defaulted to a temp directory. A default would let two
            // spawns share one root, and the clock probe would compare contaminated runs
            // while reporting a clean verdict.
            eprintln!("error: --profile-root is required and has no default.");
            std::process::exit(2);
        }
    };

    // Present-but-empty is a typo, not a request for a default path. Named explicitly because
    // the alternative -- silently dumping to some default file -- would let a fit run against
    // a stale dump from an earlier session and produce an artifact nobody could reproduce.
    if args.iter().any(|a| a == "--dump-gate-features")
        && flag_value(&args, "--dump-gate-features").is_none()
    {
        eprintln!("{USAGE}");
        eprintln!("error: --dump-gate-features requires a path and has no default.");
        std::process::exit(2);
    }

    // Same rule as --dump-gate-features: present-but-empty is a typo, not a request for a
    // default path. A default here would let a sweep land in a stale file from an earlier run
    // and produce a threshold nobody could reproduce.
    if args.iter().any(|a| a == "--dump-consolidation")
        && flag_value(&args, "--dump-consolidation").is_none()
    {
        eprintln!("{USAGE}");
        eprintln!("error: --dump-consolidation requires a path and has no default.");
        std::process::exit(2);
    }

    // Same rule again: present-but-empty is a typo, not a request for a default path. A default
    // here would let a profile land in a file from an earlier run and produce a P95 that mixes
    // two configurations -- which is the one thing a latency comparison must never do.
    if args.iter().any(|a| a == "--profile-retrieval")
        && flag_value(&args, "--profile-retrieval").is_none()
    {
        eprintln!("{USAGE}");
        eprintln!("error: --profile-retrieval requires a path and has no default.");
        std::process::exit(2);
    }

    // **The default is `auto` as of ADR-045, 2026-08-17. It was `cpu`.** See
    // `rerank_provider_choice` for the measurement that licensed it, and note that the value here
    // is a REQUEST -- the resolution is not known until the graph has loaded, which is why
    // `rerank_batched` below can no longer be derived from it.
    let rerank_choice = match rerank_provider_choice(&args) {
        Ok(c) => c,
        Err(message) => {
            eprintln!("{USAGE}");
            eprintln!("{message}");
            std::process::exit(2);
        }
    };
    let rerank_plan_flag = match rerank_plan_choice(&args) {
        Ok(c) => c,
        Err(message) => {
            eprintln!("{USAGE}");
            eprintln!("{message}");
            std::process::exit(2);
        }
    };
    let rerank_batch_flag = match flag_value(&args, "--rerank-batch") {
        Some("on") => Some(true),
        Some("off") => Some(false),
        None => None,
        Some(other) => {
            eprintln!("{USAGE}");
            eprintln!("error: --rerank-batch takes `on` or `off`, got {other:?}.");
            std::process::exit(2);
        }
    };
    let rerank_threads = match flag_value(&args, "--rerank-threads") {
        Some(v) => match v.parse::<usize>() {
            Ok(n) if n >= 1 => n,
            _ => {
                eprintln!("{USAGE}");
                eprintln!("error: --rerank-threads must be a positive integer, got {v:?}.");
                std::process::exit(2);
            }
        },
        None => marlowe_memory::rerank::SHIPPED_THREADS,
    };
    let dump_path = flag_value(&args, "--dump-gate-features").map(std::path::Path::new);
    let profile_path = flag_value(&args, "--profile-retrieval").map(std::path::Path::new);
    let fit_mode = args.iter().any(|a| a == "--fit-mode");
    let consolidation_dump = flag_value(&args, "--dump-consolidation").map(std::path::Path::new);
    let consolidation = if args.iter().any(|a| a == "--consolidation-dry-run") {
        if consolidation_dump.is_none() {
            eprintln!("{USAGE}");
            // Refused rather than defaulted. A dry run applies nothing, so a dry run with nowhere
            // to write its sweep is a run that changes nothing and records nothing -- silently
            // useless, and indistinguishable from a consolidated run that merged nothing.
            eprintln!("error: --consolidation-dry-run requires --dump-consolidation.");
            std::process::exit(2);
        }
        adapter::Consolidate::DryRun
    } else {
        adapter::Consolidate::Frozen
    };

    // Required, with no default, for the same reason --profile-root is: a default path here
    // would let a run silently pick up whatever model happened to be lying around, and a
    // different model still embeds, still scores, and still produces a number.
    let embedder_model = match flag_value(&args, "--embedder-model") {
        Some(v) => PathBuf::from(v),
        None => {
            eprintln!("{USAGE}");
            eprintln!(
                "error: --embedder-model is required and has no default. Fetch it with `python tools/fetch_model.py`."
            );
            std::process::exit(2);
        }
    };
    // Required, and it takes an explicit value. See USAGE: a bare boolean here is exactly the
    // failure mode the consolidation flag exists to avoid.
    let reranking = match flag_value(&args, "--reranking") {
        Some(v) if v == "off" => None,
        Some(v) => Some(PathBuf::from(v)),
        None => {
            eprintln!("{USAGE}");
            eprintln!(
                "error: --reranking is required and has no default. Pass `off` to disable the \
                 cross-encoder explicitly, or the directory holding its pinned files."
            );
            std::process::exit(2);
        }
    };
    // **An explicit cascade with no reranker is a contradiction, refused where both flags are in
    // scope.** `start_with` would also refuse it; refusing here names the flag pair instead of
    // making a reader infer it.
    if rerank_plan_flag == PlanChoice::Cascade && reranking.is_none() {
        eprintln!("{USAGE}");
        eprintln!("error: --rerank-plan cascade requires --reranking <DIR>; there is nothing to narrow or fuse without a cross-encoder.");
        std::process::exit(2);
    }

    // **The un-gated control arm for K1 condition 3, and it is measurement-only.**
    //
    // After M2 Session D the poisoning suite reported ASR 0.000 across five families and was
    // *still* vacuous: the declared 10%-coverage point abstains on nearly everything, so an attack
    // that fails is indistinguishable from one that was never given a chance. The pair that
    // discriminates is ASR at `declared` versus at `full`.
    //
    // Unlike `--reranking` this is optional, and the direction is why. There the danger is a
    // forgotten flag measuring the un-reranked system under a reranked label; here the shipped
    // configuration IS `declared`, so a forgotten flag yields the truthful label and the dangerous
    // default would be `full`. An unrecognised value is still a refusal rather than a fallback, and
    // the chosen arm is stamped into the gate version on every response, so an artifact records
    // which arm produced it rather than depending on a command line nobody kept.
    let coverage = match flag_value(&args, "--injection-coverage") {
        None => marlowe_memory::Coverage::Declared,
        Some(v) => match marlowe_memory::Coverage::parse(v) {
            Some(c) => c,
            None => {
                eprintln!("{USAGE}");
                eprintln!(
                    "error: --injection-coverage takes `declared` or `full`, not {v:?}. `full` is \
                     a MEASUREMENT arm that ignores the published cut point and must never produce \
                     a shipped number; it exists so an ASR of 0.000 can be attributed to the guard \
                     rather than to nothing being injected."
                );
                std::process::exit(2);
            }
        },
    };

    let cache_dir = flag_value(&args, "--embedding-cache").map(PathBuf::from);
    let workers = match flag_value(&args, "--embedder-workers") {
        Some(v) => match v.parse::<usize>() {
            Ok(n) if n >= 1 => n,
            _ => {
                eprintln!("{USAGE}");
                eprintln!("error: --embedder-workers must be a positive integer, got {v:?}.");
                std::process::exit(2);
            }
        },
        None => std::thread::available_parallelism().map_or(1, |n| n.get().min(8)),
    };

    // **The default is `auto` as of ADR-044, 2026-08-17. It was `cpu`, and what changed is a
    // measurement, not an opinion.**
    //
    // ADR-013 deferred GPU for the retrieval path pending its own ADR and ADR-015 required a
    // per-provider baseline before one graph's number could be read as another's. Both are now
    // answered: on the fit split the CPU and CUDA arms pick the SAME session on 242 of 242
    // queries -- net 0, McNemar p = 1.0 -- while 99.84% of the 117,890 candidate rows moved their
    // `dense_cosine`, which is the control that stops "identical" from meaning "the run never
    // happened". `runs/session-e-cuda/`, prediction registered at `9db08b9` before the run.
    //
    // **CUDA still FAILS the HuggingFace reference tolerance** -- median 3.072e-5, max 1.063e-4
    // against a `MAX_ABS_DIFF` of 1e-4, where CPU reads 1.043e-7. Nothing was widened and the
    // fixture was not regenerated. The tolerance is a proxy for "did the scorer move"; the
    // decision is the property, and the decision did not move. Same amendment ADR-029 made on the
    // cross-encoder, on a second scorer, with the same shape of evidence.
    //
    // **`auto`, not `cuda`, and the difference is the whole design.** `cuda` is the REFUSAL arm:
    // it errors rather than falling back, which is right for a measurement cell and unacceptable
    // in a product, because a user whose card is full would get a binary that will not start.
    // `auto` never fails a run over a busy card -- it narrows the GPU width, then falls to CPU at
    // the full requested width, and says which it did.
    //
    // The cost, accepted and recorded in ADR-044 §5: `auto` resolves against free VRAM at that
    // instant, so two spawns on one machine can select two different scorers. That is why the
    // RESOLVED provider is announced below rather than the requested one, why it is in
    // `CacheIdentity` so the vectors cannot mix, and why `cuda` still exists for anything that
    // publishes a number.
    // **Which model is TIER 1 on this machine — ADR-045 §4.** The eval adapter never runs a
    // language model, so it cannot observe one; but the card it is about to take is the same card
    // the language model will want, and tier 1 has no CPU fallback. The default is this build's
    // routing constant rather than a literal spelled here, which keeps one source for the routed
    // model's identity. `marlowe --serve` does not use this: the daemon passes `config.model`,
    // which is switchable at runtime, and a compile-time default would protect the wrong model
    // the moment a user switched.
    let tier1_model =
        flag_value(&args, "--tier1-model").unwrap_or(marlowe_provider::DEFAULT_MODEL).to_string();

    // **WHICH PROCESS holds tier 1, not only which model — ADR-060 §3.** The reserve used to know
    // a name and ask Ollama about it, which is the right question only when Ollama is what runs
    // tier 1. It is a required field on `Reserve::ForTier1` precisely so this call site cannot
    // mean "Ollama" by omission, and `ollama` is stated here rather than defaulted: the eval
    // adapter's own tier 1 is Ollama unless somebody says otherwise, and now they can.
    let tier1_runtime = match flag_value(&args, "--tier1-runtime") {
        None | Some("ollama") => marlowe_memory::cue::dense::vram::Tier1Runtime::Ollama,
        Some("llama-server") => {
            marlowe_memory::cue::dense::vram::Tier1Runtime::LlamaServerLoaded
        }
        Some("off") => marlowe_memory::cue::dense::vram::Tier1Runtime::NotOnThisCard,
        Some(other) => {
            eprintln!("{USAGE}");
            eprintln!(
                "error: --tier1-runtime {other:?} is not a runtime. Valid values are `ollama` \
                 (the default), `llama-server` (a llama-server that is ALREADY LOADED — its bytes \
                 are then already out of memory.free) and `off` (tier 1 is not on this card)."
            );
            std::process::exit(2);
        }
    };

    let embedder_provider = match embedder_provider_choice(&args) {
        Ok(c) => c,
        Err(message) => {
            eprintln!("{USAGE}");
            eprintln!("{message}");
            std::process::exit(2);
        }
    };

    let embedder = match marlowe_memory::cue::dense::embedder::Embedder::load_with_provider(
        &embedder_model,
        workers,
        cache_dir.as_deref(),
        embedder_provider,
        marlowe_memory::cue::dense::vram::Probe::Device,
        // **Tier 3 yields to tier 1 -- ADR-045.** The model name comes from the provider crate
        // rather than being spelled here: a second copy of the routed model's identity is how the
        // reserve would end up protecting a model nobody runs.
        marlowe_memory::cue::dense::vram::Reserve::ForTier1 {
            model: &tier1_model,
            runtime: tier1_runtime,
        },
    ) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("marlowe: {e}");
            std::process::exit(1);
        }
    };
    // **ADR-029's rule, ADR-044's obligation: the RESOLVED provider is announced, never inferred.**
    // Printed unconditionally, before a single query runs, and it carries the REQUEST beside the
    // resolution -- see `embedder_announcement` for why both halves are load-bearing now that the
    // default is `auto`.
    eprintln!("marlowe: {}", embedder_announcement(embedder_provider, embedder.plan()));

    // Loaded HERE rather than at first retrieval, for the same reason the gate is: a bad artifact
    // must stop the process, not become a per-query error the harness scores as a wrong number.
    let cross_encoder = match &reranking {
        Some(dir) => match marlowe_memory::rerank::CrossEncoder::load_auto(
            dir,
            rerank_threads,
            rerank_choice,
            marlowe_memory::cue::dense::vram::Probe::Device,
            marlowe_memory::cue::dense::vram::Reserve::ForTier1 {
                model: &tier1_model,
                runtime: tier1_runtime,
            },
        ) {
            Ok(e) => Some(e),
            Err(e) => {
                eprintln!("marlowe: {e}");
                std::process::exit(1);
            }
        },
        None => None,
    };
    // **THE RESOLVED PROVIDER, NOT THE REQUEST, AND BOTH FIELDS BELOW DEPEND ON IT.** ADR-045.
    //
    // Before the default became `auto` these two lines could be written beside the flag parsing,
    // because the request WAS the resolution. Under `auto` they cannot: `RerankProvider` is what
    // actually scored, and `default_batching()` measured OPPOSITE on the two providers -- CPU is
    // faster sequential (185.8 vs 195.6 ms), CUDA is faster batched (3.4 vs 15.2). Deriving the
    // batching from the *request* would run a CPU fallback in CUDA's batched shape, which is the
    // slower of the two on that provider and would be invisible in every log.
    //
    // `provider` is stamped on every retrieval-profile row, and a row carrying the request rather
    // than the resolution is the "declared control that nothing reads" family with the reader
    // present and pointed at the wrong value.
    let resolved_provider = cross_encoder
        .as_ref()
        .map(|e| e.provider())
        // With `--reranking off` nothing loaded, so nothing scored. `Cpu` is the inert value here
        // and it is never read: `RerankSettings` only reaches a profile row a rerank produced.
        .unwrap_or(marlowe_memory::rerank::RerankProvider::Cpu);

    // **Which SHAPE this run uses, decided ONCE, at load, and announced** -- `RerankPlan`'s own
    // contract. `auto` keys on whether the cross-encoder RESOLVED to CUDA, not on whether a GPU
    // exists: the cascade is GPU-only BY MEASUREMENT (CPU p50 1134 ms against §5.7's 300 ms), so
    // "the reranker is on the GPU" is the fact that matters. An explicit arm is taken as given --
    // it is a measurement setting -- and is still announced like everything else.
    let plan = match rerank_plan_flag {
        PlanChoice::Auto => marlowe_memory::retrieve::RerankPlan::select(
            resolved_provider == marlowe_memory::rerank::RerankProvider::Cuda,
        ),
        PlanChoice::Shipped => marlowe_memory::retrieve::RerankPlan::Shipped,
        PlanChoice::Cascade => marlowe_memory::retrieve::RerankPlan::Cascade,
    };

    // The cascade's second graph, digest-pinned through FUSION_GRAPHS -- there is no caller-supplied
    // digest and no second way to load it. Its provider follows the RESOLVED one: under auto→CUDA
    // both graphs are CUDA explicitly (a failure to construct stops the process, which is correct --
    // the cascade was selected because CUDA was proven), and an explicit CPU measurement run keeps
    // both on CPU. Loaded beside the primary for the same reason the primary loads here: a missing
    // or swapped graph must stop the run before its first query, not per query.
    let fusion_encoder = if plan == marlowe_memory::retrieve::RerankPlan::Cascade {
        let dir = reranking.as_ref().expect("refused above: cascade without --reranking");
        let models_root = dir.parent().unwrap_or_else(|| std::path::Path::new("."));
        match marlowe_memory::rerank::CrossEncoder::load_fusion_member(
            models_root,
            marlowe_memory::rerank::CASCADE_FUSE_GRAPH,
            resolved_provider,
        ) {
            Ok(e) => Some(e),
            Err(e) => {
                eprintln!("marlowe: {e}");
                std::process::exit(1);
            }
        }
    } else {
        None
    };

    let rerank_settings = adapter::RerankSettings {
        batched: rerank_batch_flag.unwrap_or_else(|| resolved_provider.default_batching()),
        threads: rerank_threads,
        provider: resolved_provider,
        plan,
    };
    // ADR-029's rule, ADR-045's obligation: the RESOLVED provider is announced, never inferred,
    // and the REQUEST is on the line beside it so a fallback to CPU is legible as a fallback
    // rather than as a configuration. Printed only when a cross-encoder actually loaded -- a line
    // about a stage that did not run is the widest possible gap between an event and a claim.
    if let Some(e) = cross_encoder.as_ref() {
        eprintln!("marlowe: {}", rerank_announcement(rerank_choice, e.plan(), rerank_settings.batched));
        // The plan is on ITS OWN line rather than folded into the provider line: the two shapes
        // have different published numbers, and a reader comparing a run against one of them must
        // be able to see which shape produced it without parsing compound sentences.
        eprintln!("marlowe: rerank plan {}", plan.label());
    }

    let consolidation = adapter::Consolidation {
        policy: consolidation,
        dump_path: consolidation_dump,
    };
    let started = match (fit_mode, dump_path) {
        (true, Some(path)) => adapter::Adapter::start_for_fit(
            &profile_root,
            embedder,
            path,
            consolidation,
            cross_encoder,
            fusion_encoder,
            rerank_settings,
            profile_path,
        ),
        (true, None) => {
            eprintln!("{USAGE}");
            // Refused rather than defaulted to a path. Fit mode with nowhere to write is a run
            // that loads no gate and produces nothing -- silently useless.
            eprintln!("error: --fit-mode requires --dump-gate-features.");
            std::process::exit(2);
        }
        (false, path) => adapter::Adapter::start(
            &profile_root,
            embedder,
            adapter::Diagnostics { gate_features: path, retrieval_profile: profile_path },
            consolidation,
            cross_encoder,
            fusion_encoder,
            rerank_settings,
            coverage,
        ),
    };

    let mut adapter = match started {
        Ok(a) => a,
        Err(e) => {
            // stderr is diagnostic only and is never parsed by the harness (section 4.0.1).
            // An unfitted gate artifact lands here, and the error names the command that
            // regenerates it -- see `marlowe_memory::gate::GateError`.
            eprintln!("marlowe: could not start against {}: {e}", profile_root.display());
            std::process::exit(1);
        }
    };

    let stdin = BufReader::new(io::stdin());
    // Locked once for the whole run rather than re-acquired per frame.
    let stdout = io::stdout();
    let stdout = stdout.lock();

    if let Err(e) = adapter.run(stdin, stdout) {
        eprintln!("marlowe: transport failure: {e}");
        std::process::exit(1);
    }

    // Section 4.0.6: the harness closes stdin, the implementation flushes and exits 0.
}

/// Which provider serves this process's model calls. **ADR-046.**
///
/// # Every branch that is not `ollama` exits rather than degrading
///
/// The three ways this can be wrong are a bad `--provider` value, a missing `--openrouter-model`
/// and a missing `OPENROUTER_API_KEY`. All three exit with code 2 and name what is missing.
///
/// **None of them falls back to Ollama**, and that is the decision worth stating. A fallback is
/// the obvious kindness and it is exactly wrong here: this path exists for benchmark runs, and a
/// benchmark that silently measured a local 9B under a frontier model's label is not a degraded
/// result, it is a wrong one — and nothing downstream would ever observe the substitution, because
/// every label in the output would read the name that was asked for.
///
/// # `OPENROUTER_API_KEY` alone cannot select this path
///
/// It is checked **only** inside the `openrouter` branch. An environment variable that could flip
/// the provider on its own would mean a machine with a key exported for some other tool answers
/// `marlowe --ask` over the network and bills for it — K6 gone, silently, on a machine where
/// nothing was configured. `tests/zero_config_is_unchanged.rs` asserts that at the daemon.
/// The cross-encoder directory for `--serve`: the flag if given, otherwise **the pinned model if it
/// is sitting in the workspace**.
///
/// # Absent is not the same as off, and the daemon could not tell the difference
///
/// `--reranking` is optional here on purpose — see the call site — and the intent was that a user
/// without the model still gets a daemon rather than a refusal. What actually happened is that
/// **nothing passes the flag on the path a user takes.** The Windows Terminal shortcut runs
/// `--tui --ground`; the TUI's `spawn_args` builds the daemon's argv and has no `--reranking` in
/// it. So every daemon Matthew has ever launched reported `rerank not-loaded` and
/// `memory retrieval WRITE-ONLY`, with the pinned model sitting in `models/` the whole time.
///
/// That is the shape CLAUDE.md keeps naming: a default that makes a mismatch unobservable. The
/// daemon announced the degradation honestly and nobody could act on it, because the remedy was a
/// flag no launch path offered.
///
/// # Why a search rather than a compiled default
///
/// The path is workspace-relative, so a constant would be wrong for anyone who checked the repo out
/// somewhere else, and `models/` is gitignored — a fresh clone genuinely does not have it. Looking
/// is the only thing that can tell those two cases apart, and the answer is announced either way.
///
/// **`off` still means off**, explicitly, and an explicit `--reranking <dir>` still wins. This only
/// fills the case where nobody said anything, which used to mean "silently write-only".
fn reranking_for_serve(args: &[String], workspace: &std::path::Path) -> Option<PathBuf> {
    match flag_value(args, "--reranking") {
        Some("off") => return None,
        Some(v) => return Some(PathBuf::from(v)),
        None => {}
    }
    // ADR-018/ADR-020's pinned graph: the Session J fine-tune, f32. The int8 directory it replaced
    // is deliberately NOT a candidate — CLAUDE.md refuses it by name, because scoring the old graph
    // under the shipped label is exactly the mismatch this whole function exists to stop.
    let pinned = workspace.join("models").join("ms-marco-MiniLM-L-2-v2-ft-session-j");
    pinned.is_dir().then_some(pinned)
}

fn resolve_provider(args: &[String]) -> marlowe_daemon::ModelProviderChoice {
    use marlowe_daemon::ModelProviderChoice;

    let named = flag_value(args, "--provider");
    if named.is_none() && args.iter().any(|a| a == "--provider") {
        eprintln!("error: --provider requires a value: `ollama`, `openrouter` or `llamacpp`.");
        std::process::exit(2);
    }
    match named {
        // **Back to Ollama, 2026-08-27.** The hybrid is shelved — see
        // `marlowe_view::provider::PROVIDERS` for the reason, which is tool calling and not speed.
        // The engine's numbers stand; its tool-call parsing did not survive real use.
        None => ModelProviderChoice::Ollama,
        Some("ollama") => ModelProviderChoice::Ollama,
        Some("openrouter") => {
            let Some(model) = flag_value(args, "--openrouter-model") else {
                eprintln!(
                    "error: --provider openrouter requires --openrouter-model <SLUG>, and it has \
                     no default.\n       \
                     Nothing on OpenRouter has been measured by this project, so a built-in slug \
                     would read as a recommendation nobody made.\n       \
                     Example: --openrouter-model anthropic/claude-sonnet-4.5\n       \
                     The catalogue is at https://openrouter.ai/models"
                );
                std::process::exit(2);
            };
            // **Checked HERE, at load, and not at the first turn.** A key discovered missing
            // mid-run is a turn that degrades; a key discovered missing at startup is a command
            // that did not run. The second is what the user can act on.
            if let Err(e) = marlowe_openrouter::ApiKey::from_environment() {
                eprintln!("error: {e}");
                std::process::exit(2);
            }
            ModelProviderChoice::OpenRouter { model: model.to_string() }
        }
        // **Two spellings, one meaning, and neither is a default.** `ollama/llama.cpp` is what
        // the picker offers and what `/provider` takes; `llamacpp` is the same choice typed
        // without a slash, which a shell user will reach for. Both are explicit — an alias is not
        // a default, and nothing here is reachable by omission.
        Some(marlowe_view::provider::HYBRID) | Some("llamacpp") => {
            // **ADR-060, accepted as the hybrid.** Ollama stores, downloads and lists; a
            // `llama-server` this daemon starts and owns serves off the blob Ollama already holds.
            // Ollama stays the compiled default.
            //
            // Nothing is checked against the network here: the daemon starts the engine at
            // `Daemon::open` and falls back to Ollama with a stated reason if it cannot. What IS
            // checked at load is the port, because a bad port is a typo the user can fix now and a
            // fallback four minutes from now otherwise.
            let port = match flag_value(args, "--llamacpp-port") {
                None => marlowe_provider::LLAMACPP_DEFAULT_PORT,
                Some(v) => match v.parse::<u16>() {
                    Ok(p) if p > 0 => p,
                    _ => {
                        eprintln!("error: --llamacpp-port {v:?} is not a port number.");
                        std::process::exit(2);
                    }
                },
            };
            // **An explicit value with no bare-boolean form**, for the reason `--reranking` has
            // one: Ollama applies the model's `.params` layer and `llama-server` pointed at the raw
            // blob does not, so a forgotten switch runs the same weights at a different temperature
            // under the same label. `ollama` is the default because it is what the model was
            // published with; `server` is a recorded choice, never reached by omission.
            let sampling = match flag_value(args, "--llamacpp-sampling") {
                None | Some("ollama") => {
                    marlowe_provider::llamacpp::SamplingSource::OllamaParams
                }
                Some("server") => marlowe_provider::llamacpp::SamplingSource::ServerDefaults,
                Some(other) => {
                    eprintln!(
                        "error: --llamacpp-sampling {other:?} is not a source. Valid values are \
                         `ollama` (the default: the model's own .params layer, which is what \
                         Ollama would have applied) and `server` (llama-server's own defaults — \
                         temperature 0.8, top_k 40, presence_penalty 0)."
                    );
                    std::process::exit(2);
                }
            };
            // **Refused at load, against the port this invocation will actually use.** The two
            // defaults collided once already (both 11435) and no unit test could see it, because
            // no single process knows both constants. This one does.
            let daemon_port = flag_value(args, "--daemon-port")
                .and_then(|v| v.parse::<u16>().ok())
                .unwrap_or(marlowe_daemon::DEFAULT_DAEMON_PORT);
            if port == daemon_port {
                eprintln!(
                    "error: --llamacpp-port {port} is Marlowe's own daemon port. A llama-server \
                     cannot be there. Pick another port for the server, or move the daemon with \
                     --daemon-port."
                );
                std::process::exit(2);
            }
            if port == marlowe_provider::LocalEndpoint::DEFAULT_PORT {
                eprintln!(
                    "error: --llamacpp-port {port} is Ollama's port. This provider exists to run \
                     BESIDE Ollama, which stays the model store and the default."
                );
                std::process::exit(2);
            }
            let Some(endpoint) = marlowe_provider::LocalEndpoint::new("127.0.0.1", port) else {
                eprintln!("error: 127.0.0.1:{port} is not a loopback endpoint.");
                std::process::exit(2);
            };
            ModelProviderChoice::LlamaCpp { endpoint, sampling }
        }
        Some(other) => {
            eprintln!(
                "error: --provider {other:?} is not a provider. Valid values are `ollama` (the \
                 default, local, zero-config), `openrouter` (hosted, needs a key) and `llamacpp` \
                 (a local llama-server you start yourself — ADR-060)."
            );
            std::process::exit(2);
        }
    }
}

fn flag_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    let index = args.iter().position(|a| a == flag)?;
    args.get(index + 1)
        .map(String::as_str)
        // A following token that is itself a flag means the value was omitted.
        .filter(|v| !v.starts_with("--"))
}

/// Resolve `--embedder-provider`. **The default is `auto` — ADR-044.**
///
/// Extracted from `main` for one reason: a default nothing can call is a default nothing can
/// test, and this project's ledger is mostly controls that were declared rather than exercised.
/// `main` is unreachable from a test, so the flip lived in a line no assertion could see.
///
/// The error is returned rather than printed so the caller owns `USAGE` and the exit code.
fn embedder_provider_choice(
    args: &[String],
) -> Result<marlowe_memory::cue::dense::embedder::ProviderChoice, String> {
    use marlowe_memory::cue::dense::embedder::{EmbedProvider, ProviderChoice};
    match flag_value(args, "--embedder-provider") {
        // **ADR-044.** GPU where a CUDA session constructs and device memory holds one with a
        // spare, CPU otherwise, and never a failed run over a busy card.
        None => Ok(ProviderChoice::Auto),
        Some(v) => EmbedProvider::parse(v).ok_or_else(|| {
            format!(
                "error: --embedder-provider takes `cpu`, `cuda` or `auto`, got {v:?}. The default \
                 is `auto` (ADR-044): GPU where one constructs and fits, CPU otherwise, never a \
                 failed run. `cuda` is the REFUSAL arm -- it errors rather than falling back, \
                 because a cell that silently ran on CPU under a CUDA label is the failure that \
                 flag exists to prevent, and it is the only value safe to publish a number under."
            )
        }),
    }
}

/// Resolve `--rerank-provider`. **The default is `auto` — ADR-045.**
///
/// Extracted from `main` for the same reason `embedder_provider_choice` is: a default nothing can
/// call is a default nothing can test, and `main` is unreachable from a test.
///
/// **It was `cpu`, and what changed is a measurement, not an opinion.** ADR-029's own headline says
/// *"where a CUDA device is available the rerank runs on it"*, and the shipped default has
/// disagreed with its own ADR since that ADR was adopted. What was missing was a ranking
/// measurement on **this** graph — ADR-029's 0-of-229 was taken on the int8 graph at ORT 1.24.2,
/// which is a different system and is a reason to expect agreement rather than evidence for it.
/// See `runs/session-e-rerank-cuda/`.
///
/// The error is returned rather than printed so the caller owns `USAGE` and the exit code.
fn rerank_provider_choice(args: &[String]) -> Result<marlowe_memory::rerank::RerankChoice, String> {
    use marlowe_memory::rerank::RerankChoice;
    match flag_value(args, "--rerank-provider") {
        // **ADR-045.** GPU where a CUDA session constructs and the card has room, CPU otherwise,
        // and never a failed run over a busy card.
        None => Ok(RerankChoice::Auto),
        Some(v) => RerankChoice::parse(v).ok_or_else(|| {
            format!(
                "error: --rerank-provider takes `cpu`, `cuda` or `auto`, got {v:?}. The default \
                 is `auto` (ADR-045): GPU where one constructs and fits, CPU otherwise, never a \
                 failed run. `cuda` is the REFUSAL arm -- it errors rather than falling back, \
                 because a cell that silently ran on CPU under a CUDA label is the failure that \
                 flag exists to prevent, and it is the only value safe to publish a number under."
            )
        }),
    }
}

/// Resolve `--rerank-plan`. **The default is `auto`**: the cascade where it is measured to fit.
///
/// `auto` is a rule, not a preference: the cascade is GPU-only BY MEASUREMENT (CPU p50 1134 ms
/// against §5.7's 300 ms), so it keys on whether the cross-encoder RESOLVED to CUDA — a
/// construction-proven fact, not an availability list. `shipped` and `cascade` are the explicit
/// measurement arms; `cascade` additionally requires both graphs and refuses otherwise, which is
/// `start_with`'s validation rather than this function's.
fn rerank_plan_choice(args: &[String]) -> Result<PlanChoice, String> {
    match flag_value(args, "--rerank-plan") {
        None => Ok(PlanChoice::Auto),
        Some("auto") => Ok(PlanChoice::Auto),
        Some("shipped") => Ok(PlanChoice::Shipped),
        Some("cascade") => Ok(PlanChoice::Cascade),
        Some(v) => Err(format!(
            "error: --rerank-plan takes `auto`, `shipped` or `cascade`, got {v:?}. `auto` runs \
             the cascade where the cross-encoder resolved to CUDA and the shipped depth-10 path \
             everywhere else; the two explicit values are measurement arms."
        )),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlanChoice {
    Auto,
    Shipped,
    Cascade,
}

/// The rerank startup line: **what was asked for, what was obtained, and how it will batch.**
///
/// The batching is on the line because it is *derived from the resolution* and the two providers
/// measured opposite — CPU sequential 185.8 ms against batched 195.6, CUDA batched 3.4 against
/// sequential 15.2 (ADR-029). Under a `cpu` default the batching followed the request and the two
/// were the same fact; under `auto` they are not, and a run that fell back to CPU while batching
/// like a GPU would be slower than either published configuration with nothing saying so.
///
/// **This function formats what the source will emit. That is the weaker claim** — the stronger one
/// is the running binary's own stderr, and ADR-045 §7 reads it there.
fn rerank_announcement(
    requested: marlowe_memory::rerank::RerankChoice,
    plan: &marlowe_memory::rerank::RerankProviderPlan,
    batched: bool,
) -> String {
    format!(
        "rerank asked for {}, running on {}, {} -- {}",
        requested.asked(),
        plan.provider.name(),
        if batched { "batched" } else { "sequential" },
        plan.reason
    )
}

/// The startup line: **what was asked for, and what was obtained.**
///
/// Both halves, and neither is decoration. Before ADR-044 the default was `cpu`, so a line reading
/// `embedder on CPUExecutionProvider` described a configuration. With `auto` as the default the
/// same line has two utterly different causes — a machine with no card, or a user who typed
/// `--embedder-provider cpu` — and ADR-029's rule is that an unannounced fallback is
/// indistinguishable from the failure mode it resembles. Printing the request beside the
/// resolution is what makes the fallback *visible as one*.
///
/// The width is here for the same reason: under `auto` it is derived from free device memory at
/// load, so `6 of 8` is a fact about the card at that instant and not about the configuration.
///
/// **This function formats what the source will emit. That is the weaker claim** — the stronger one
/// is the running binary's own stderr, and ADR-044 §7 says to read it there.
fn embedder_announcement(
    requested: marlowe_memory::cue::dense::embedder::ProviderChoice,
    plan: &marlowe_memory::cue::dense::embedder::ProviderPlan,
) -> String {
    use marlowe_memory::cue::dense::embedder::ProviderChoice;
    let asked = match requested {
        ProviderChoice::Auto => "auto",
        ProviderChoice::Cpu => "cpu",
        ProviderChoice::Cuda => "cuda",
    };
    format!(
        "embedder asked for {asked}, running on {} with {} of {} worker session(s) -- {}",
        plan.provider.name(),
        plan.workers,
        plan.requested,
        plan.reason
    )
}

#[cfg(test)]
mod embedder_provider_flag {
    use super::*;
    use marlowe_memory::cue::dense::embedder::{EmbedProvider, ProviderChoice, ProviderPlan};

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }


    #[test]
    fn the_default_is_auto() {
        // **ADR-044.** This is the assertion that fails if the flip is reverted, and it is the
        // whole reason `embedder_provider_choice` exists as a function.
        let a = args(&["--eval-adapter", "--profile-root", "p"]);
        assert_eq!(
            embedder_provider_choice(&a).expect("an absent flag is not an error"),
            ProviderChoice::Auto,
            "the embedder default is `auto` (ADR-044). `cpu` here would silently un-ship the GPU \
             path on every machine that has one, with nothing in the output to say so"
        );
    }

    #[test]
    fn the_explicit_arms_still_reach_the_loader() {
        // The default must not swallow the flag: `cpu` is what every number published before
        // 2026-08-17 was taken on, and `cuda` is the refusal arm every future measurement needs.
        for (value, expected) in [
            ("cpu", ProviderChoice::Cpu),
            ("cuda", ProviderChoice::Cuda),
            ("auto", ProviderChoice::Auto),
        ] {
            let a = args(&["--eval-adapter", "--embedder-provider", value]);
            assert_eq!(
                embedder_provider_choice(&a).expect("a valid value parses"),
                expected,
                "--embedder-provider {value} must reach the loader"
            );
        }
    }

    #[test]
    fn an_unknown_value_is_refused_rather_than_defaulted() {
        // A permissive default here is the family CLAUDE.md names: a typo would run `auto` and
        // report nothing, which is a mismatch made unobservable.
        let a = args(&["--eval-adapter", "--embedder-provider", "gpu"]);
        let message = embedder_provider_choice(&a).expect_err("`gpu` is not a provider");
        assert!(message.contains("auto"), "{message}");
        assert!(message.contains("REFUSAL"), "{message}");
    }

    #[test]
    fn a_flag_with_no_value_takes_the_default_and_that_is_recorded_not_discovered() {
        // `--embedder-provider --reranking off` is a value that was omitted. `flag_value` filters
        // the following `--` token, so it reaches the None arm and resolves to `auto`. Asserted
        // so the behaviour is KNOWN rather than found later in a run nobody can explain.
        let a = args(&["--embedder-provider", "--reranking", "off"]);
        assert_eq!(embedder_provider_choice(&a).expect("no value"), ProviderChoice::Auto);
    }

    fn plan(provider: EmbedProvider, workers: usize, reason: &str) -> ProviderPlan {
        ProviderPlan {
            provider,
            workers,
            requested: 8,
            free_at_load: Some(6_000 * 1024 * 1024),
            session_cost: None,
            reason: reason.to_string(),
        }
    }

    #[test]
    fn the_announcement_names_what_was_asked_and_what_was_obtained() {
        // The case ADR-044 §5 is about: the request and the resolution DISAGREE, and a line that
        // printed only one of them would be read as the other.
        let line = embedder_announcement(
            ProviderChoice::Auto,
            &plan(EmbedProvider::Cpu, 8, "a CUDA session did not construct: cublasLt64_12.dll"),
        );
        assert!(line.contains("asked for auto"), "the REQUEST must be on the line: {line}");
        assert!(
            line.contains("CPUExecutionProvider"),
            "the RESOLVED provider must be on the line: {line}"
        );
        assert!(
            line.contains("did not construct"),
            "the reason is what makes a fallback legible as one: {line}"
        );
    }

    #[test]
    fn an_auto_run_that_got_cuda_reads_differently_from_one_that_got_cpu() {
        // The control for the test above. Without it that assertion passes on a formatter that
        // hardcoded either provider name, which is the "green on a build where the control does
        // nothing" shape.
        let got_cuda = embedder_announcement(
            ProviderChoice::Auto,
            &plan(EmbedProvider::Cuda, 6, "CUDA, 6 of 8 sessions; device memory"),
        );
        let got_cpu = embedder_announcement(
            ProviderChoice::Auto,
            &plan(EmbedProvider::Cpu, 8, "no readable NVIDIA device"),
        );
        assert_ne!(got_cuda, got_cpu);
        assert!(got_cuda.contains("CUDAExecutionProvider"), "{got_cuda}");
        assert!(got_cuda.contains("6 of 8"), "the width is a fact about the card: {got_cuda}");
        assert!(got_cpu.contains("CPUExecutionProvider"), "{got_cpu}");
    }

    #[test]
    fn an_explicit_cpu_run_is_distinguishable_from_a_fallback_to_cpu() {
        // The property that motivated putting the request on the line at all. Both of these
        // resolve to CPU; before ADR-044 they printed the same words.
        let asked_cpu = embedder_announcement(
            ProviderChoice::Cpu,
            &plan(EmbedProvider::Cpu, 8, "CPU was asked for explicitly"),
        );
        let fell_back = embedder_announcement(
            ProviderChoice::Auto,
            &plan(EmbedProvider::Cpu, 8, "no readable NVIDIA device"),
        );
        assert_ne!(
            asked_cpu, fell_back,
            "a configured CPU run and a silent fallback must not print the same line"
        );
        assert!(asked_cpu.contains("asked for cpu"), "{asked_cpu}");
        assert!(fell_back.contains("asked for auto"), "{fell_back}");
    }
}

#[cfg(test)]
mod rerank_provider_flag {
    use super::*;
    use marlowe_memory::rerank::{RerankChoice, RerankProvider, RerankProviderPlan};

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    fn plan(provider: RerankProvider, requested: RerankChoice, reason: &str) -> RerankProviderPlan {
        RerankProviderPlan {
            provider,
            requested,
            free_at_load: Some(6_000 * 1024 * 1024),
            session_cost: None,
            reason: reason.to_string(),
        }
    }

    #[test]
    fn the_default_is_auto() {
        // **ADR-045.** This is the assertion that fails if the flip is reverted, and it is the
        // whole reason `rerank_provider_choice` exists as a function rather than a match in `main`.
        let a = args(&["--eval-adapter", "--profile-root", "p"]);
        assert_eq!(
            rerank_provider_choice(&a).expect("an absent flag is not an error"),
            RerankChoice::Auto,
            "the rerank default is `auto` (ADR-045). `cpu` here silently un-ships the GPU path on \
             every machine that has one, and it is what made the shipped binary disagree with \
             ADR-029's own headline sentence for nine days"
        );
    }

    #[test]
    fn the_explicit_arms_still_reach_the_loader() {
        // The default must not swallow the flag: `cpu` is what every rerank number published
        // before 2026-08-17 was taken on, and `cuda` is the refusal arm every measurement needs.
        for (value, expected) in [
            ("cpu", RerankChoice::Cpu),
            ("cuda", RerankChoice::Cuda),
            ("auto", RerankChoice::Auto),
        ] {
            let a = args(&["--eval-adapter", "--rerank-provider", value]);
            assert_eq!(
                rerank_provider_choice(&a).expect("a valid value parses"),
                expected,
                "--rerank-provider {value} must reach the loader"
            );
        }
    }

    #[test]
    fn an_unknown_value_is_refused_rather_than_defaulted() {
        // A permissive default here is the family CLAUDE.md names: a typo would run `auto` and
        // report nothing, which is a mismatch made unobservable.
        let a = args(&["--eval-adapter", "--rerank-provider", "gpu"]);
        let message = rerank_provider_choice(&a).expect_err("`gpu` is not a provider");
        assert!(message.contains("auto"), "{message}");
        assert!(message.contains("REFUSAL"), "{message}");
    }

    #[test]
    fn the_announcement_names_what_was_asked_and_what_was_obtained() {
        // The case ADR-045 §5 is about: the request and the resolution DISAGREE, and a line
        // printing only one of them would be read as the other.
        let line = rerank_announcement(
            RerankChoice::Auto,
            &plan(
                RerankProvider::Cpu,
                RerankChoice::Auto,
                "a CUDA session did not construct: cublasLt64_12.dll",
            ),
            false,
        );
        assert!(line.contains("asked for auto"), "the REQUEST must be on the line: {line}");
        assert!(
            line.contains("CPUExecutionProvider"),
            "the RESOLVED provider must be on the line: {line}"
        );
        assert!(
            line.contains("did not construct"),
            "the reason is what makes a fallback legible as one: {line}"
        );
    }

    #[test]
    fn an_auto_run_that_got_cuda_reads_differently_from_one_that_got_cpu() {
        // The control for the test above. Without it that assertion passes on a formatter that
        // hardcoded either provider name -- green on a build where the control does nothing.
        let got_cuda = rerank_announcement(
            RerankChoice::Auto,
            &plan(RerankProvider::Cuda, RerankChoice::Auto, "CUDA, one session opened"),
            true,
        );
        let got_cpu = rerank_announcement(
            RerankChoice::Auto,
            &plan(RerankProvider::Cpu, RerankChoice::Auto, "no readable NVIDIA device"),
            false,
        );
        assert_ne!(got_cuda, got_cpu);
        assert!(got_cuda.contains("CUDAExecutionProvider"), "{got_cuda}");
        assert!(got_cpu.contains("CPUExecutionProvider"), "{got_cpu}");
    }

    #[test]
    fn an_explicit_cpu_run_is_distinguishable_from_a_fallback_to_cpu() {
        // The property that motivated putting the request on the line at all. Both resolve to CPU;
        // before ADR-045 there was no rerank line at all, so they were not merely identical --
        // they were both silent.
        let asked_cpu = rerank_announcement(
            RerankChoice::Cpu,
            &plan(RerankProvider::Cpu, RerankChoice::Cpu, "CPU was asked for explicitly"),
            false,
        );
        let fell_back = rerank_announcement(
            RerankChoice::Auto,
            &plan(RerankProvider::Cpu, RerankChoice::Auto, "no readable NVIDIA device"),
            false,
        );
        assert_ne!(
            asked_cpu, fell_back,
            "a configured CPU run and a silent fallback must not print the same line"
        );
        assert!(asked_cpu.contains("asked for cpu"), "{asked_cpu}");
        assert!(fell_back.contains("asked for auto"), "{fell_back}");
    }

    #[test]
    fn the_batching_follows_the_resolution_and_the_announcement_shows_it() {
        // **The defect `auto` introduces, asserted rather than commented.** `default_batching`
        // measured OPPOSITE on the two providers (ADR-029: CPU sequential 185.8 vs batched 195.6;
        // CUDA batched 3.4 vs sequential 15.2). Under the old `cpu` default the request was the
        // resolution and this could not go wrong. Under `auto` it can: a fallback to CPU that
        // kept CUDA's batching would run the slower of the two CPU shapes, in silence.
        assert!(!RerankProvider::Cpu.default_batching(), "CPU ships sequential");
        assert!(RerankProvider::Cuda.default_batching(), "CUDA ships batched");

        // And the two must be distinguishable on the emitted line, or the mismatch is unobservable
        // even once it exists.
        let cpu_line = rerank_announcement(
            RerankChoice::Auto,
            &plan(RerankProvider::Cpu, RerankChoice::Auto, "fell back"),
            RerankProvider::Cpu.default_batching(),
        );
        let cuda_line = rerank_announcement(
            RerankChoice::Auto,
            &plan(RerankProvider::Cuda, RerankChoice::Auto, "opened"),
            RerankProvider::Cuda.default_batching(),
        );
        assert!(cpu_line.contains("sequential"), "{cpu_line}");
        assert!(cuda_line.contains("batched"), "{cuda_line}");
    }
}
