//! Runs every tool call of one assistant message: free reads start at once in parallel, calls
//! that need approval are asked one at a time (so a "don't ask again" answer covers later calls)
//! and start as soon as they are allowed. Calls that write (by their manifest) run one after
//! another on a single worker, free ones in call order and approved ones as they are allowed, so
//! two edits of one file in one message apply in sequence.
//!
//! Middleware sees every call twice: `before_approval` on the turn thread before the gate
//! decides (it may rewrite the arguments, and the gate, taint and egress judge the rewritten
//! call), then `before_call` / `pre_mutation` / `after_call` around the run itself. The
//! conversation's extensions are lent to the batch through [`CallCtx::scope`] and taken back when
//! it ends.
//!
//! With a [`Permissions`] service (every conductor session) the decision is the permission
//! layer's: mode, rules, tripwires, hooks; a denial's reason goes back to the model and the turn
//! goes on. "Don't ask again" grants the proposed rule (tool plus command prefix, path glob or
//! domain, never a bare tool) for the session or into a settings file. PermissionRequest hooks
//! may answer an approval before the user sees it, but only the user allows a tripwire. Without
//! the service (the crew, tests) the tool's own consequence decides, as before.

use std::collections::HashSet;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Mutex};
use std::thread;

use marlowe_model::{CancelToken, ToolCall, ToolResult};
use marlowe_proto::SettingsScope;
use serde_json::Value;

use crate::approval::{ApprovalGate, ApprovalReq, Gate, HookVerdicts, Permissions};
use crate::convo::Conversation;
use crate::events::AgentEvent;
use crate::hooks::HookRunner;
use crate::middleware::{CallFlow, ToolMiddleware};
use crate::tools::{CallCtx, Consequence, Mutation, Services, SessionScope, ToolCtx, ToolHost, ToolOutcome};
use crate::ApprovalDecision;

/// Content returned to the model for a denied call.
pub(crate) const DENIED: &str =
    "The user denied this call. Do not retry it; continue without it or ask the user how to proceed.";

/// Content returned to the model for a call skipped because the turn was interrupted.
pub(crate) const INTERRUPTED: &str = "Not run: the user interrupted the turn.";

/// Content for a call the permission layer refused without asking.
fn refused(why: &str) -> ToolOutcome {
    ToolOutcome {
        summary: "not allowed".into(),
        ..ToolOutcome::error(format!(
            "Not run: {why}. Do not retry it unchanged; continue another way, or tell the user what you need."
        ))
    }
}

enum Msg {
    /// A call's outcome; `ran` is false for denied or interrupted calls.
    Done { i: usize, out: ToolOutcome, ran: bool },
    AllowForSession(String),
    HostApproved(String),
}

/// One call waiting for approval.
struct Pending {
    i: usize,
    summary: String,
    proposal: Vec<String>,
    hosts: Vec<String>,
    /// Only the user may allow it (a PermissionRequest hook may still deny it).
    tripwire: bool,
}

