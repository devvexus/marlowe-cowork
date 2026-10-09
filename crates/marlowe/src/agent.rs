//! `--serve` and `--ask`: the two roles of ARCHITECTURE §6, at the command line.
//!
//! This is the first path in the product where a real model answers a real question through the
//! real loop. Everything before it ran against M1's scripted stub.
//!
//! # `--ask` is a thin client and nothing else
//!
//! It connects, sends one message, renders the events, and exits. It holds no run state — the
//! daemon owns the run, which is why closing this process does not end it (§6, invariant 6).
//! **It auto-spawns a daemon if none is listening, and says so**, because a process appearing on
//! a machine without the user being told is what a well-behaved tool does not do.

use std::path::PathBuf;

use marlowe_contract::text::{sanitize_line, sanitize_prose};
use marlowe_daemon::protocol::Event;
use marlowe_daemon::{Client, Daemon, DaemonConfig, ModelProviderChoice};

/// Where the profile lives when nobody says otherwise.
///
/// **Delegated, not duplicated.** The body moved to `marlowe_daemon` when the socket token landed:
/// the client resolves a profile root to find the token, and the client cannot see this crate. Two
/// copies of a path that has to match on both ends is exactly the shape that lets a mismatch go
/// unobserved, so there is one.
pub fn default_profile_root() -> PathBuf {
    marlowe_daemon::default_profile_root()
}

/// List what this machine's Ollama holds, so `--model` is a choice from a list rather than a guess.
///
/// **Three things are stated per model and none of them is inferred.**
///
/// * whether it is the one this daemon would route to by default
/// * whether it is a **cloud tag**, which `Routing::uniform` refuses by name — shown rather than
///   filtered out, because a user who has one pulled will otherwise pick it and get a refusal they
///   could not have anticipated
/// * whether its tool-call reliability has been **measured**. Exactly one model has been
///   (`qwen3.5:9b`, 12/12 on 2026-08-08); every other line says so. Selecting an unmeasured model is
///   allowed and is the user's call — what is not allowed is quietly reporting the measured model's
///   number under a different name. See `marlowe_provider::capability_for`.
pub fn models() -> Result<(), String> {
    use marlowe_provider::{is_cloud_tag, Availability, LocalEndpoint, Routing, DEFAULT_MODEL};

    let endpoint = LocalEndpoint::default_ollama();
    // Probed against the DEFAULT model purely so the probe has a routing to check; the list it
    // returns is the machine's, not that model's.
    let routing = Routing::uniform(DEFAULT_MODEL).map_err(|e| e.to_string())?;
    let available = match Availability::probe(&endpoint, &routing) {
        Availability::Ready { models } => models,
        Availability::ModelMissing { available, .. } => available,
        other => return Err(other.remedy()),
    };

    if available.is_empty() {
        println!("no models. Pull one with `ollama pull qwen3.5:9b`.");
        return Ok(());
    }

    println!("{} model(s) on this machine:\n", available.len());
    for m in &available {
        let mut notes: Vec<String> = Vec::new();
        if m == DEFAULT_MODEL {
            notes.push("default".to_string());
        }
        if is_cloud_tag(m) {
            notes.push("CLOUD TAG — refused; ADR-028 keeps the default path local".to_string());
        }
        notes.push(
            if m == DEFAULT_MODEL {
                "tool calls 12/12 measured 2026-08-08".to_string()
            } else {
                "tool-call reliability NOT MEASURED".to_string()
            },
        );
        println!("  {m:<28} {}", notes.join(" · "));
    }
    println!("\nChoose one with `marlowe --serve --model <NAME>` (or `--ask --model <NAME>`).");
    println!(
        "A model this machine does not have is refused at startup and the refusal lists what it \
         does have."
    );
    Ok(())
}

