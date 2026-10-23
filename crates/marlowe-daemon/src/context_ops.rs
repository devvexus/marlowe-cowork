//! Context and hardware requests: the plan, the policies, saving one, and the approval-gated
//! Ollama restart. On a worker thread: a plan reads a GGUF header, the runner log, `/api/ps` and
//! the GPU, and a restart waits on the user.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;

use marlowe_hw::ServerChange;
use marlowe_proto::{ApprovalDecision, ChangeWhen, Event, Request, SessionId, Slot, SERVER_RESTART_TOOL};

use crate::daemon::Core;

/// The session an approval for a server restart names (it belongs to no conversation).
pub(crate) const CONTEXT_SESSION: &str = "context-planner";

pub(crate) fn dispatch(core: &Arc<Core>, client: u64, req: Request) {
    let c = core.clone();
    let spawned = std::thread::Builder::new().name("marlowe-context".into()).spawn(move || {
        if catch_unwind(AssertUnwindSafe(|| handle(&c, client, req))).is_err() {
            c.bus.reply(client, error("the context planner failed".into()));
        }
    });
    if spawned.is_err() {
        core.bus.reply(client, error("the daemon is out of threads".into()));
    }
}

fn error(message: String) -> Event {
    Event::Error { session: None, message }
}

fn handle(core: &Core, client: u64, req: Request) {
    let b = &core.brains;
    match req {
        Request::ContextPlan { model } => match b.context_plan(&model) {
            Ok(p) => core.bus.reply(client, Event::ContextPlan(Box::new(p))),
            Err(e) => core.bus.reply(client, error(format!("context plan for {model}: {e}"))),
        },
        Request::ContextPolicies => match b.context_policies() {
            Ok(models) => core.bus.reply(client, Event::ContextPolicies { models }),
            Err(e) => core.bus.reply(client, error(e.to_string())),
        },
        Request::SetContextPolicy { model, policy } => match b.context().set_policy(&model, policy) {
            Ok(changes) => {
                let pending_restart = changes.iter().any(|c| c.when == ChangeWhen::Restart);
                let policy = b.context().policy(&model).unwrap_or(policy);
                core.bus.publish(Event::ContextPolicySaved { model, policy, changes, pending_restart });
                if let Ok(models) = b.context_policies() {
                    core.bus.publish(Event::ContextPolicies { models });
                }
            }
            Err(e) => core.bus.reply(client, error(e.to_string())),
        },
        Request::SetModelEngine { model, llama } => match b.set_model_engine(&model, llama) {
            Ok(()) => {
                if let Ok(models) = b.context_policies() {
                    core.bus.publish(Event::ContextPolicies { models });
                }
                match b.context_plan(&model) {
                    Ok(p) => core.bus.reply(client, Event::ContextPlan(Box::new(p))),
                    Err(e) => core.bus.reply(client, error(format!("context plan for {model}: {e}"))),
                }
            }
            Err(e) => core.bus.reply(client, error(e.to_string())),
        },
        Request::ApplyOllamaServer { kv_type, parallel, flash_attn } => {
            let change = ServerChange { kv_type, parallel, flash_attn };
            apply(core, change);
        }
        _ => core.bus.reply(client, error("not a context request".into())),
    }
}

/// Asks first, always: what restarts, what it unloads, what every Ollama model will use.
fn apply(core: &Core, change: ServerChange) {
    if change.is_empty() {
        core.bus.publish(Event::ServerApplied { ok: true, message: "nothing to change".into() });
        return;
    }
    let vars = change.variables();
    let summary = format!(
        "Restart Ollama with {}",
        vars.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(", ")
    );
    let engine = core.brains.resolve(&Slot::Conductor).ok().map(|(m, _)| core.brains.context_plan(&m).map(|p| p.in_effect));
    let running = match engine {
        Some(Ok(e)) => e.slots.iter().filter(|s| s.is_processing).count(),
        _ => 0,
    };
    // Every loaded model goes, not only the conductor's.
    let loaded = core.brains.loaded_local_models();
    let unloads = match loaded.len() {
        0 => "No model is loaded now".to_string(),
        n => format!("That unloads {n} model{} ({})", if n == 1 { "" } else { "s" }, loaded.join(", ")),
    };
    let mut detail = vec![
        "These are Ollama server settings: every Ollama model uses them, not only this one.".to_string(),
        format!(
            "Marlowe sets them in your user environment, quits the Ollama app and starts it again. {unloads}, stops every request in flight ({running} on the conductor's model now) and ends any `ollama` command running in a terminal.",
        ),
        String::new(),
        "To do it yourself instead:".to_string(),
    ];
    detail.extend(change.manual_steps().into_iter().map(|s| format!("  {s}")));
    if core.is_shutting_down() {
        return;
    }
    let (id, _tx, rx) = core.approvals.register();
    core.bus.publish(Event::ApprovalRequested {
        approval: id,
        session: SessionId(CONTEXT_SESSION.into()),
        tool: SERVER_RESTART_TOOL.into(),
        summary,
        detail: detail.join("\n"),
        run: None,
        proposed_rule: None,
    });
    let decision = rx.recv_timeout(core.cfg.approval_timeout).unwrap_or(ApprovalDecision::Deny);
    core.approvals.remove(id);
    core.bus.publish(Event::ApprovalResolved { approval: id, decision });
    let manual = change.manual_steps().join("; ");
    let event = if decision.is_allowed() {
        match core.brains.apply_server(change) {
            Ok(message) => Event::ServerApplied { ok: true, message },
            Err(e) => Event::ServerApplied { ok: false, message: format!("{e}. To finish by hand: {manual}") },
        }
    } else {
        Event::ServerApplied { ok: false, message: format!("not restarted; the change waits. To apply it by hand: {manual}") }
    };
    core.bus.publish(event);
    if let Ok(models) = core.brains.context_policies() {
        core.bus.publish(Event::ContextPolicies { models });
    }
}
