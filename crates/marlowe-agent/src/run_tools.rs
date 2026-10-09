//! Runs every tool call of one assistant message: free calls start at once in parallel, calls
//! that need approval are asked one at a time (so "allow for session" covers later calls) and
//! start as soon as they are allowed.

use std::collections::HashSet;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::thread;

use marlowe_model::{CancelToken, ToolCall, ToolResult};
use serde_json::Value;

use crate::approval::{ApprovalGate, ApprovalReq};
use crate::convo::Conversation;
use crate::events::AgentEvent;
use crate::tools::{Consequence, ToolCtx, ToolHost, ToolOutcome};
use crate::ApprovalDecision;

/// Content returned to the model for a denied call.
pub(crate) const DENIED: &str =
    "The user denied this call. Do not retry it; continue without it or ask the user how to proceed.";

/// Content returned to the model for a call skipped because the turn was interrupted.
pub(crate) const INTERRUPTED: &str = "Not run: the user interrupted the turn.";

enum Msg {
    /// A call's outcome; `ran` is false for denied or interrupted calls.
    Done { i: usize, out: ToolOutcome, ran: bool },
    AllowForSession(String),
    HostApproved(String),
}

/// Runs `calls` and returns one result per call, in call order. Emits `ToolStarted` for every
/// call up front (in order) and `ToolFinished` as each completes.
pub(crate) fn run_calls(
    host: &dyn ToolHost,
    gate: &dyn ApprovalGate,
    calls: &[ToolCall],
    convo: &mut Conversation,
    turn: u32,
    cancel: &CancelToken,
    sink: &mut dyn FnMut(AgentEvent),
) -> Vec<ToolResult> {
    let ctx = ToolCtx {
        session: convo.session.clone(),
        turn,
        workspace: convo.workspace.clone(),
        cancel: cancel.clone(),
        tainted: convo.exposure.tainted,
    };
    for c in calls {
        sink(AgentEvent::ToolStarted { call_id: c.id.clone(), tool: c.name.clone(), summary: host.describe(c) });
    }

    let mut outcomes: Vec<Option<ToolOutcome>> = vec![None; calls.len()];
    let mut free = Vec::new();
    let mut ask = Vec::new();
    for (i, c) in calls.iter().enumerate() {
        if let Some(bad) = invalid_args(c) {
            outcomes[i] = Some(bad);
            continue;
        }
        match host.consequence_exposed(c, &convo.exposure) {
            Consequence::Free => free.push(i),
            Consequence::Ask { .. } if convo.is_allowed_for_session(&c.name) => free.push(i),
            Consequence::Ask { summary } => ask.push((i, summary)),
        }
    }
    // Precomputed outcomes still report a finish.
    for (i, o) in outcomes.iter().enumerate() {
        if let Some(o) = o {
            sink(finished(&calls[i], o));
        }
    }

    let allowed = convo.session_allows.clone();
    let (tx, rx) = crossbeam_channel::unbounded::<Msg>();
    let ctx = &ctx;
    thread::scope(|s| {
        for &i in &free {
            let tx = tx.clone();
            s.spawn(move || {
                let _ = tx.send(Msg::Done { i, out: run_one(host, &calls[i], ctx), ran: true });
            });
        }
        if !ask.is_empty() {
            let tx = tx.clone();
            s.spawn(move || {
                let mut allowed: HashSet<String> = allowed;
                for (i, summary) in ask {
                    let c = &calls[i];
                    if cancel.is_cancelled() {
                        let _ = tx.send(Msg::Done { i, out: interrupted(), ran: false });
                        continue;
                    }
                    let decision = if allowed.contains(&c.name) {
                        ApprovalDecision::Allow
                    } else {
                        gate.ask(ApprovalReq {
                            session: ctx.session.clone(),
                            call_id: c.id.clone(),
                            tool: c.name.clone(),
                            summary,
                            detail: serde_json::to_string_pretty(&c.args).unwrap_or_else(|_| c.args.to_string()),
                        })
                    };
                    if !decision.is_allowed() {
                        let denied = ToolOutcome { content: DENIED.into(), is_error: true, summary: "denied".into() };
                        let _ = tx.send(Msg::Done { i, out: denied, ran: false });
                        continue;
                    }
                    if let Some(h) = host.egress_host(c) {
                        let _ = tx.send(Msg::HostApproved(h));
                    }
                    if decision == ApprovalDecision::AllowForSession && allowed.insert(c.name.clone()) {
                        let _ = tx.send(Msg::AllowForSession(c.name.clone()));
                    }
                    let tx = tx.clone();
                    s.spawn(move || {
                        let _ = tx.send(Msg::Done { i, out: run_one(host, &calls[i], ctx), ran: true });
                    });
                }
            });
        }
        drop(tx);
        for msg in rx {
            match msg {
                Msg::Done { i, out, ran } => {
                    if ran && host.taints(&calls[i]) {
                        convo.exposure.tainted = true;
                    }
                    sink(finished(&calls[i], &out));
                    outcomes[i] = Some(out);
                }
                Msg::AllowForSession(tool) => convo.allow_for_session(tool),
                Msg::HostApproved(h) => {
                    convo.exposure.approved_hosts.insert(h);
                }
            }
        }
    });

    calls
        .iter()
        .zip(outcomes)
        .map(|(c, o)| {
            let o = o.unwrap_or_else(|| ToolOutcome::error("the tool produced no result"));
            ToolResult { call_id: c.id.clone(), content: o.content, is_error: o.is_error }
        })
        .collect()
}

fn finished(c: &ToolCall, o: &ToolOutcome) -> AgentEvent {
    AgentEvent::ToolFinished { call_id: c.id.clone(), ok: !o.is_error, summary: o.summary.clone() }
}

fn interrupted() -> ToolOutcome {
    ToolOutcome { content: INTERRUPTED.into(), is_error: true, summary: "interrupted".into() }
}

/// Runs one call, unless the turn is already interrupted. A panicking tool becomes an error.
fn run_one(host: &dyn ToolHost, call: &ToolCall, ctx: &ToolCtx) -> ToolOutcome {
    if ctx.cancel.is_cancelled() {
        return interrupted();
    }
    catch_unwind(AssertUnwindSafe(|| host.call(call, ctx)))
        .unwrap_or_else(|_| ToolOutcome::error(format!("the {} tool failed unexpectedly", call.name)))
}

/// Arguments the fold could not parse arrive as a JSON string; report them instead of running.
fn invalid_args(c: &ToolCall) -> Option<ToolOutcome> {
    if c.name.trim().is_empty() {
        return Some(ToolOutcome::error("the tool call had no tool name"));
    }
    match &c.args {
        Value::String(raw) => {
            let shown: String = raw.chars().take(200).collect();
            Some(ToolOutcome::error(format!(
                "the arguments for {} were not valid JSON; send a JSON object. Received: {shown}",
                c.name
            )))
        }
        _ => None,
    }
}
