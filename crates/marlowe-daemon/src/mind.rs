//! Memory wiring: the agent's `MemoryHook`, the conductor's memory tools, and the composed host.
//! Memory requests, the legacy offer and episodes live in [`crate::mind_ops`].

use std::collections::HashSet;
use std::time::Instant;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use marlowe_agent::{Consequence, Exposure, MemoryHook, Recollection, ToolCallReq, ToolCtx, ToolHost, ToolOutcome};
use marlowe_mind::{
    InjectQuery, Kind, LiveEvent, LiveSink, MemoryChange, MemoryIn, MemoryKind, Mind, ProfileQuery, Provenance, RecallHit, RecallQuery,
    Remembered, Scope, Trust, TurnIn, TurnRole,
};
use marlowe_model::ToolSpec;
use marlowe_proto::{Event, PhaseEvent, PhaseKind, SessionId};
use serde_json::Value;

use crate::{lock, now_ms};

/// Publishes an event on the bus (used for `Recalled` and `MemoryChanged`).
pub type RecallSink = Arc<dyn Fn(Event) + Send + Sync>;

/// Names of the memory tools the conductor sees.
pub const MEMORY_TOOLS: [&str; 4] = ["remember", "recall", "forget", "correct"];

/// Results one `recall` call returns by default, and at most.
const RECALL_DEFAULT: usize = 8;
const RECALL_MAX: usize = 20;

/// Longest `Recalled` summary, in characters.
const RECALLED_SUMMARY_CHARS: usize = 160;

/// The agent's memory for one turn: `before_turn` reads the profile ([`Mind::profile`]), injects
/// via [`Mind::inject`] excluding what the profile already holds, emits `Recalled` and ingests the
/// user's words; `after_turn` ingests the assistant's reply.
pub struct MindMemory {
    mind: Mind,
    workspace: Option<String>,
    turn: u32,
    budget_tokens: u32,
    emit: RecallSink,
    /// Where the Mind's write tail files its events.
    tail: Option<Arc<crate::phases::TailTarget>>,
    /// A crew relay turn: the "user" text is a crew result, not the user's words, so it is not
    /// ingested at user trust (the run's own Result memory carries its provenance).
    relay: bool,
}

impl MindMemory {
    /// Files the Mind's write-tail events under this session and turn.
    fn mind_tail(&self, session: &SessionId) {
        if let Some(t) = &self.tail {
            t.set(session, self.turn);
        }
    }

    fn phase(&self, session: &SessionId, ev: PhaseEvent) {
        (self.emit)(Event::Phase(ev.on(session, self.turn)));
    }

    /// `budget_tokens` (the whole memory tier) is capped at `marlowe_mind::MAX_INJECT_TOKENS`;
    /// the profile takes its share first, at most `marlowe_mind::DEFAULT_PROFILE_TOKENS`.
    pub fn new(mind: Mind, workspace: Option<String>, turn: u32, budget_tokens: u32, emit: RecallSink) -> Self {
        let budget_tokens = budget_tokens.min(marlowe_mind::MAX_INJECT_TOKENS);
        Self { mind, workspace, turn, budget_tokens, emit, tail: None, relay: false }
    }

    /// Files the Mind's write-tail events (embedded, indexed) under the turn that wrote them.
    pub(crate) fn with_tail(mut self, tail: Arc<crate::phases::TailTarget>) -> Self {
        self.tail = Some(tail);
        self
    }

    /// For a relay turn: retrieval and the reply as usual, no user-trust ingest of the relay.
    pub fn relay(mut self) -> Self {
        self.relay = true;
        self
    }

    fn ingest(&self, session: &SessionId, role: TurnRole, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        // The write tail's events (embedded, indexed) are filed under the turn that wrote.
        self.mind_tail(session);
        let turn = TurnIn {
            session: session.to_string(),
            workspace: self.workspace.clone(),
            role,
            text: text.to_string(),
            at_ms: now_ms(),
        };
        if let Err(e) = self.mind.ingest_turn(turn) {
            (self.emit)(Event::Error { session: Some(session.clone()), message: format!("memory: turn not remembered: {e}") });
        }
    }
}

