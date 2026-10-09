//! Memory wiring: the agent's `MemoryHook`, the conductor's memory tools, and the composed host.
//! Memory requests, the legacy offer and episodes live in [`crate::mind_ops`].

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use marlowe_agent::{Consequence, Exposure, MemoryHook, Recollection, ToolCallReq, ToolCtx, ToolHost, ToolOutcome};
use marlowe_mind::{
    InjectQuery, Kind, MemoryChange, MemoryIn, MemoryKind, Mind, Provenance, RecallHit, RecallQuery, Remembered,
    Scope, Trust, TurnIn, TurnRole,
};
use marlowe_model::ToolSpec;
use marlowe_proto::{Event, SessionId};
use serde_json::{json, Value};

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

/// The agent's memory for one turn: `before_turn` injects via [`Mind::inject`], emits `Recalled`
/// and ingests the user's words; `after_turn` ingests the assistant's reply.
pub struct MindMemory {
    mind: Mind,
    workspace: Option<String>,
    turn: u32,
    budget_tokens: u32,
    emit: RecallSink,
}

impl MindMemory {
    /// `budget_tokens` is capped at `marlowe_mind::MAX_INJECT_TOKENS`.
    pub fn new(mind: Mind, workspace: Option<String>, turn: u32, budget_tokens: u32, emit: RecallSink) -> Self {
        let budget_tokens = budget_tokens.min(marlowe_mind::MAX_INJECT_TOKENS);
        Self { mind, workspace, turn, budget_tokens, emit }
    }

    fn ingest(&self, session: &SessionId, role: TurnRole, text: &str) {
        if text.trim().is_empty() {
            return;
        }
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
        let injection = self.mind.inject(InjectQuery {
            session: session.to_string(),
            workspace: self.workspace.clone(),
            text: query.to_string(),
            budget_tokens: self.budget_tokens,
        });
        // Ingested here rather than in `after_turn` (which a failed turn skips), and after the
        // inject so a statement is never recalled back to itself.
        self.ingest(session, TurnRole::User, query);
        if !injection.recalled.is_empty() {
            (self.emit)(Event::Recalled {
                session: session.clone(),
                turn: self.turn,
                count: injection.recalled.len() as u32,
                summary: recalled_summary(&injection.recalled),
            });
        }
        // Untrusted lineage in the context taints the run, as an untrusted `recall` does.
        let untrusted = injection.floor == Some(Trust::Untrusted);
        Recollection { blocks: injection.blocks, untrusted }
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

fn spec(name: &str, description: &str, schema: Value) -> ToolSpec {
    ToolSpec { name: name.into(), description: description.into(), input_schema: schema }
}

impl ToolHost for MindTools {
    fn specs(&self) -> Vec<ToolSpec> {
        let kinds = ["fact", "preference", "decision", "result", "note", "procedure"];
        vec![
            spec(
                "remember",
                "Store a durable memory: something the user told you, a decision and its reason, a \
                 result worth keeping, or how to do something. One self-contained statement per call.",
                json!({
                    "type": "object",
                    "properties": {
                        "text": { "type": "string", "description": "The memory, written to stand on its own." },
                        "kind": { "type": "string", "enum": kinds, "default": "fact" },
                        "reason": { "type": "string", "description": "Why; required when kind is decision." },
                        "scope": { "type": "string", "enum": ["global", "project"], "default": "global",
                                   "description": "project: applies only to this session's workspace." }
                    },
                    "required": ["text"]
                }),
            ),
            spec(
                "recall",
                "Search stored memories, including ones not yet matured and forgotten records. Each \
                 hit shows its id, kind, trust, date and provenance.",
                json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string" },
                        "limit": { "type": "integer", "minimum": 1, "maximum": RECALL_MAX, "default": RECALL_DEFAULT }
                    },
                    "required": ["query"]
                }),
            ),
            spec(
                "forget",
                "Forget a memory by id. The record of it stays in the audit journal; its text does not.",
                json!({
                    "type": "object",
                    "properties": { "id": { "type": "string" } },
                    "required": ["id"]
                }),
            ),
            spec(
                "correct",
                "Replace a memory's text with a corrected version. Refused if it would lower the \
                 memory's trust.",
                json!({
                    "type": "object",
                    "properties": { "id": { "type": "string" }, "text": { "type": "string" } },
                    "required": ["id", "text"]
                }),
            ),
        ]
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

    #[test]
    fn a_hit_shows_kind_trust_date_scope_and_provenance() {
        let line = format_hit(&hit(MemoryKind::Fact, "the cluster is in Oslo"));
        assert!(line.starts_with("- [m1] the cluster is in Oslo\n  fact · you · 2023-11-1"), "{line}");
        assert!(line.contains("· project C:/ws · you"));
    }
}