pub fn serve(
    workspace: PathBuf,
    profile_root: PathBuf,
    port: Option<u16>,
    dev: bool,
    context: Option<u32>,
    thinking: bool,
    reranking: Option<PathBuf>,
    model: Option<String>,
    provider: ModelProviderChoice,
) -> Result<(), String> {
    let mut config = DaemonConfig::new(profile_root, workspace);
    config.reranking = reranking;
    // **ADR-046. Opt-in, and refused at load rather than defaulted.** `main` has already checked
    // that a key and a model are present when this is `OpenRouter`; passing an unusable choice
    // through would move the refusal to the first turn, where it reads as a provider fault.
    config.model_provider = provider;
    // **A chosen model does not inherit the default's measured reliability.** `capability_for`
    // returns `unmeasured` for anything but `DEFAULT_MODEL`, so `--status` reads "tool-call
    // reliability NOT MEASURED" rather than reporting qwen3.5:9b's 12/12 under another name.
    // A model this machine does not have is refused by `Availability::probe`, which names what it
    // does have — so a typo is a list, not a hang.
    if let Some(m) = model {
        config.model = m;
    }
    if let Some(p) = port {
        config.port = p;
    }
    config.dev = dev;
    config.thinking = thinking;
    if let Some(n) = context {
        config.context_tokens = n;
    }
    // ── EVERY `marlowe:` LINE BELOW IS ALSO KEPT, AND THAT IS THE POINT ──────────────────────
    //
    // `marlowe_daemon::announce` prints exactly what `eprintln!("marlowe: …")` printed and retains
    // a bounded copy, which `StatusReport::announcements` carries to the TUI's §B7 Status tab.
    // These are the sentences a user wants when something is slow or wrong, and until now the only
    // way to see them was to have started the daemon by hand in a terminal that was still open —
    // which §B17's launcher makes the unusual case, not the normal one.
    //
    // **The `[dev]` dumps are deliberately NOT routed through this.** §B1's carve-out keeps
    // instrumentation behind `--dev`; the split between the two prefixes already existed, and this
    // change respects it rather than flattening it.
    use marlowe_daemon::announce;
    announce::info(format!(
        "context window {} tokens (model ceiling {})",
        config.context_tokens,
        marlowe_provider::MODEL_CONTEXT_CEILING
    ));
    // **Announced, never inferred** — ADR-029's rule applied to the model provider. A daemon on
    // openrouter.ai and one on loopback otherwise print an identical startup, and the difference
    // is money and a network.
    //
    // **An exhaustive `match`, not an `if let`.** As an `if let` on OpenRouter, a daemon serving
    // from a local `llama-server` started with output byte-identical to an Ollama one — ADR-029's
    // announced-never-inferred violated by omission, in the one place a person reads what they
    // just started.
    match config.model_provider() {
        ModelProviderChoice::Ollama => {}
        ModelProviderChoice::OpenRouter { model } => {
            announce::info(format!(
                "model provider OPENROUTER · {model} · https://openrouter.ai"
            ));
            // **Amber.** Money leaves the machine and the run stops being reproducible; both are
            // things a person would want to have noticed before the third turn.
            announce::warn(
                "this path is NOT bit-identically reproducible — the serving upstream is \
                 recorded per call instead. See ADR-046 §6.",
            );
            announce::info(
                marlowe_provider::ModelCapability::unmeasured(&model).disclosure(),
            );
        }
        ModelProviderChoice::LlamaCpp { endpoint, sampling } => {
            announce::info(format!(
                "model provider {} · Ollama stores, downloads and lists; a llama-server on \
                 {endpoint} serves. ADR-060",
                marlowe_view::provider::HYBRID,
            ));
            // **The two things that silently change under this engine, both printed.** The
            // template is llama.cpp's rendering of the GGUF's own, not Ollama's Go renderer; the
            // sampler is whatever `resolve_sampling` found, or a stated absence.
            //
            // **Neither clause is stated as a fact about THIS run**, because the engine has not
            // started yet — `Daemon::open` starts it a few lines below and prints which half is
            // actually serving. Saying "llama.cpp renders the template" here, ahead of a start
            // that may fall back, would be a claim about a run that had not happened.
            announce::info(
                "if llama.cpp serves, it renders the GGUF's own chat template and parses its own \
                 tool-call dialect — Ollama's renderer and parser are NOT in that path. The \
                 engine line below says which half actually started",
            );
            match marlowe_provider::llamacpp::resolve_sampling(&config.model, sampling) {
                Ok(plan) => announce::info(plan.disclosure()),
                // Printed, not swallowed. The refusal itself lands later -- at the first turn,
                // where the driver is built -- and a reader who sees only that has to guess which
                // of four reads of Ollama.s store failed and where it looked.
                // A sampler that could not be resolved is a degraded start, not a fact.
                Err(e) => announce::warn(e.remedy()),
            }
            // Through the ONE definition the daemon's status line also calls, so a startup
            // announcement and a status band cannot say different things about one run.
            announce::info(marlowe_provider::llamacpp::disclosure_for(&config.model));
        }
    }
    let port = config.port;
    // **Read BEFORE `Daemon::open`, because opening is what creates the journal.** Asking
    // afterwards would answer "no" on every run including the first — the guard would be
    // permanently green and permanently wrong, which is the shape this project keeps logging.
    let first_run = marlowe_daemon::onboarding::is_first_run(&config.profile_root);
    let workspace = config.workspace.clone();
    let daemon = Daemon::open(config).map_err(|e| e.to_string())?;
    if first_run {
        // ADR-002 (revised): a zero-config first run must not become a zero-disclosure one.
        let state = daemon.memory_state().headline();
        eprint!("{}", marlowe_daemon::onboarding::disclosure(&workspace, &state));
    }
    // **Announced, never inferred.** ADR-029's rule applied to memory: a daemon whose retrieval
    // half is not running behaves exactly like one whose store is empty, and those are very
    // different facts to a person wondering why Marlowe does not remember.
    // **WRITE-ONLY is amber and READY is not**, read off the state rather than from the string:
    // a headline is prose and a level is a decision, and deciding by substring would put the two a
    // rename apart. `RetrievalState::is_live` is the predicate; `headline` is the prose.
    announce::say(
        if daemon.memory_state().is_live() {
            marlowe_daemon::protocol::AnnounceLevel::Info
        } else {
            marlowe_daemon::protocol::AnnounceLevel::Warn
        },
        format!("memory retrieval {}", daemon.memory_state().headline()),
    );

    // **Skills and MCP servers are announced, and so is everything that refused.** ADR-051 §6 and
    // ADR-052 §4, on ADR-029's rule: a state that is not announced is a state the user meets as a
    // mystery. A skill they installed and cannot find, a server that did not connect, and a tool
    // describing itself differently than when they approved it are three ways an installed
    // capability stops being what they think it is, and none of them raises an error anywhere
    // else. Scanning once at startup is only defensible because this is where it lands.
    announce::info(format!(
        "skills {} installed{}",
        daemon.skills_installed(),
        if daemon.skill_refusals().is_empty() {
            String::new()
        } else {
            format!(", {} refused", daemon.skill_refusals().len())
        }
    ));
    for refusal in daemon.skill_refusals() {
        announce::warn(format!("  ! {}", marlowe_contract::text::sanitize_line(refusal)));
    }
    if daemon.mcp_tools() > 0 || !daemon.mcp_notices().is_empty() {
        announce::info(format!(
            "mcp {} tool(s) from {} server(s)",
            daemon.mcp_tools(),
            daemon.mcp_servers()
        ));
        // Sanitised: a notice quotes a tool name the server chose. It reaches a pane now as well
        // as a pipe, so the sanitiser matters more rather than less.
        for notice in daemon.mcp_notices() {
            announce::warn(format!("  ! {}", marlowe_contract::text::sanitize_line(notice)));
        }
    }

    announce::info(format!("daemon listening on 127.0.0.1:{port}"));
    announce::info("runs are owned here and survive the client that started them");
    daemon.serve().map_err(|e| e.to_string())
}