impl MemoryHook for MindMemory {
    fn before_turn(&self, session: &SessionId, query: &str) -> Recollection {
        let started = Instant::now();
        // A failed profile read leaves the turn with retrieval only.
        self.phase(session, PhaseEvent::start(PhaseKind::Recall).stage("profile"));
        let profile_started = Instant::now();
        let profile = self
            .mind
            .profile(ProfileQuery { workspace: self.workspace.clone(), ..ProfileQuery::default() })
            .unwrap_or_default();
        self.phase(
            session,
            PhaseEvent::end(PhaseKind::Recall, profile_started.elapsed().as_millis() as u64).stage("profile"),
        );
        let query_in = InjectQuery {
            session: session.to_string(),
            workspace: self.workspace.clone(),
            text: query.to_string(),
            budget_tokens: self.budget_tokens.saturating_sub(profile.tokens),
            exclude: profile.ids,
        };
        // The Mind streams each retrieval stage as it runs; a thread forwards them to the bus
        // while the actor works. The sink drops when the Mind is done, which ends the thread.
        let (tx, rx) = crossbeam_channel::bounded::<LiveEvent>(256);
        let injection = std::thread::scope(|scope| {
            scope.spawn(|| {
                for ev in rx.iter() {
                    self.phase(session, crate::phases::recall_phase(ev));
                }
            });
            self.mind.inject_live(query_in, LiveSink::new(tx))
        });
        // Ingested here rather than in `after_turn` (which a failed turn skips), and after the
        // inject so a statement is never recalled back to itself.
        if !self.relay {
            self.phase(session, PhaseEvent::start(PhaseKind::Recall).stage("ingest"));
            let ingest_started = Instant::now();
            self.ingest(session, TurnRole::User, query);
            self.phase(
                session,
                PhaseEvent::end(PhaseKind::Recall, ingest_started.elapsed().as_millis() as u64).stage("ingest"),
            );
        }
        (self.emit)(Event::Recalled {
            session: session.clone(),
            turn: self.turn,
            count: injection.recalled.len() as u32,
            summary: recalled_summary(&injection.recalled),
            total_ms: started.elapsed().as_millis() as u64,
            stages: injection.stages.clone(),
            abstained: injection.abstained.is_some(),
        });
        // Untrusted lineage in the context taints the run, as an untrusted `recall` does.
        let untrusted = injection.floor == Some(Trust::Untrusted);
        Recollection { profile: profile.block, blocks: injection.blocks, untrusted }
    }

    fn after_turn(&self, session: &SessionId, _user: &str, assistant: &str) {
        self.ingest(session, TurnRole::Assistant, assistant);
    }
}

/// `kind · text; kind · text…`, short, for the dim recall line.
fn recalled_summary(hits: &[RecallHit]) -> String {
    let joined = hits
        .iter()
        .map(|h| format!("{} · {}", h.kind.label(), one_line(&h.text)))
        .collect::<Vec<_>>()
        .join("; ");
    clip(&joined, RECALLED_SUMMARY_CHARS)
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut t: String = s.chars().take(max).collect();
    t = t.trim_end().to_string();
    t.push('…');
    t
}

/// `remember`, `recall`, `forget`, `correct` as a tool host; composed with the builtin tools by
/// [`ComposedTools`]. Every call is `Consequence::Free` except a `forget` of a memory that
/// outranks the run, which asks the user and then runs with the user's authority.
pub struct MindTools {
    mind: Mind,
    workspace: Option<String>,
    emit: Option<RecallSink>,
    /// Latched from the run's exposure: writes after an untrusted read are untrusted.
    tainted: AtomicBool,
    /// `recall` calls that returned untrusted memories; they taint the conversation.
    untrusted_recalls: Mutex<HashSet<String>>,
    /// `forget` calls that needed approval; the loop runs them only once the user allowed them.
    user_authorised: Mutex<HashSet<String>>,
}

impl MindTools {
    pub fn new(mind: Mind, workspace: Option<String>) -> Self {
        Self {
            mind,
            workspace,
            emit: None,
            tainted: AtomicBool::new(false),
            untrusted_recalls: Mutex::new(HashSet::new()),
            user_authorised: Mutex::new(HashSet::new()),
        }
    }

    /// Broadcasts `MemoryChanged` for writes made through the tools.
    pub fn with_events(mut self, emit: RecallSink) -> Self {
        self.emit = Some(emit);
        self
    }