/// Runs `calls` and returns one result per call, in call order. Emits `ToolStarted` for every
/// call up front (in order) and `ToolFinished` as each completes. Every call passes through
/// `middleware`: `before_approval` before the gate, then `before_call`, `pre_mutation` and
/// `after_call` around the run.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_calls(
    host: &dyn ToolHost,
    middleware: &[Arc<dyn ToolMiddleware>],
    services: &Services,
    gate: &dyn ApprovalGate,
    calls: &[ToolCall],
    convo: &mut Conversation,
    turn: u32,
    cancel: &CancelToken,
    sink: &mut dyn FnMut(AgentEvent),
) -> Vec<ToolResult> {
    let tool = ToolCtx::new(convo.session.clone(), turn, convo.workspace.clone(), cancel.clone(), convo.exposure.tainted);
    let ext = Arc::new(Mutex::new(std::mem::take(&mut convo.ext)));
    let cx = CallCtx::new(tool, SessionScope { ext: ext.clone(), services: services.clone() });
    for c in calls {
        sink(AgentEvent::ToolStarted { call_id: c.id.clone(), tool: c.name.clone(), summary: host.describe(c) });
    }
    let perms = services.get::<Permissions>();

    let mut outcomes: Vec<Option<ToolOutcome>> = vec![None; calls.len()];
    // The calls as they will run: `before_approval` may have rewritten their arguments.
    let mut effective: Vec<ToolCall> = calls.to_vec();
    let mut free = Vec::new();
    let mut ask: Vec<Pending> = Vec::new();
    for (i, c) in effective.iter_mut().enumerate() {
        if let Some(bad) = invalid_args(c) {
            outcomes[i] = Some(bad);
            continue;
        }
        if let Some(answered) = before_approval(middleware, c, &cx) {
            outcomes[i] = Some(answered);
            continue;
        }
        if let Some(bad) = invalid_args(c) {
            outcomes[i] = Some(bad);
            continue;
        }
        match perms {
            Some(p) => {
                let hook = cx.scope.with_ext(|e| e.get_mut::<HookVerdicts>().and_then(|v| v.0.remove(&c.id)));
                match p.decide(host, c, &convo.exposure, hook) {
                    Gate::Free => free.push(i),
                    Gate::Deny(why) => outcomes[i] = Some(refused(&why)),
                    Gate::Ask { summary, proposal, hosts, tripwire } => ask.push(Pending { i, summary, proposal, hosts, tripwire }),
                }
            }
            // Without the permission layer (the crew, tests) a session allow is keyed on the tool
            // name, so it frees only an ordinary ask: a fixed one (`AskEvery`) asks every time.
            None => match host.consequence_exposed(c, &convo.exposure) {
                Consequence::Free => free.push(i),
                Consequence::Ask { .. } if convo.is_allowed_for_session(&c.name) => free.push(i),
                Consequence::Ask { summary } => ask.push(Pending { i, summary, proposal: Vec::new(), hosts: Vec::new(), tripwire: false }),
                Consequence::AskEvery { summary } => {
                    ask.push(Pending { i, summary, proposal: Vec::new(), hosts: Vec::new(), tripwire: true })
                }
            },
        }
    }
    // Precomputed outcomes still report a finish.
    for (i, o) in outcomes.iter().enumerate() {
        if let Some(o) = o {
            sink(finished(&calls[i], o));
        }
    }

    let allowed = convo.session_allows.clone();
    let hooks = services.get::<Arc<HookRunner>>();
    let (tx, rx) = crossbeam_channel::unbounded::<Msg>();
    let calls: &[ToolCall] = &effective;
    let ctx = &cx;
    let writes = move |i: usize| host.manifest(&calls[i].name).is_some_and(|m| m.effects.writes);
    let (wtx, wrx) = crossbeam_channel::unbounded::<usize>();
    thread::scope(|s| {
        for &i in &free {
            if writes(i) {
                let _ = wtx.send(i);
                continue;
            }
            let tx = tx.clone();
            s.spawn(move || {
                let _ = tx.send(Msg::Done { i, out: run_one(host, middleware, &calls[i], ctx), ran: true });
            });
        }
        {
            let tx = tx.clone();
            s.spawn(move || {
                for i in wrx {
                    let _ = tx.send(Msg::Done { i, out: run_one(host, middleware, &calls[i], ctx), ran: true });
                }
            });
        }
        if !ask.is_empty() {
            let tx = tx.clone();
            let wtx = wtx.clone();
            s.spawn(move || {
                let mut allowed: HashSet<String> = allowed;
                for Pending { i, summary, proposal, hosts, tripwire } in ask {
                    let c = &calls[i];
                    if cancel.is_cancelled() {
                        let _ = tx.send(Msg::Done { i, out: interrupted(), ran: false });
                        continue;
                    }
                    let decision = match perms {
                        // A rule granted earlier in this batch already covers it.
                        Some(p) if p.granted(&proposal) => ApprovalDecision::Allow,
                        Some(p) => {
                            // PermissionRequest hooks may answer before the user is asked; a
                            // tripwire (push, delete outside, new host, after taint) only the
                            // user may allow, so a hook's `allow` there still asks.
                            match hooks.and_then(|h| h.permission_request(&c.name, &c.args, ctx, p.mode())) {
                                Some((true, _)) if !tripwire => ApprovalDecision::Allow,
                                Some((false, why)) => {
                                    let out = refused(&why.unwrap_or_else(|| "a PermissionRequest hook denied it".into()));
                                    let _ = tx.send(Msg::Done { i, out, ran: false });
                                    continue;
                                }
                                _ => gate.ask_with(req(ctx, c, summary), (!proposal.is_empty()).then(|| proposal.join("\n"))),
                            }
                        }
                        None if !tripwire && allowed.contains(&c.name) => ApprovalDecision::Allow,
                        None => gate.ask(req(ctx, c, summary)),
                    };
                    if !decision.is_allowed() {
                        let denied = ToolOutcome { summary: "denied".into(), ..ToolOutcome::error(DENIED) };
                        let _ = tx.send(Msg::Done { i, out: denied, ran: false });
                        continue;
                    }
                    if let Some(h) = host.egress_host(c) {
                        let _ = tx.send(Msg::HostApproved(h));
                    }
                    for h in hosts {
                        let _ = tx.send(Msg::HostApproved(h));
                    }
                    match (perms, decision) {
                        (Some(p), ApprovalDecision::AllowForSession) if !proposal.is_empty() => {
                            let _ = p.grant(&proposal, SettingsScope::Session);
                        }
                        (Some(p), ApprovalDecision::AllowPattern { scope }) if !proposal.is_empty() => {
                            // If the rule cannot be saved it still holds for this session.
                            if p.grant(&proposal, scope).is_err() {
                                let _ = p.grant(&proposal, SettingsScope::Session);
                            }
                        }
                        (None, ApprovalDecision::AllowForSession) if !tripwire && allowed.insert(c.name.clone()) => {
                            let _ = tx.send(Msg::AllowForSession(c.name.clone()));
                        }
                        _ => {}
                    }
                    if writes(i) {
                        let _ = wtx.send(i);
                        continue;
                    }
                    let tx = tx.clone();
                    s.spawn(move || {
                        let _ = tx.send(Msg::Done { i, out: run_one(host, middleware, &calls[i], ctx), ran: true });
                    });
                }
            });
        }
        drop(wtx);
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
    // Take the extensions back; a tool that kept the scope sees them empty from here on.
    convo.ext = std::mem::take(&mut *ext.lock().unwrap_or_else(|p| p.into_inner()));

    calls
        .iter()
        .zip(outcomes)
        .map(|(c, o)| {
            let o = o.unwrap_or_else(|| ToolOutcome::error("the tool produced no result"));
            ToolResult { call_id: c.id.clone(), content: o.content, is_error: o.is_error, parts: o.parts }
        })
        .collect()
}

fn req(cx: &CallCtx, c: &ToolCall, summary: String) -> ApprovalReq {
    ApprovalReq {
        session: cx.session.clone(),
        call_id: c.id.clone(),
        tool: c.name.clone(),
        summary,
        detail: serde_json::to_string_pretty(&c.args).unwrap_or_else(|_| c.args.to_string()),
    }
}

fn finished(c: &ToolCall, o: &ToolOutcome) -> AgentEvent {
    AgentEvent::ToolFinished { call_id: c.id.clone(), ok: !o.is_error, summary: o.summary.clone() }
}

fn interrupted() -> ToolOutcome {
    ToolOutcome { summary: "interrupted".into(), ..ToolOutcome::error(INTERRUPTED) }
}

/// Runs `before_approval` over one call; `Some` when a middleware answered for it (or panicked).
/// Only the arguments may change: the name and id are restored.
fn before_approval(middleware: &[Arc<dyn ToolMiddleware>], call: &mut ToolCall, cx: &CallCtx) -> Option<ToolOutcome> {
    if middleware.is_empty() {
        return None;
    }
    let (name, id) = (call.name.clone(), call.id.clone());
    let answered = catch_unwind(AssertUnwindSafe(|| {
        middleware.iter().find_map(|m| match m.before_approval(call, cx) {
            CallFlow::Continue => None,
            CallFlow::Done(out) => Some(out),
        })
    }));
    call.name = name;
    call.id = id;
    answered.unwrap_or_else(|_| Some(ToolOutcome::error(format!("the {} call was refused: a check before it failed", call.name))))
}

/// Runs one call, unless the turn is already interrupted. A panicking tool (or middleware)
/// becomes an error. With middleware: `before_call` may answer for it, `pre_mutation` sees what
/// the call will change, `after_call` may amend the outcome.
fn run_one(host: &dyn ToolHost, middleware: &[Arc<dyn ToolMiddleware>], call: &ToolCall, cx: &CallCtx) -> ToolOutcome {
    if cx.cancel.is_cancelled() {
        return interrupted();
    }
    catch_unwind(AssertUnwindSafe(|| {
        if middleware.is_empty() {
            return host.call_scoped(call, cx);
        }
        for m in middleware {
            if let CallFlow::Done(out) = m.before_call(call, cx) {
                return out;
            }
        }
        let mutation = host.mutation(call, &cx.tool);
        if mutation != Mutation::None {
            for m in middleware {
                if let Err(out) = m.pre_mutation(call, cx, &mutation) {
                    return out;
                }
            }
        }
        let mut out = host.call_scoped(call, cx);
        for m in middleware {
            m.after_call(call, cx, &mut out);
        }
        out
    }))
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