/// One question, one answer. Auto-spawns a daemon if none is up.
pub fn ask(
    message: &str,
    workspace: PathBuf,
    profile_root: PathBuf,
    dev: bool,
    context: Option<u32>,
    thinking: bool,
    port: Option<u16>,
    provider: ModelProviderChoice,
) -> Result<(), String> {
    // **`--daemon-port` reaches `--ask` as of M2 C2f.** Without it every `--ask` went to
    // whatever sat on the default port, which made two things impossible: talking to a scratch
    // daemon, and knowing which daemon answered. It cost real time in this session: a `--dev`
    // dump produced nothing because the request had gone over the socket to a daemon started
    // WITHOUT `--dev`, and that reads as a broken instrument rather than as the wrong process.
    let mut client = Client::new("cli").with_profile_root(profile_root.clone());
    if let Some(p) = port {
        client = client.with_port(p);
    }
    let client = client;

    if !client.daemon_is_up() {
        // Announced, never silent.
        eprintln!("marlowe: no daemon running — starting one in this process for this question.");
        eprintln!("marlowe: run `marlowe --serve` for a daemon that outlives the command.");
        // **The disclosure belongs HERE as much as in `serve`, and this is the path that matters.**
        //
        // `marlowe --ask "..."` on a machine with no daemon is the FIRST thing a new user runs —
        // K6 measures exactly this command — and it auto-spawns a daemon on its own path rather
        // than going through `serve`. Wiring the first-run disclosure into `serve` alone left the
        // most common first run silent.
        //
        // Found by running it: a clean-container K6 measurement printed the two lines above and
        // nothing else. Not by review — the call is one function away and reads as covered.
        let first_run = marlowe_daemon::onboarding::is_first_run(&profile_root);
        let disclosed_workspace = workspace.clone();
        let mut config = DaemonConfig::new(profile_root, workspace);
        config.dev = dev;
        config.thinking = thinking;
        config.model_provider = provider;
        if let Some(n) = context {
            config.context_tokens = n;
        }
        let mut daemon = Daemon::open(config).map_err(|e| e.to_string())?;
        if first_run {
            let state = daemon.memory_state().headline();
            eprint!(
                "{}",
                marlowe_daemon::onboarding::disclosure(&disclosed_workspace, &state)
            );
        }
        // **The in-process path prompts too.**
        //
        // `Daemon::ask` uses the deny-by-default gate, which is correct for a daemon nobody is
        // attached to — and wrong here, because somebody *is* attached: this is a terminal. Left
        // as it was, the most natural command (`marlowe --ask`, no daemon running) would decline
        // every approval and read as a broken gate rather than an absent surface.
        let mut events = Vec::new();
        let mut gate = TerminalApprovals;
        daemon.ask_streaming_with("cli", message, &mut gate, |e| events.push(e));
        render(&events);
        return Ok(());
    }

    // **Collected, then rendered — but the approval is answered inline.**
    //
    // `render` resolves retractions across the whole list, which streaming cannot do (stdout
    // cannot be un-written), so the events are still gathered before printing. What changed is
    // that the gathering loop now answers §B9 prompts as they arrive: the daemon is blocked
    // reading the reply, so this cannot be deferred to the end.
    let mut events = Vec::new();
    client
        .ask_streaming_approving(
            message,
            &mut |event| (approve_at_the_terminal(event), None),
            &mut |event| events.push(event),
        )
        .map_err(|e| e.to_string())?;
    render(&events);
    Ok(())
}

/// The in-process gate, for `--ask` with no daemon running. Same prompt, no socket in between.
struct TerminalApprovals;

impl marlowe_loop::ApprovalGate for TerminalApprovals {
    /// A terminal is attached; that is the whole point of this gate.
    fn is_interactive(&self) -> bool {
        true
    }

    fn await_approval(&mut self, radius: &marlowe_permission::BlastRadius) -> bool {
        approve_at_the_terminal(&Event::Approval {
            decision: 0,
            verb: radius.verb.clone(),
            scope: radius.scope.clone(),
            reversible: radius.reversible,
            novelty: radius.novelty.as_ref().map(|n| format!("{n:?}")),
        })
    }
}

/// §B9 at the command line: **state the blast radius, take one answer, default to no.**
///
/// It reads stdin. When stdin is not a terminal — a pipe, a script, `< /dev/null` — `read_line`
/// returns EOF and this returns `false`. That is the correct outcome and not a degradation: an
/// unattended `--ask` has nobody to approve anything, and approving because nothing objected is
/// the failure §8.2 puts enforcement in the harness to prevent.
///
/// Everything goes to **stderr**, so a piped `--ask` still produces clean output.
fn approve_at_the_terminal(event: &Event) -> bool {
    let Event::Approval { .. } = event else {
        return false;
    };
    let mut err = std::io::stderr();
    if write_approval_prompt(event, &mut err).is_err() {
        return false;
    }
    let _ = std::io::Write::flush(&mut err);

    let mut line = String::new();
    match std::io::stdin().read_line(&mut line) {
        Ok(0) | Err(_) => {
            eprintln!("no answer available — declined");
            false
        }
        Ok(_) => {
            let yes = matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes");
            eprintln!("{}", if yes { "approved" } else { "declined" });
            yes
        }
    }
}