    /// Records the run's exposure; once tainted, stays tainted for this host.
    pub fn latch_exposure(&self, exposure: &Exposure) {
        if exposure.tainted {
            self.tainted.store(true, Ordering::SeqCst);
        }
    }

    pub fn is_memory_tool(name: &str) -> bool {
        MEMORY_TOOLS.contains(&name)
    }

    /// A `forget` whose target carries more trust than the run (inferred, or untrusted once
    /// tainted) asks the user; if allowed it runs as the user's request. Unknown ids run free
    /// and fail in the call.
    fn forget_consequence(&self, call: &ToolCallReq) -> Consequence {
        lock(&self.user_authorised).remove(&call.id);
        let run = if self.tainted.load(Ordering::SeqCst) { Trust::Untrusted } else { Trust::Inferred };
        let target = str_arg(&call.args, "id").and_then(|id| self.mind.get(id).ok().flatten());
        match target {
            Some(t) if t.trust > run => {
                lock(&self.user_authorised).insert(call.id.clone());
                Consequence::Ask {
                    summary: format!("forget a memory from {}: {}", t.trust.label(), clip(&one_line(&t.text), 60)),
                }
            }
            _ => Consequence::Free,
        }
    }

    fn changed(&self, id: &str, change: MemoryChange) {
        if let Some(emit) = &self.emit {
            emit(Event::MemoryChanged { id: id.to_string(), change });
        }
    }

    fn source(&self, ctx: &ToolCtx) -> Provenance {
        Provenance::Agent {
            session: ctx.session.to_string(),
            turn: ctx.turn,
            tainted: self.tainted.load(Ordering::SeqCst),
        }
    }

    fn remember(&self, args: &Value, ctx: &ToolCtx) -> ToolOutcome {
        let Some(text) = str_arg(args, "text") else { return ToolOutcome::error("`text` is required") };
        let label = str_arg(args, "kind").unwrap_or("fact");
        let kind = match MemoryKind::from_label(label) {
            Some(MemoryKind::Episode) => return ToolOutcome::error("episodes are written by the harness, not by remember"),
            Some(MemoryKind::Decision) => match str_arg(args, "reason") {
                Some(reason) => Kind::Decision { reason: reason.to_string() },
                None => return ToolOutcome::error("a decision needs a `reason`"),
            },
            Some(k) => plain_kind(k),
            None => return ToolOutcome::error(format!("unknown kind {label:?}")),
        };
        let scope = match str_arg(args, "scope").unwrap_or("global") {
            "global" => Scope::Global,
            "project" => match &self.workspace {
                Some(ws) => Scope::Project(ws.clone()),
                None => return ToolOutcome::error("this session has no workspace; use scope \"global\""),
            },
            other => return ToolOutcome::error(format!("unknown scope {other:?} (global, project)")),
        };
        let memory = MemoryIn { text: text.to_string(), kind, scope, source: self.source(ctx) };
        match self.mind.remember(memory) {
            Ok(r) => {
                self.changed(&r.id, MemoryChange::Remembered);
                ToolOutcome::ok(receipt("remembered", &r), format!("remembered · {}", r.trust.label()))
            }
            Err(e) => ToolOutcome::error(format!("not remembered: {e}")),
        }
    }

    fn recall(&self, call: &ToolCallReq) -> ToolOutcome {
        let Some(query) = str_arg(&call.args, "query") else { return ToolOutcome::error("`query` is required") };
        let limit = call.args.get("limit").and_then(Value::as_u64).map(|n| n as usize).unwrap_or(RECALL_DEFAULT);
        let scope = self.workspace.clone().map(Scope::Project);
        let hits = match self.mind.recall(RecallQuery { text: query.to_string(), scope, limit: limit.clamp(1, RECALL_MAX) }) {
            Ok(h) => h,
            Err(e) => return ToolOutcome::error(format!("recall failed: {e}")),
        };
        if hits.iter().any(|h| h.trust == Trust::Untrusted) {
            lock(&self.untrusted_recalls).insert(call.id.clone());
        }
        if hits.is_empty() {
            return ToolOutcome::ok(format!("nothing stored matches {query:?}."), "0 memories");
        }
        let n = hits.len();
        let mut out = format!("{n} {} for {query:?}:\n", plural(n));
        for h in &hits {
            out.push_str(&format_hit(h));
        }
        ToolOutcome::ok(out, format!("{n} {}", plural(n)))
    }