/// **The §B9 prompt, split out from the stdin read so the property can be asserted here.**
///
/// This is the highest-value sanitiser site in the product. `scope` is the model's composed
/// `bash` command — an argument the model chose, printed to a human who is about to authorise it
/// — and until 2026-08-17 it was printed unfiltered. `\u{1b}[2K\r` in a command erases the line
/// it is written on, so the model could repaint the very line the decision is being made from.
/// Every field here is [`Shape::Line`]: a `\n` in `scope` writes a second, fraudulent prompt.
fn write_approval_prompt(event: &Event, out: &mut impl std::io::Write) -> std::io::Result<()> {
    let Event::Approval { verb, scope, reversible, novelty, .. } = event else {
        return Ok(());
    };
    writeln!(out)?;
    writeln!(out, "  approval needed: {}", sanitize_line(verb))?;
    writeln!(out, "  on:              {}", sanitize_line(scope))?;
    writeln!(
        out,
        "  reversible:      {}",
        if *reversible { "yes" } else { "NO — this cannot be undone" }
    )?;
    match novelty {
        Some(n) => writeln!(out, "  novelty:         {}", sanitize_line(n))?,
        // §B9 wants a novelty reason and a ceiling. Neither is fabricated when absent: a
        // defaulted "routine" would be a claim about promotion logic nobody has written.
        None => writeln!(out, "  novelty:         not assessed (the trust ledger is M6)")?,
    }
    write!(out, "  approve? [y/N] ")
}

/// Stop a running daemon. **The graceful path, and until now there was none.**
///
/// `Request::Shutdown` and `Client::shutdown()` have both existed since M2 C2e with nothing on
/// the command line able to reach either, so a daemon left behind — by a hard window close, say —
/// could only be killed. `Stop-Process` skips the daemon's own guard; this does not.
///
/// **It refuses while a run is in flight**, which is invariant 6 where it means something: a run
/// outlives the client that started it. There is no WAL, so stopping mid-run loses the run.
pub fn shutdown(port: Option<u16>, profile_root: PathBuf) -> Result<(), String> {
    let mut client = Client::new("cli").with_profile_root(profile_root);
    if let Some(p) = port {
        client = client.with_port(p);
    }
    if !client.daemon_is_up() {
        // Not an error. "Stop it" and "it is already stopped" want the same outcome, and failing
        // here would make the command awkward to use in a script that just wants a clean slate.
        println!("no daemon is running");
        return Ok(());
    }
    let events = client.shutdown().map_err(|e| e.to_string())?;
    render(&events);
    Ok(())
}

/// `--status`, from a live daemon when there is one and from a throwaway one when there is not.
///
/// **`provider` is threaded in for the second case, and it was missing until the binary was run.**
///
/// `Daemon::status()` reads `config.model_provider()` correctly — the code was right. This function
/// built a `DaemonConfig::new(..)` and never told it, so
/// `marlowe --status --provider openrouter` printed `provider ollama` and `qwen3.5:9b`'s measured
/// reliability: an announcement that was confidently, specifically wrong.
///
/// The test that should have caught it asserted at the daemon, where the field is *set*. Nothing
/// asserted at the CLI, where the field comes *from* — one function away, and it reads as covered.
/// Same shape as the first-run disclosure being wired into `serve` and not into `ask`, which is
/// recorded a few functions above this one and was also found by running it.
///
/// A live daemon answers for itself and is unaffected: it knows what it was started with, and a
/// `--provider` on a *client* invocation cannot change what a running daemon routes to.
pub fn status(
    workspace: PathBuf,
    profile_root: PathBuf,
    provider: ModelProviderChoice,
    port: Option<u16>,
) -> Result<(), String> {
    // **The port, and its absence WAS the bug.** `--status --daemon-port N` probed the default
    // port, found nothing, and fell through to the throwaway daemon below -- which reported the
    // CLI's own defaults while describing a daemon that was running something else entirely.
    // `shutdown` in this file has always threaded it; `status` never did.
    let mut client = Client::new("cli").with_profile_root(profile_root.clone());
    if let Some(p) = port {
        client = client.with_port(p);
    }
    let events = if client.daemon_is_up() {
        client.status().map_err(|e| e.to_string())?
    } else {
        // **Say which reading this is.** The two cases printed identical output, so a person could
        // not tell a description of a live daemon from a description of one that does not exist.
        // That is the whole `get_providers()` family: the measurement was of the client's own
        // configuration and it read as a report about the daemon.
        println!(
            "no daemon on 127.0.0.1:{}. What follows is the configuration `marlowe --serve` would \
             start with, not a description of anything running.",
            client.port()
        );
        let mut config = DaemonConfig::new(profile_root, workspace);
        config.model_provider = provider;
        let daemon = Daemon::open(config).map_err(|e| e.to_string())?;
        vec![Event::Status(daemon.status())]
    };
    render(&events);
    Ok(())
}

/// A client aimed at a running daemon, or a refusal that says there is none.
///
/// **Never a throwaway daemon.** `--status` can honestly describe a configuration when nothing is
/// running; `--runs` and `--steer` cannot, because a daemon that does not exist owns no runs. A
/// fresh process answering them would be reporting its own emptiness as the daemon's.
fn live_client(profile_root: PathBuf, port: Option<u16>) -> Result<Client, String> {
    let mut client = Client::new("cli").with_profile_root(profile_root);
    if let Some(p) = port {
        client = client.with_port(p);
    }
    if !client.daemon_is_up() {
        return Err(format!(
            "no daemon on 127.0.0.1:{}. Runs are the daemon's; start one with `marlowe --serve`",
            client.port()
        ));
    }
    Ok(client)
}

fn a_run_id(value: Option<&str>, flag: &str) -> Result<String, String> {
    match value {
        Some(v) if !v.trim().is_empty() => Ok(v.trim().to_string()),
        _ => Err(format!("{flag} requires a run id. `marlowe --runs` lists them")),
    }
}