    fn forget(&self, call: &ToolCallReq, ctx: &ToolCtx) -> ToolOutcome {
        let Some(id) = str_arg(&call.args, "id") else { return ToolOutcome::error("`id` is required") };
        // Approved by the user (see `forget_consequence`): the user's authority, not the run's.
        let source = if lock(&self.user_authorised).remove(&call.id) {
            Provenance::User { session: Some(ctx.session.to_string()) }
        } else {
            self.source(ctx)
        };
        match self.mind.forget(id, source) {
            Ok(()) => {
                self.changed(id, MemoryChange::Forgotten);
                ToolOutcome::ok(format!("forgot {id}"), "forgotten")
            }
            Err(e) => ToolOutcome::error(format!("not forgotten: {e}")),
        }
    }

    fn correct(&self, args: &Value, ctx: &ToolCtx) -> ToolOutcome {
        let (Some(id), Some(text)) = (str_arg(args, "id"), str_arg(args, "text")) else {
            return ToolOutcome::error("`id` and `text` are required");
        };
        match self.mind.correct(id, text, self.source(ctx)) {
            Ok(r) => {
                self.changed(id, MemoryChange::Corrected { by: r.id.clone() });
                ToolOutcome::ok(receipt(&format!("corrected {id}; now"), &r), "corrected")
            }
            Err(e) => ToolOutcome::error(format!("not corrected: {e}")),
        }
    }
}

fn plain_kind(k: MemoryKind) -> Kind {
    match k {
        MemoryKind::Fact => Kind::Fact,
        MemoryKind::Preference => Kind::Preference,
        MemoryKind::Result => Kind::Result,
        MemoryKind::Note => Kind::Note,
        MemoryKind::Procedure => Kind::Procedure,
        MemoryKind::Episode => Kind::Episode,
        MemoryKind::Decision => Kind::Decision { reason: String::new() },
    }
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        "memory"
    } else {
        "memories"
    }
}

fn receipt(verb: &str, r: &Remembered) -> String {
    let when = match r.matures_at_ms {
        Some(ms) => format!("; it can be recalled into context after {}", date_time(ms)),
        None => String::new(),
    };
    format!("{verb} {} (trust: {}{when})", r.id, r.trust.label())
}

/// One hit as two lines: id and text, then kind · trust · date · provenance.
pub(crate) fn format_hit(h: &RecallHit) -> String {
    let scope = match &h.scope {
        Scope::Global => String::new(),
        Scope::Project(p) => format!(" · project {p}"),
    };
    format!(
        "- [{}] {}\n  {} · {} · {}{} · {}\n",
        h.id,
        one_line(&h.text),
        h.kind.label(),
        h.trust.label(),
        date(h.occurred_at_ms),
        scope,
        h.provenance
    )
}

/// Local calendar date of a Unix-ms instant.
pub(crate) fn date(ms: i64) -> String {
    local(ms).map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_else(|| "?".into())
}

fn date_time(ms: i64) -> String {
    local(ms).map(|d| d.format("%Y-%m-%d %H:%M").to_string()).unwrap_or_else(|| "?".into())
}

fn local(ms: i64) -> Option<chrono::DateTime<chrono::Local>> {
    chrono::DateTime::from_timestamp_millis(ms).map(|d| d.with_timezone(&chrono::Local))
}

/// The four memory tools' declarations (what [`MindTools`] offers the model).
pub(crate) fn memory_specs() -> Vec<ToolSpec> {
    ["remember", "recall", "forget", "correct"].map(marlowe_agent::compact_specs::spec).to_vec()
}

impl ToolHost for MindTools {
    fn specs(&self) -> Vec<ToolSpec> {
        memory_specs()
    }

    fn call(&self, call: &ToolCallReq, ctx: &ToolCtx) -> ToolOutcome {
        match call.name.as_str() {
            "remember" => self.remember(&call.args, ctx),
            "recall" => self.recall(call),
            "forget" => self.forget(call, ctx),
            "correct" => self.correct(&call.args, ctx),
            other => ToolOutcome::error(format!("unknown tool {other}")),
        }
    }

    fn consequence(&self, _call: &ToolCallReq) -> Consequence {
        Consequence::Free
    }

    fn consequence_exposed(&self, call: &ToolCallReq, exposure: &Exposure) -> Consequence {
        self.latch_exposure(exposure);
        if call.name == "forget" {
            return self.forget_consequence(call);
        }
        Consequence::Free
    }

    fn taints(&self, call: &ToolCallReq) -> bool {
        call.name == "recall" && lock(&self.untrusted_recalls).contains(&call.id)
    }

    fn describe(&self, call: &ToolCallReq) -> String {
        let key = match call.name.as_str() {
            "remember" => "text",
            "recall" => "query",
            _ => "id",
        };
        match str_arg(&call.args, key) {
            Some(v) => format!("{} {}", call.name, clip(&one_line(v), 60)),
            None => call.name.clone(),
        }
    }
}

/// The session's tool host plus the memory tools. Memory tools shadow same-named inner tools.
pub struct ComposedTools {
    inner: Arc<dyn ToolHost>,
    memory: MindTools,
}

impl ComposedTools {
    pub fn new(inner: Arc<dyn ToolHost>, memory: MindTools) -> Self {
        Self { inner, memory }
    }
}

impl ToolHost for ComposedTools {
    fn specs(&self) -> Vec<ToolSpec> {
        let mut specs: Vec<ToolSpec> =
            self.inner.specs().into_iter().filter(|s| !MindTools::is_memory_tool(&s.name)).collect();
        specs.extend(self.memory.specs());
        specs
    }

    fn call(&self, call: &ToolCallReq, ctx: &ToolCtx) -> ToolOutcome {
        if MindTools::is_memory_tool(&call.name) {
            self.memory.call(call, ctx)
        } else {
            self.inner.call(call, ctx)
        }
    }

    fn call_scoped(&self, call: &ToolCallReq, cx: &marlowe_agent::CallCtx) -> ToolOutcome {
        if MindTools::is_memory_tool(&call.name) {
            self.memory.call_scoped(call, cx)
        } else {
            self.inner.call_scoped(call, cx)
        }
    }

    fn middleware(&self) -> Vec<Arc<dyn marlowe_agent::ToolMiddleware>> {
        let mut m = self.inner.middleware();
        m.extend(self.memory.middleware());
        m
    }

    fn persona_blocks(&self) -> Vec<marlowe_agent::PersonaBlock> {
        let mut b = self.inner.persona_blocks();
        b.extend(self.memory.persona_blocks());
        b
    }

    fn consequence(&self, call: &ToolCallReq) -> Consequence {
        if MindTools::is_memory_tool(&call.name) {
            self.memory.consequence(call)
        } else {
            self.inner.consequence(call)
        }
    }

    fn consequence_exposed(&self, call: &ToolCallReq, exposure: &Exposure) -> Consequence {
        // Every call passes here before any runs, so the latch is current for the batch.
        self.memory.latch_exposure(exposure);
        if MindTools::is_memory_tool(&call.name) {
            self.memory.consequence_exposed(call, exposure)
        } else {
            self.inner.consequence_exposed(call, exposure)
        }
    }

    fn taints(&self, call: &ToolCallReq) -> bool {
        if MindTools::is_memory_tool(&call.name) {
            self.memory.taints(call)
        } else {
            self.inner.taints(call)
        }
    }

    fn egress_host(&self, call: &ToolCallReq) -> Option<String> {
        if MindTools::is_memory_tool(&call.name) {
            None
        } else {
            self.inner.egress_host(call)
        }
    }

    fn describe(&self, call: &ToolCallReq) -> String {
        if MindTools::is_memory_tool(&call.name) {
            self.memory.describe(call)
        } else {
            self.inner.describe(call)
        }
    }

    fn tool_spec(&self, name: &str) -> Option<ToolSpec> {
        if MindTools::is_memory_tool(name) {
            self.memory.tool_spec(name)
        } else {
            self.inner.tool_spec(name)
        }
    }

    fn arg_rules(&self, name: &str) -> Option<marlowe_agent::args::ArgRules> {
        if MindTools::is_memory_tool(name) {
            marlowe_agent::args::wired_rules(name)
        } else {
            self.inner.arg_rules(name)
        }
    }

    fn prompt_hints(&self) -> Vec<String> {
        self.inner.prompt_hints()
    }