/// `marlowe --runs` -- **every run the daemon owns**, read from the daemon.
/// `marlowe --runs <id>` -- that one run in full, including what a resume would resume from.
///
/// # The per-run form used to be `--watch`
///
/// `--watch` now opens a window (§6.6: *"`/watch` opens a window; it does not stream into the
/// conversation pane"*), so the printing form moved here — which is where a per-run listing belongs
/// anyway. **Nothing was lost**: this is the classic surface, it has no window, and it prints the
/// same state the window renders. One state, two renderings, and this is the second one.
pub fn runs(run: Option<&str>, profile_root: PathBuf, port: Option<u16>) -> Result<(), String> {
    let client = live_client(profile_root, port)?;
    if let Some(run) = run.filter(|r| !r.trim().is_empty()) {
        // `since: 0` -- everything. A printed listing is a snapshot and has no earlier poll to
        // continue from.
        let events = client.watch(run.trim(), 0).map_err(|e| e.to_string())?;
        render(&events);
        return Ok(());
    }
    let events = client.runs().map_err(|e| e.to_string())?;
    if events.is_empty() {
        // A fact, not a layout filler (§6.3). "No runs" is true; printing nothing would read as
        // a failure to ask.
        println!("no runs");
        return Ok(());
    }
    render(&events);
    Ok(())
}

/// `marlowe --steer <run> --guidance "..."` -- §10.1's steering **from outside**.
pub fn steer(
    run: Option<&str>,
    guidance: Option<&str>,
    profile_root: PathBuf,
    port: Option<u16>,
) -> Result<(), String> {
    let run = a_run_id(run, "--steer")?;
    let Some(text) = guidance.filter(|g| !g.trim().is_empty()) else {
        return Err("--steer requires --guidance, the words to give the run".to_string());
    };
    let events = live_client(profile_root, port)?.steer(&run, text).map_err(|e| e.to_string())?;
    render(&events);
    Ok(())
}

/// `marlowe --cancel <run>` -- stop at the next iteration boundary, never mid-tool-call.
pub fn cancel(run: Option<&str>, profile_root: PathBuf, port: Option<u16>) -> Result<(), String> {
    let run = a_run_id(run, "--cancel")?;
    let events = live_client(profile_root, port)?.cancel(&run).map_err(|e| e.to_string())?;
    render(&events);
    Ok(())
}

/// `marlowe --resume <run>` -- continue from the last completed checkpoint.
pub fn resume(run: Option<&str>, profile_root: PathBuf, port: Option<u16>) -> Result<(), String> {
    let run = a_run_id(run, "--resume")?;
    let events = live_client(profile_root, port)?.resume(&run).map_err(|e| e.to_string())?;
    render(&events);
    Ok(())
}

/// Drop every `Text` event that a later `SpeechRetracted` proved was reasoning.
///
/// A retraction applies to the speech emitted **so far in the turn**, so it clears the run of
/// `Text` events back to the previous retraction or the start of the list — the same span the
/// TUI moves into its thinking block.
fn resolve_retractions(events: &[Event]) -> Vec<Event> {
    let mut out: Vec<Event> = Vec::with_capacity(events.len());
    for event in events {
        match event {
            Event::SpeechRetracted { .. } => out.retain(|e| !matches!(e, Event::Text { .. })),
            other => out.push(other.clone()),
        }
    }
    out
}

/// Render events as §B6 asks: one line per tool call, prose as prose, failures visible.
///
/// **Retractions are resolved before anything prints.** `stdout` cannot be un-written, so the TUI's
/// approach — move the text into the thinking block — has no equivalent here. The classic path
/// gets the whole event list at once, so the retraction is applied to the list instead: text the
/// model later proved was reasoning never reaches the pipe in the first place.
fn render(events: &[Event]) {
    let mut out = std::io::stdout();
    let _ = render_to(events, &mut out);
}