    fn manifest(&self, name: &str) -> Option<marlowe_agent::ToolManifest> {
        if MindTools::is_memory_tool(name) {
            let effects = if name == "recall" { marlowe_agent::Effects::READ } else { marlowe_agent::Effects::NONE };
            Some(marlowe_agent::ToolManifest {
                group: marlowe_agent::ToolGroup::Memory,
                profiles: marlowe_agent::Profiles::ALL,
                effects,
                path_args: &[],
            })
        } else {
            self.inner.manifest(name)
        }
    }

    fn mutation(&self, call: &ToolCallReq, ctx: &ToolCtx) -> marlowe_agent::Mutation {
        if MindTools::is_memory_tool(&call.name) {
            marlowe_agent::Mutation::None
        } else {
            self.inner.mutation(call, ctx)
        }
    }

    fn consequence_in_sandbox(
        &self,
        call: &ToolCallReq,
        exposure: &Exposure,
        sandbox: &dyn marlowe_agent::SandboxView,
    ) -> Option<Consequence> {
        if MindTools::is_memory_tool(&call.name) {
            None
        } else {
            self.inner.consequence_in_sandbox(call, exposure, sandbox)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(kind: MemoryKind, text: &str) -> RecallHit {
        RecallHit {
            id: "m1".into(),
            text: text.into(),
            kind,
            trust: Trust::User,
            scope: Scope::Project("C:/ws".into()),
            occurred_at_ms: 1_700_000_000_000,
            provenance: "you".into(),
            score: 1.0,
        }
    }

    #[test]
    fn the_recalled_summary_is_one_short_line() {
        let hits = vec![hit(MemoryKind::Preference, "tabs\nover spaces"), hit(MemoryKind::Fact, &"x".repeat(400))];
        let s = recalled_summary(&hits);
        assert!(s.starts_with("preference · tabs over spaces; fact · x"));
        assert!(!s.contains('\n'));
        assert_eq!(s.chars().count(), RECALLED_SUMMARY_CHARS + 1, "clipped with an ellipsis");
    }

    /// A search backend that is never called: the binary wires one, so `web_search` is counted.
    struct NoBackend;

    impl marlowe_exec::SearchBackend for NoBackend {
        fn search(&self, _: &str, _: &marlowe_model::CancelToken) -> Result<marlowe_exec::SearchResponse, String> {
            Err("not wired in this test".into())
        }
    }

    /// The wired conductor on dusk at 32k in a git repository with a Cargo manifest (so the git and
    /// verify groups are live next to the core ones): the real built-ins (host built with a search
    /// backend, as `wire.rs` does), the real memory and dispatch declarations (the 13 built-in
    /// roles in `dispatch`'s menu) and the real persona blocks the session would append.
    struct WiredDusk {
        root: std::path::PathBuf,
        host: marlowe_exec::BuiltinTools,
        specs: Vec<marlowe_model::ToolSpec>,
        system: marlowe_agent::SystemPrompt,
    }

    impl Drop for WiredDusk {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn wired_dusk() -> WiredDusk {
        use marlowe_agent::{PersonaBlock, SystemPrompt};
        use marlowe_model::ProviderKind;

        let root = std::env::temp_dir().join(format!("marlowe-a15-{}-{}", std::process::id(), now_ms()));
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".git").join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"x\"\nversion = \"0.1.0\"\n").unwrap();
        let home = root.join("home");
        std::fs::create_dir_all(&home).unwrap();

        let dusk = marlowe_model::ModelInfo { context_window: Some(32_768), ..marlowe_model::ModelInfo::bare(ProviderKind::Ollama, "marlowe-dusk:27b-super") };
        let host = marlowe_exec::BuiltinTools::new(Some(root.clone())).with_model_exposure(&dusk).with_search(Arc::new(NoBackend));
        let host = marlowe_exec::specs::configure(host, &marlowe_exec::specs::HostSetup { home: &home, workspace: Some(&root) });
        let mut specs = host.specs();
        specs.extend(memory_specs());
        specs.push(marlowe_crew::dispatch::dispatch_spec(&marlowe_crew::Roles::builtin().infos()));
        let system = SystemPrompt::new().block(PersonaBlock::Memory).block(PersonaBlock::Crew).blocks_from(host.persona_blocks());
        WiredDusk { root, host, specs, system }
    }

    /// Gate A15 on the wired conductor, the shape of the surface: every built-in of the detected
    /// groups is listed on a 32k window (nothing hides behind a search), no `tool_search` exists
    /// without MCP overflow, and no group's persona block is held back.
    #[test]
    fn the_wired_dusk_lists_every_built_in_and_holds_no_block_back() {
        let w = wired_dusk();
        let names: Vec<&str> = w.specs.iter().map(|s| s.name.as_str()).collect();
        for live in ["project", "remember", "dispatch", "git", "gh", "web_search", "web_fetch", "workflow", "monitor", "symbols"] {
            assert!(names.contains(&live), "{live} missing: {names:?}");
        }
        assert!(!names.contains(&"tool_search"), "no tool_search without MCP overflow: {names:?}");
        let table = marlowe_exec::specs::table();
        for d in table.defs.iter().filter(|d| w.host.exposes(d) && (d.enabled)(&w.host)) {
            assert!(names.contains(&d.name()), "built-in {} is exposed but not listed", d.name());
        }
        // Every group with a block and a live tool has its block in the prompt.
        let persona_text = w.system.persona_text();
        for g in marlowe_exec::specs::GROUPS {
            let live = table.defs.iter().any(|d| d.manifest.group == g.group && w.host.exposes(d) && (d.enabled)(&w.host));
            if let (true, Some(block)) = (live, (g.persona_block)()) {
                assert!(persona_text.contains(block.trim_end()), "{:?}'s block is held back", g.group);
            }
        }
        for held in ["<coding>", "<verify_tools>", "<git_tools>"] {
            assert!(persona_text.contains(held), "{held} is held back");
        }
        assert_eq!(w.host.persona_blocks().len(), w.system.blocks().len() - 2, "memory and crew are the daemon's; the rest are the host's");
    }

    /// Gate A15, the sizes (design.budget): tools inside 7,000 tokens, persona and blocks inside
    /// 2,000, the fitted prefix inside 12,500 with project instructions at their cap arriving
    /// uncut.
    #[test]
    fn the_wired_dusk_prefix_fits_its_slices_in_a_repository() {
        use marlowe_agent::context::{estimate_tokens, fit_prefix, tool_spec_tokens, PrefixBudget};
        use marlowe_agent::system::PromptSections;

        let w = wired_dusk();
        let tools = tool_spec_tokens(&w.specs);
        for s in &w.specs {
            eprintln!("{:>5}  {}", tool_spec_tokens(std::slice::from_ref(s)), s.name);
        }
        let persona = estimate_tokens(&w.system.persona_text());
        let budget = PrefixBudget::for_window(32_768);
        let sections = PromptSections {
            instructions: format!("<instructions>\n{}\n</instructions>", "Follow the project rules. ".repeat(170)),
            project_profile: "build: cargo build\ntest: cargo test\n".repeat(20),
            repo_map: "src/lib.rs: fn main, struct Agent\n".repeat(200),
            skills_index: "- skill: does a thing\n".repeat(40),
            output_style: String::new(),
            warnings: Vec::new(),
        };
        let fit = fit_prefix(persona, tools, &sections, budget);
        eprintln!(
            "A15 wired: persona+blocks {persona}/{} tools {tools}/{} prefix {}/{} warnings {:?}",
            budget.persona, budget.tools, fit.tokens, budget.total, fit.warnings
        );
        assert!(tools <= budget.tools, "tool schemas are {tools} tokens, over their {} slice", budget.tools);
        assert!(persona <= budget.persona, "persona and blocks are {persona} tokens, over their {} slice", budget.persona);
        assert!(fit.tokens <= budget.total, "the prefix is {} tokens: {:?}", fit.tokens, fit.warnings);
        assert!(!fit.warnings.iter().any(|w| w.contains("over the prefix budget")), "the fitter had to cut: {:?}", fit.warnings);
    }

    #[test]
    fn a_hit_shows_kind_trust_date_scope_and_provenance() {
        let line = format_hit(&hit(MemoryKind::Fact, "the cluster is in Oslo"));
        assert!(line.starts_with("- [m1] the cluster is in Oslo\n  fact · you · 2023-11-1"), "{line}");
        assert!(line.contains("· project C:/ws · you"));
    }
}