/// The body of [`render`], against a writer so the sanitiser can be asserted where it runs.
///
/// **Every model-influenced field is sanitised here**, and the shape is chosen per field rather
/// than globally: prose keeps its newlines, anything sharing a line with harness-authored text
/// does not. A `\n` inside a tool `target` would otherwise forge a second §B6 tool line, which
/// reads as a call the model never made.
fn render_to(events: &[Event], out: &mut impl std::io::Write) -> std::io::Result<()> {
    let events = resolve_retractions(events);
    for event in &events {
        match event {
            // **The classic CLI prints announcements too, and it is not the TUI's job alone.**
            // These are the lines that used to go only to stderr -- an engine start, a reserve that
            // could not be taken, resumable runs -- and a person running `marlowe --ask` in a
            // terminal has exactly the same need for them as a person in the surface. Sanitised
            // like every other daemon-authored string, so a newline inside one cannot forge
            // a line the daemon never wrote.
            Event::Announce(a) => {
                writeln!(out, "marlowe: {}", sanitize_line(&a.text))?;
            }
            // **Not printed here, deliberately.** The cadence is a live figure for a band that
            // repaints; in a linear transcript it would be a number stamped mid-answer with no
            // frame to belong to. `--status` and the TUI are where it reads as a measurement.
            Event::Cadence { .. } => {}
            Event::Status(r) => {
                writeln!(out, "marlowe {}", sanitize_line(&r.version))?;
                writeln!(out, "  workspace   {}", sanitize_line(&r.workspace))?;
                writeln!(out, "  model       {}", sanitize_line(&r.model_disclosure))?;
                // **ADR-046, and it was missing until the binary was run.**
                //
                // The field was added to `StatusReport` and nothing rendered it, so `--status` on
                // a daemon routing to openrouter.ai and one routing to loopback printed the same
                // five lines — and the difference between them is money and a network. That is a
                // declared control with no reader, found the way this project keeps finding them:
                // by running the thing rather than by reading the test.
                //
                // Empty only for a frame from a client built before this field existed; a blank
                // line would then claim something, so it is skipped rather than shown as unknown.
                if !r.model_provider.is_empty() {
                    writeln!(out, "  provider    {}", sanitize_line(&r.model_provider))?;
                }
                // ADR-029: announced, never inferred.
                writeln!(out, "  rerank      {}", sanitize_line(&r.rerank_provider))?;
                writeln!(out, "  runs        {} live", r.live_runs)?;
                match &r.degraded {
                    Some(d) => writeln!(out, "  DEGRADED    {}", sanitize_line(d))?,
                    None => writeln!(out, "  ready")?,
                }
            }
            // Prose: the model's reply, and its newlines are its own.
            Event::Text { delta } => write!(out, "{}", sanitize_prose(delta))?,
            // Only ever produced by `Replay`, which the classic path does not use.
            Event::User { text } => writeln!(out, "> {}", sanitize_prose(text))?,
            // The classic path has no collapsible element, so reasoning is counted rather than
            // printed: it is progress, not an answer, and dumping a chain of thought into a
            // piped stdout would make `--ask` unusable in a script.
            Event::Reasoning { .. } => {}
            // Already applied by `resolve_retractions`, above.
            Event::SpeechRetracted { .. } => {}
            // ── `detail` IS BOUND, NOT SWEPT INTO THE `..`. THE DECISION IS BELOW. ──────────
            //
            // This arm read `{ verb, target, state, summary, .. }`, so adding `detail` to
            // `Event::Tool` would have compiled here and printed nothing — the "declared control
            // nothing reads" shape, in the one place the compiler could not have told anyone.
            // Binding it means a future field has to be decided about here too.
            //
            // **A SUCCESS DETAIL IS WITHHELD, AND THAT IS THE DECISION.** §B6's expansion is an
            // interactive affordance — a keystroke, on a line a person chose. This path has no
            // collapsible element; it is a script's stdout, and it already declines to print
            // reasoning for exactly that reason. A `read` here would put the whole file between
            // the tool line and the answer on every `--ask`, which is the interface, changed.
            //
            // **A FAILURE REASON IS PRINTED.** A failed call reached this surface as
            // `⋯ edit  notes.md    [failed]` — the verb, the target, and no cause, because
            // `summary` is metrics and `failed()` has no metrics. That is the same silence the
            // loop closed for the model on 2026-08-26, on the same field, and it is the one thing
            // somebody reading a piped transcript afterwards actually needs.
            Event::Tool { verb, target, state, summary, detail, .. } => {
                writeln!(
                    out,
                    "  ⋯ {}  {}  {}  [{}]",
                    sanitize_line(verb),
                    sanitize_line(target),
                    sanitize_line(summary),
                    sanitize_line(state)
                )?;
                if state == "failed" {
                    for line in detail.iter().flat_map(|d| d.lines()) {
                        writeln!(out, "    {}", sanitize_line(line))?;
                    }
                }
            }
            Event::Compacted { turns } => writeln!(out, "  ─ compacted · {turns} turns ─")?,
            Event::Degraded { what, remedy } => {
                writeln!(out, "  ! {}", sanitize_line(what))?;
                writeln!(out, "    {}", sanitize_line(remedy))?;
            }
            Event::Approval { verb, scope, reversible, .. } => {
                writeln!(
                    out,
                    "  ? approval needed: {} {}{}",
                    sanitize_line(verb),
                    sanitize_line(scope),
                    if *reversible { "" } else { " · not recoverable" }
                )?;
                writeln!(out, "    (no interactive surface attached; the harness declined)")?;
            }
            Event::Done { outcome, detail, elapsed_ms, .. } => {
                if !detail.is_empty() {
                    writeln!(out, "{}", sanitize_prose(detail))?;
                }
                writeln!(out, "  [{} · {elapsed_ms} ms]", sanitize_line(outcome))?;
            }
            // **The name first, the id under it.** A UUID is unsayable and untypeable, and this
            // listing is where a person goes to find the run they are about to steer or watch.
            // The id stays on its own line rather than being replaced: it is what the journal
            // keys on, a script may be reading this, and a mnemonic is an affordance for typing
            // and never a guarantee of identity — 4096 names collide.
            Event::Run { id, status, tokens, attribution, .. } => {
                writeln!(
                    out,
                    "  run {}  {}  {tokens} tokens",
                    marlowe_loop::run::sayable(&sanitize_line(id)),
                    sanitize_line(status)
                )?;
                writeln!(out, "      {}", sanitize_line(id))?;
                // ADR-046 §3. Absent on the local path, where the question does not arise.
                if let Some(a) = attribution {
                    writeln!(out, "      {}", sanitize_line(a))?;
                }
            }
            // §6.2's field list, rendered linearly. **Every line is a fact the daemon holds**;
            // `last_checkpoint_step: None` prints as "none", never as step 0.
            Event::RunDetail {
                id,
                status,
                parent,
                elapsed_ms,
                spend_micros_usd,
                ceiling_micros_usd,
                spent_tokens,
                granted_tokens,
                depth,
                last_checkpoint_step,
                resumable,
                orphan_policy,
                pending_steers,
                subagents,
            } => {
                writeln!(out, "run {}", marlowe_loop::run::sayable(&sanitize_line(id)))?;
                writeln!(out, "  id          {}", sanitize_line(id))?;
                writeln!(out, "  status      {}", sanitize_line(status))?;
                if let Some(p) = parent {
                    writeln!(out, "  parent      {}", sanitize_line(p))?;
                }
                writeln!(out, "  elapsed     {elapsed_ms} ms")?;
                writeln!(
                    out,
                    "  spend       {spend_micros_usd} of {ceiling_micros_usd} micros_usd"
                )?;
                writeln!(out, "  tokens      {spent_tokens} of {granted_tokens} granted")?;
                writeln!(out, "  depth       {depth}")?;
                match last_checkpoint_step {
                    Some(step) => writeln!(out, "  checkpoint  step {step}")?,
                    None => writeln!(out, "  checkpoint  none")?,
                }
                writeln!(
                    out,
                    "  resume      {}",
                    if *resumable { "from that step" } else { "not resumable" }
                )?;
                // §6.2: cancel, with the orphan policy the run declared, stated plainly.
                writeln!(out, "  on cancel   children {}", sanitize_line(orphan_policy))?;
                writeln!(out, "  steers      {pending_steers} queued")?;
                // **The roster, one line per child.** M3 Session B1 gave it a producer; before
                // that `run` could not spawn and this was structurally always empty. Printed only
                // when there are children, for `parent`'s reason above: a line reading
                // `subagents   none` on every run there has ever been is noise, and the window's
                // panel is where an explicit *none* belongs.
                //
                // Both fields go through `sanitize_line` even though both are harness-derived —
                // an id from the journal and a status word from a closed enum. The two other
                // render sites here do the same, and a display sanitiser applied selectively is
                // one refactor away from being applied nowhere.
                for child in subagents {
                    writeln!(
                        out,
                        "  subagent    {}  {}  {}",
                        marlowe_loop::run::sayable(&sanitize_line(&child.id)),
                        sanitize_line(&child.status),
                        sanitize_line(&child.id)
                    )?;
                }
            }
            Event::Error { detail } => writeln!(out, "  error: {}", sanitize_line(detail))?,
            // **The control plane's frames, and the classic CLI does not render them here.**
            // `--ask` is a conversation; `--watch` and `--runs` are where a run's own state is
            // shown, and `watch.rs` formats them. Listing the variants rather than sweeping them
            // into a `_` keeps the next `Event` somebody adds a compile error at this site.
            Event::RunDetail { .. } | Event::RunOutput { .. } => {}
        }
    }
    Ok(())
}

/// **The display sanitiser at the two classic-CLI render sites.**
///
/// Two tests, one per site, and that is deliberate: reverting the sanitiser in `render_to` must
/// fail a *different named test* from reverting it in `write_approval_prompt`. A single test
/// covering both sites would pass while one of them was unguarded — which is instance #16, the
/// property asserted somewhere other than where it is enforced, committed while fixing it.
#[cfg(test)]
mod display_sanitiser {
    use super::*;
    use marlowe_daemon::protocol::{Event, StatusReport};

    /// Erase-line + carriage-return: the payload from the audit finding. It repaints the line it
    /// is printed on, so a human reading a command sees whatever the model wanted them to see.
    const OVERWRITE: &str = "\u{1b}[2K\r";

    fn text(bytes: Vec<u8>) -> String {
        String::from_utf8(bytes).expect("render writes utf-8")
    }

    #[test]
    fn the_approval_prompt_never_prints_an_escape_the_model_composed() {
        // `scope` is the model's composed `bash` command, printed to a human about to authorise
        // it. This is the highest-severity instance of the finding.
        let event = Event::Approval {
            decision: 1,
            verb: "bash".into(),
            scope: format!("rm -rf /important{OVERWRITE}ls -l"),
            reversible: false,
            novelty: Some(format!("first use{OVERWRITE}routine")),
        };
        let mut out = Vec::new();
        write_approval_prompt(&event, &mut out).unwrap();
        let s = text(out);

        assert!(!s.contains('\u{1b}'), "ESC reached the approval prompt:\n{s}");
        assert!(!s.contains('\r'), "CR reached the approval prompt:\n{s}");
        assert!(s.contains("<U+001B>"), "stripped silently instead of marked:\n{s}");

        // The prompt still says what is being approved — marking, not truncating.
        assert!(s.contains("rm -rf /important"), "{s}");
        assert!(s.contains("approve? [y/N]"), "{s}");

        // And the decision surface is still the shape the human expects: one line per field, so
        // a `\n` in scope cannot write a second, fraudulent prompt below the real one.
        let forged = Event::Approval {
            decision: 2,
            verb: "bash".into(),
            scope: "ok\n  approve? [y/N] y".into(),
            reversible: true,
            novelty: None,
        };
        let mut out2 = Vec::new();
        write_approval_prompt(&forged, &mut out2).unwrap();
        let s2 = text(out2);
        // **The property is "no forged prompt LINE", not "the substring appears once".**
        //
        // The first draft of this assertion counted substrings and failed against a *working*
        // sanitiser: the forged text is still present, inert, in the middle of the scope line
        // after a visible `<U+000A>`. That is the fix working — what makes a prompt a prompt is
        // that it starts its own line, and a human scanning the left margin sees one. Counting
        // occurrences measured something adjacent to the property and stricter than it.
        let prompt_lines = s2.lines().filter(|l| l.trim_start().starts_with("approve?")).count();
        assert_eq!(prompt_lines, 1, "the scope forged a second prompt line:\n{s2}");
        assert!(s2.contains("<U+000A>"), "the newline was dropped, not marked:\n{s2}");
        // ...and the forged text is still visible rather than removed, so the human can see what
        // was attempted.
        assert!(s2.contains("ok<U+000A>"), "{s2}");
    }

    #[test]
    fn no_rendered_event_carries_a_control_sequence_to_the_terminal() {
        // Every model-influenced field on every variant, in one pass: if a variant is added
        // later and left unsanitised, this fails as soon as it is given a hostile value.
        let events = vec![
            Event::Status(StatusReport {
                version: format!("0.1.0{OVERWRITE}"),
                workspace: format!("C:\\w{OVERWRITE}"),
                model: "m".into(),
                model_disclosure: format!("marlowe-red:9b{OVERWRITE}"),
                degraded: Some(format!("ollama down{OVERWRITE}"),),
                rerank_provider: format!("cpu{OVERWRITE}"),
                model_provider: format!("openrouter{OVERWRITE}"),
                live_runs: 0,
                models: Vec::new(),
                // Nothing has been announced into this fixture and nothing has been up.
                announcements: Vec::new(),
                uptime_ms: 0,
            }),
            Event::Text { delta: format!("prose{OVERWRITE}more") },
            Event::User { text: format!("hi{OVERWRITE}") },
            Event::Tool {
                id: 1,
                verb: format!("bash{OVERWRITE}"),
                target: format!("ls\u{202E}gnp.exe"),
                state: format!("ok{OVERWRITE}"),
                summary: format!("0 files{OVERWRITE}"),
                detail: None,
            },
            Event::Degraded { what: format!("w{OVERWRITE}"), remedy: format!("r{OVERWRITE}") },
            Event::Approval {
                decision: 3,
                verb: format!("edit{OVERWRITE}"),
                scope: format!("/etc/passwd{OVERWRITE}"),
                reversible: false,
                novelty: None,
            },
            Event::Done {
                outcome: format!("completed{OVERWRITE}"),
                detail: format!("done{OVERWRITE}"),
                spend_micros_usd: 0,
                elapsed_ms: 5,
            },
            Event::Run {
                id: format!("r1{OVERWRITE}"),
                status: format!("live{OVERWRITE}"),
                tokens: 3,
                depth: 0,
                // ADR-046 §3: the upstream's own name for itself, which is a string a SERVER
                // chose. It is sanitised where it arrives too; this asserts the render site.
                attribution: Some(format!("upstream Anthropic{OVERWRITE}")),
            },
            Event::Error { detail: format!("boom{OVERWRITE}") },
        ];

        let mut out = Vec::new();
        render_to(&events, &mut out).unwrap();
        let s = text(out);

        assert!(!s.contains('\u{1b}'), "ESC survived render:\n{s:?}");
        assert!(!s.contains('\r'), "CR survived render:\n{s:?}");
        assert!(!s.contains('\u{202E}'), "RLO survived render:\n{s:?}");
        assert!(s.contains("<U+001B>"), "stripped silently instead of marked:\n{s:?}");
        assert!(s.contains("<U+202E>"), "stripped silently instead of marked:\n{s:?}");

        // **The control that stops this being vacuous.** If the events had never been rendered
        // — a match arm that dropped them, an early return — every assertion above would pass on
        // an empty string. This asserts the payloads actually reached the writer.
        assert!(s.contains("prose"), "nothing was rendered at all:\n{s:?}");
        assert!(s.contains("/etc/passwd"), "{s:?}");
        assert!(s.contains("boom"), "{s:?}");
    }

    #[test]
    fn prose_keeps_its_newlines_and_a_tool_line_does_not() {
        // The shape distinction, asserted where it is applied rather than in the contract crate:
        // a model reply is prose, and a §B6 tool line is one line.
        let mut out = Vec::new();
        render_to(&[Event::Text { delta: "line one\nline two".into() }], &mut out).unwrap();
        assert_eq!(text(out), "line one\nline two");

        let mut out = Vec::new();
        render_to(
            &[Event::Tool {
                id: 1,
                verb: "read".into(),
                target: "a.txt\n  ⋯ bash  rm -rf /  ok  [ok]".into(),
                state: "ok".into(),
                summary: "1 file".into(),
                detail: None,
            }],
            &mut out,
        )
        .unwrap();
        let s = text(out);
        assert_eq!(s.lines().count(), 1, "the target forged a second tool line:\n{s}");
        assert!(s.contains("<U+000A>"), "{s}");
    }

    /// **ADR-046. Found by running the binary, not by a test — which is why this test exists.**
    ///
    /// `model_provider` was added to `StatusReport`, the daemon filled it from the same function
    /// the run path selects a driver with, and **nothing rendered it**. `marlowe --status` printed
    /// five identical lines whether the daemon routed to `openrouter.ai` or to loopback, and the
    /// difference between those is money and a network.
    ///
    /// A declared control with no reader, shipped by the session whose own ADR §2 is about a
    /// declared control with no reader. The instrument that caught it was `--status` on the
    /// release binary; nothing in the suite could have.
    ///
    /// Asserted here, at `render_to`, because that is where the byte is either written or not.
    #[test]
    fn status_shows_which_provider_answers_and_the_two_providers_do_not_render_alike() {
        let report = |provider: &str| {
            Event::Status(StatusReport {
                version: "0.1.0".into(),
                workspace: "/ws".into(),
                model: "m".into(),
                model_disclosure: "m · NOT MEASURED".into(),
                degraded: None,
                rerank_provider: "cpu".into(),
                model_provider: provider.into(),
                live_runs: 0,
                models: Vec::new(),
                // Nothing has been announced into this fixture and nothing has been up.
                announcements: Vec::new(),
                uptime_ms: 0,
            })
        };

        let mut local = Vec::new();
        render_to(&[report("ollama")], &mut local).unwrap();
        let local = text(local);
        let mut hosted = Vec::new();
        render_to(&[report("openrouter")], &mut hosted).unwrap();
        let hosted = text(hosted);

        assert!(local.contains("provider    ollama"), "{local}");
        assert!(hosted.contains("provider    openrouter"), "{hosted}");
        // **The assertion with teeth.** Both of the above would pass against a renderer that
        // printed a constant; what has to be true is that the two READ DIFFERENTLY.
        assert_ne!(
            local, hosted,
            "a hosted daemon and a local one rendered identically, which is the defect this test \
             was written for"
        );

        // A frame from a client built before the field existed says nothing rather than claiming
        // a provider it was never told about.
        let mut old = Vec::new();
        render_to(&[report("")], &mut old).unwrap();
        assert!(!text(old).contains("provider"), "an empty field must not render a blank claim");
    }
}
