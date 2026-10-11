//! The crew inside the daemon: starts the [`Supervisor`], forwards its events to the bus, raises
//! run approvals, relays finished runs into their parent session, and gives the conductor (only
//! the conductor) the `dispatch` tool. Crew requests live in [`crate::crew_ops`].

use std::collections::{HashMap, HashSet};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Mutex, Weak};
use std::thread::JoinHandle;
use std::time::Duration;

use crossbeam_channel::{unbounded, Receiver, RecvTimeoutError};
use marlowe_agent::{ApprovalReq, Consequence, Exposure, ToolCallReq, ToolCtx, ToolHost, ToolOutcome};
use marlowe_crew::{
    CrewDeps, CrewEvent, CrewRunInfo, CrewRunStatus, CrewSetup, Dispatch, DispatchTools, GpuBusy, RunApprovals, RunId,
    Supervisor, DISPATCH_TOOL,
};
use marlowe_model::ToolSpec;
use marlowe_proto::{ApprovalDecision, ApprovalId, Event, GpuReport, SessionId, WatchState};

use crate::daemon::Core;
use crate::lock;
use crate::session::{Enqueue, RELAY_PREFIX};

/// Tools whose results are untrusted content; a role allowed any of them relays tainted.
const UNTRUSTED_TOOLS: [&str; 4] = ["web_search", "web_fetch", "arxiv", "paper"];
/// Longest relayed summary, in characters; the rest is one `marlowe crew show` away.
const RELAY_SUMMARY_CHARS: usize = 6_000;
const RELAY_GOAL_CHARS: usize = 300;
const RELAY_CITATIONS: usize = 12;
/// Utilisation that means heavy graphics (a game, a render) when no Ollama runner is on the
/// device. The desktop alone (compositor, browsers, a live wallpaper) sits near 20% and must not
/// hold local crew runs.
const HEAVY_UTIL_PCT: u32 = 60;

/// The running supervisor and the daemon's bookkeeping around it.
pub(crate) struct CrewLink {
    pub supervisor: Supervisor,
    /// Runs already relayed to their session, so a repeated event never relays twice.
    relayed: Mutex<HashSet<RunId>>,
    /// Relays handed to a session and not yet in its saved history, by relay text.
    awaiting: Mutex<HashMap<String, RunId>>,
    /// Approvals pending per run, denied when the run is cancelled.
    pending: Mutex<HashMap<RunId, Vec<ApprovalId>>>,
    /// A relay turn found no model it could run without loading one: its run stays due, and
    /// the user's next turn delivers it.
    deferred: std::sync::atomic::AtomicBool,
}

impl CrewLink {
    /// Denies every approval the run is waiting on.
    pub fn deny_pending(&self, core: &Core, run: &RunId) {
        for id in lock(&self.pending).remove(run).unwrap_or_default() {
            core.approvals.resolve(id, ApprovalDecision::Deny);
        }
    }
}

/// Starts the supervisor and its event forwarder. A supervisor that fails (or panics) leaves the
/// daemon without a crew: `dispatch` is not exposed and crew requests answer with an error.
pub(crate) fn start(core: &Arc<Core>, setup: CrewSetup) -> Result<JoinHandle<()>, String> {
    let (tx, rx) = unbounded();
    let deps = CrewDeps {
        brains: core.brains.clone(),
        tools: setup.tools,
        approvals: Arc::new(RunGate { core: Arc::downgrade(core) }),
        mind: core.mind.clone(),
        gpu_busy: gpu_probe(Arc::downgrade(core)),
        // With channels and the corpus fetcher, deep research runs v2; without them, v1.
        research: setup.research,
    };
    let home = core.home.clone();
    let supervisor = catch_unwind(AssertUnwindSafe(|| Supervisor::start(&home, deps, setup.config, tx)))
        .map_err(|p| format!("the crew supervisor panicked: {}", panic_text(&p)))?
        .map_err(|e| format!("the crew is not running: {e}"))?;
    let link = CrewLink {
        supervisor,
        relayed: Mutex::new(HashSet::new()),
        awaiting: Mutex::new(HashMap::new()),
        pending: Mutex::new(HashMap::new()),
        deferred: std::sync::atomic::AtomicBool::new(false),
    };
    if core.crew.set(link).is_err() {
        return Err("the crew was already started".into());
    }
    let fcore = core.clone();
    std::thread::Builder::new()
        .name("marlowe-crew-fwd".into())
        .spawn(move || forward(fcore, rx))
        .map_err(|e| format!("crew forwarder: {e}"))
}

/// Stops run threads at their next step boundary; they stay resumable for the next start.
pub(crate) fn shutdown(core: &Core) {
    if let Some(link) = core.crew.get() {
        if let Err(p) = catch_unwind(AssertUnwindSafe(|| link.supervisor.shutdown())) {
            eprintln!("marlowe: the crew did not shut down cleanly: {}", panic_text(&p));
        }
    }
}

fn panic_text(p: &Box<dyn std::any::Any + Send>) -> String {
    p.downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| p.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "panic".into())
}

/// Maps supervisor events onto the bus until the daemon shuts down.
fn forward(core: Arc<Core>, rx: Receiver<CrewEvent>) {
    loop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(ev) => {
                if let Err(p) = catch_unwind(AssertUnwindSafe(|| on_event(&core, ev))) {
                    core.bus.publish(Event::Error { session: None, message: format!("crew event: {}", panic_text(&p)) });
                }
            }
            Err(RecvTimeoutError::Timeout) if core.is_shutting_down() => break,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn on_event(core: &Arc<Core>, ev: CrewEvent) {
    match ev {
        CrewEvent::Updated(info) => {
            let failed = info.status == CrewRunStatus::Failed;
            core.bus.publish(Event::RunUpdated(info.clone()));
            if let (true, Some(session)) = (failed, info.session.clone()) {
                let id = info.id.clone();
                relay(core, &session, &id, None, Some(info));
            }
        }
        CrewEvent::Step { run, step, action, text_delta } => {
            core.bus.publish(Event::RunStep { run, step, action, text_delta });
        }
        CrewEvent::Finished { run, session, dispatch } => {
            // Into the session first, so whoever sees `RunFinished` finds the result already there.
            if let Some(session) = session {
                relay(core, &session, &run, Some(dispatch.clone()), None);
            }
            core.bus.publish(Event::RunFinished { run, dispatch });
        }
        CrewEvent::PlanProposed { run, plan, quote, .. } => {
            crate::brief::speak_notice(core, &plan_notice(&run, &plan, &quote));
            core.bus.publish(Event::PlanProposed { run, plan, quote });
        }
        CrewEvent::Research { run, progress } => {
            core.bus.publish(Event::ResearchProgress { run, progress });
        }
    }
}

/// A waiting research plan as a notice, for the voice: said once per run (outside quiet hours,
/// when voice is on), the same way an urgent Sentinel notice is. The client shows the plan itself.
fn plan_notice(run: &RunId, plan: &marlowe_proto::ResearchPlan, quote: &marlowe_proto::CostQuote) -> marlowe_proto::Notice {
    let n = plan.sub_questions.len();
    marlowe_proto::Notice {
        id: format!("crew-plan-{}", run.as_str()),
        at_ms: crate::now_ms(),
        source: marlowe_proto::NoticeSource::Run,
        severity: marlowe_proto::Severity::Attention,
        kind: "plan_waiting".into(),
        title: "a research plan is waiting for your approval".into(),
        detail: format!(
            "{n} sub-question{} · about {} tokens · {} fetches · {} min",
            if n == 1 { "" } else { "s" },
            quote.model_tokens,
            quote.fetches,
            quote.minutes
        ),
        subject: run.as_str().to_string(),
        seen: false,
    }
}

/// Relays every run whose result (or failure) never reached its session: a daemon that stopped
/// between a run's end and the relay turn, or with relays still queued behind a busy turn.
pub(crate) fn relay_due(core: &Arc<Core>) {
    let Some(link) = core.crew.get() else { return };
    for info in link.supervisor.relays_due().unwrap_or_default() {
        if let Some(session) = info.session.clone() {
            let id = info.id.clone();
            relay(core, &session, &id, None, Some(info));
        }
    }
}

/// A relay turn could not run (relays never load a model): its run stays due and is relayed
/// again after the user's next turn, when the conductor's model is up.
pub(crate) fn defer(core: &Core, text: &str) {
    let Some(link) = core.crew.get() else { return };
    if let Some(run) = lock(&link.awaiting).remove(text) {
        lock(&link.relayed).remove(&run);
        link.deferred.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

/// After a user's turn: relays deferred for want of a loaded model go out now.
pub(crate) fn retry_deferred(core: &Arc<Core>) {
    let Some(link) = core.crew.get() else { return };
    if link.deferred.swap(false, std::sync::atomic::Ordering::SeqCst) {
        relay_due(core);
    }
}

/// A relay's text reached the session's saved history: its run is no longer due.
pub(crate) fn delivered(core: &Core, text: &str) {
    let Some(link) = core.crew.get() else { return };
    if let Some(run) = lock(&link.awaiting).remove(text) {
        let _ = link.supervisor.mark_relayed(&run);
    }
}

/// Posts a finished (or failed) run into its parent session, once per run.
fn relay(core: &Arc<Core>, session: &SessionId, run: &RunId, dispatch: Option<Dispatch>, info: Option<CrewRunInfo>) {
    let Some(link) = core.crew.get() else { return };
    if core.is_shutting_down() || !lock(&link.relayed).insert(run.clone()) {
        return;
    }
    let info = match info {
        Some(i) => i,
        None => match link.supervisor.get(run) {
            Ok(Some(i)) => i,
            _ => return,
        },
    };
    let dispatch = dispatch.or_else(|| info.result.clone());
    let text = relay_text(&info, dispatch.as_ref());
    let tainted = relay_taints(link, &info, dispatch.as_ref());
    let deliver = |core: &Arc<Core>| -> Result<(), Enqueue> {
        let handle = core.session(session).map_err(|(_, m)| Enqueue::Refused(m))?;
        handle.relay(text.clone(), tainted)
    };
    lock(&link.awaiting).insert(text.clone(), run.clone());
    let result = match deliver(core) {
        // The runner went idle and exited after the lookup: load it again.
        Err(Enqueue::Closed) => deliver(core),
        other => other,
    };
    if result.is_err() {
        lock(&link.awaiting).remove(&text);
        // A session that no longer exists will never take it.
        if core.store.get(session).is_ok_and(|s| s.is_none()) {
            let _ = link.supervisor.mark_relayed(run);
        }
    }
    if let Err(e) = result {
        let message = format!("crew run {} finished but could not reach its session: {e}", run.short());
        core.bus.publish(Event::Error { session: Some(session.clone()), message });
    }
}

/// The message a relay turn starts from: the outcome first, then what needs the user, then where
/// the artifacts are.
pub(crate) fn relay_text(info: &CrewRunInfo, dispatch: Option<&Dispatch>) -> String {
    let mut lines = Vec::new();
    let playbook = info.playbook.map(|p| format!(" · {}", p.as_str())).unwrap_or_default();
    lines.push(format!("{RELAY_PREFIX}{} · {}{playbook} · {}]", info.id.short(), info.role, info.status.as_str().replace('_', " ")));
    match dispatch {
        Some(d) => {
            lines.push(format!("Outcome: {}", clip(d.summary.trim(), RELAY_SUMMARY_CHARS, &info.id)));
            lines.push(format!("Confidence: {}", confidence_word(d)));
        }
        None => {
            let why = info.note.as_deref().unwrap_or("no reason was recorded");
            lines.push(format!("Outcome: the run {} without a result: {why}", info.status.as_str().replace('_', " ")));
        }
    }
    let mut needs = Vec::new();
    if let Some(diff) = dispatch.and_then(|d| d.diff.as_ref()) {
        needs.push(format!(
            "merge approval for {} ({} files, +{} −{}): `marlowe crew merge {}` or m in the Run view; never pushed",
            diff.branch,
            diff.files.len(),
            diff.insertions,
            diff.deletions,
            info.id.short()
        ));
    } else if info.status == CrewRunStatus::NeedsYou {
        needs.push(info.note.clone().unwrap_or_else(|| "a decision in the Run view".into()));
    }
    if dispatch.is_some_and(|d| d.artifacts.iter().any(|a| a.ends_with("report.md"))) && info.playbook.is_some() {
        needs.push(format!("accepting the report copies it into the workspace's research folder (`marlowe crew merge {}`)", info.id.short()));
    }
    if !needs.is_empty() {
        lines.push(format!("Needs the user: {}", needs.join("; ")));
    }
    if let Some(d) = dispatch {
        if !d.artifacts.is_empty() {
            lines.push(format!("Artifacts: {}", d.artifacts.join(", ")));
        }
        if !d.citations.is_empty() {
            lines.push("Citations:".into());
            for c in d.citations.iter().take(RELAY_CITATIONS) {
                lines.push(format!("- {} — {}", c.title, c.url_or_path));
            }
            if d.citations.len() > RELAY_CITATIONS {
                lines.push(format!("- and {} more (`marlowe crew show {}`)", d.citations.len() - RELAY_CITATIONS, info.id.short()));
            }
        }
    }
    lines.push(format!("Goal was: {}", clip(info.goal.trim(), RELAY_GOAL_CHARS, &info.id)));
    lines.push("Tell the user now, briefly: the outcome first, then what needs them, then where the artifacts are.".into());
    lines.join("\n")
}

fn confidence_word(d: &Dispatch) -> &'static str {
    match d.confidence {
        marlowe_crew::Confidence::Low => "low",
        marlowe_crew::Confidence::Medium => "medium",
        marlowe_crew::Confidence::High => "high",
    }
}

fn clip(s: &str, max: usize, run: &RunId) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut t: String = s.chars().take(max).collect();
    t = t.trim_end().to_string();
    t.push_str(&format!("… (the rest: `marlowe crew show {}`)", run.short()));
    t
}

/// A result relays tainted when the run read untrusted content, its role could, or it cites the
/// web.
fn relay_taints(link: &CrewLink, info: &CrewRunInfo, dispatch: Option<&Dispatch>) -> bool {
    let cites_web = dispatch.is_some_and(|d| {
        d.citations.iter().any(|c| c.url_or_path.starts_with("http://") || c.url_or_path.starts_with("https://"))
    });
    let reads_web = link
        .supervisor
        .roles()
        .iter()
        .find(|r| r.name == info.role)
        .map(|r| r.tools.iter().any(|t| UNTRUSTED_TOOLS.contains(&t.as_str())))
        // An unknown role (a card removed since): assume it read the web.
        .unwrap_or(true);
    info.tainted || cites_web || reads_web || info.playbook == Some(marlowe_crew::Playbook::DeepResearch)
}

/// Raises `ApprovalRequested` for a run (or a merge/discard of one) and waits for the answer.
/// Denies on timeout and shutdown. `AllowForSession` counts as `Allow`.
pub(crate) fn ask(core: &Core, run: &RunId, session: Option<&SessionId>, tool: String, summary: String, detail: String) -> ApprovalDecision {
    if core.is_shutting_down() {
        return ApprovalDecision::Deny;
    }
    let (id, _tx, rx) = core.approvals.register();
    if let Some(link) = core.crew.get() {
        lock(&link.pending).entry(run.clone()).or_default().push(id);
    }
    let session = session.cloned().unwrap_or_else(|| orphan_session(run));
    core.bus.publish(Event::ApprovalRequested { approval: id, session, tool, summary, detail, run: Some(run.clone()), proposed_rule: None });
    let decision = rx.recv_timeout(core.cfg.approval_timeout).unwrap_or(ApprovalDecision::Deny);
    core.approvals.remove(id);
    if let Some(link) = core.crew.get() {
        let mut pending = lock(&link.pending);
        if let Some(ids) = pending.get_mut(run) {
            ids.retain(|p| *p != id);
            if ids.is_empty() {
                pending.remove(run);
            }
        }
    }
    core.bus.publish(Event::ApprovalResolved { approval: id, decision });
    match decision {
        ApprovalDecision::AllowForSession => ApprovalDecision::Allow,
        d => d,
    }
}

/// The session an approval names when the run has no parent (dispatched from the CLI).
pub(crate) fn orphan_session(run: &RunId) -> SessionId {
    SessionId(format!("crew-{}", run.short()))
}

/// Run approvals as an `ApprovalRequested { run }` / `Approve` round trip.
struct RunGate {
    core: Weak<Core>,
}

impl RunApprovals for RunGate {
    fn ask(&self, run: &RunId, session: Option<&SessionId>, req: ApprovalReq) -> ApprovalDecision {
        let Some(core) = self.core.upgrade() else { return ApprovalDecision::Deny };
        let session = session.cloned().or_else(|| (!req.session.as_str().is_empty()).then(|| req.session.clone()));
        ask(&core, run, session.as_ref(), req.tool, req.summary, req.detail)
    }
}

/// Busy when the Sentinel sees a watched run alive, when another process computes on a GPU (a
/// training job, another inference server: NVML's compute list minus its graphics list, the same
/// signal the reranker's GPU gate reads), or when graphics load is heavy with no Ollama runner on
/// the device. Ollama is Marlowe's own (crew steps check before every step and must not wait on
/// the model a previous step left loaded), and so is the voice sidecar's process tree
/// ([`marlowe_voice::owns_process`]). No Sentinel, or no GPU reported: not busy.
fn gpu_probe(core: Weak<Core>) -> GpuBusy {
    let nvml = Mutex::new(None::<marlowe_sentinel::gpu::Gpu>);
    Arc::new(move || {
        let Some(core) = core.upgrade() else { return false };
        let Some(s) = core.sentinel.as_ref() else { return false };
        catch_unwind(AssertUnwindSafe(|| {
            let compute = lock(&nvml)
                .get_or_insert_with(marlowe_sentinel::gpu::Gpu::open)
                .compute_only_except(&marlowe_voice::owns_process);
            // A device whose compute processes cannot be read may hold a training job: busy.
            // No NVML at all keeps the no-GPU rule above.
            let unreadable = compute.as_ref().is_ok_and(|c| c.iter().any(|(_, n)| n.is_none()));
            let lists: Vec<(u32, Vec<String>)> =
                compute.unwrap_or_default().into_iter().filter_map(|(i, n)| n.map(|n| (i, n))).collect();
            unreadable || gpu_busy(&s.state(), &lists)
        }))
        .unwrap_or(false)
    })
}

/// `compute`: per device index, other processes doing compute only.
pub(crate) fn gpu_busy(state: &WatchState, compute: &[(u32, Vec<String>)]) -> bool {
    devices_busy(&state.gpu, compute) || state.runs.iter().any(|r| r.status.is_live())
}

fn is_ollama(name: &str) -> bool {
    name.to_ascii_lowercase().starts_with("ollama")
}

fn devices_busy(gpu: &GpuReport, compute: &[(u32, Vec<String>)]) -> bool {
    gpu.devices.iter().any(|d| {
        let on_device: Vec<&String> = compute.iter().filter(|(i, _)| *i == d.index).flat_map(|(_, n)| n).collect();
        let foreign = on_device.iter().any(|n| !is_ollama(n));
        let ollama = on_device.iter().any(|n| is_ollama(n)) || d.processes.iter().any(|p| is_ollama(p));
        foreign || (d.util_pct >= HEAVY_UTIL_PCT && !ollama)
    })
}

/// The conductor's tools plus `dispatch`, when the supervisor is running.
pub(crate) fn with_dispatch(core: &Core, tools: Arc<dyn ToolHost>) -> Arc<dyn ToolHost> {
    match core.crew.get() {
        Some(link) => Arc::new(WithDispatch { inner: tools, dispatch: DispatchTools::new(link.supervisor.clone()) }),
        None => tools,
    }
}

struct WithDispatch {
    inner: Arc<dyn ToolHost>,
    dispatch: DispatchTools,
}

fn is_dispatch(call: &ToolCallReq) -> bool {
    call.name == DISPATCH_TOOL
}

impl ToolHost for WithDispatch {
    fn specs(&self) -> Vec<ToolSpec> {
        let mut specs: Vec<ToolSpec> = self.inner.specs().into_iter().filter(|s| s.name != DISPATCH_TOOL).collect();
        specs.extend(self.dispatch.specs());
        specs
    }

    fn call(&self, call: &ToolCallReq, ctx: &ToolCtx) -> ToolOutcome {
        if is_dispatch(call) {
            match catch_unwind(AssertUnwindSafe(|| self.dispatch.call(call, ctx))) {
                Ok(o) => o,
                Err(p) => ToolOutcome::error(format!("dispatch failed: {}", panic_text(&p))),
            }
        } else {
            self.inner.call(call, ctx)
        }
    }

    fn call_scoped(&self, call: &ToolCallReq, cx: &marlowe_agent::CallCtx) -> ToolOutcome {
        if is_dispatch(call) {
            self.call(call, cx)
        } else {
            self.inner.call_scoped(call, cx)
        }
    }

    fn middleware(&self) -> Vec<Arc<dyn marlowe_agent::ToolMiddleware>> {
        self.inner.middleware()
    }

    fn persona_blocks(&self) -> Vec<marlowe_agent::PersonaBlock> {
        self.inner.persona_blocks()
    }

    fn consequence(&self, call: &ToolCallReq) -> Consequence {
        if is_dispatch(call) {
            self.dispatch.consequence(call)
        } else {
            self.inner.consequence(call)
        }
    }

    fn consequence_exposed(&self, call: &ToolCallReq, exposure: &Exposure) -> Consequence {
        if is_dispatch(call) {
            self.dispatch.consequence_exposed(call, exposure)
        } else {
            self.inner.consequence_exposed(call, exposure)
        }
    }

    fn taints(&self, call: &ToolCallReq) -> bool {
        !is_dispatch(call) && self.inner.taints(call)
    }

    fn egress_host(&self, call: &ToolCallReq) -> Option<String> {
        if is_dispatch(call) {
            None
        } else {
            self.inner.egress_host(call)
        }
    }

    fn manifest(&self, name: &str) -> Option<marlowe_agent::ToolManifest> {
        if name == DISPATCH_TOOL {
            Some(marlowe_agent::ToolManifest {
                group: marlowe_agent::ToolGroup::Crew,
                profiles: marlowe_agent::Profiles::ALL,
                effects: marlowe_agent::Effects::NONE,
                path_args: &[],
            })
        } else {
            self.inner.manifest(name)
        }
    }

    fn mutation(&self, call: &ToolCallReq, ctx: &ToolCtx) -> marlowe_agent::Mutation {
        if is_dispatch(call) {
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
        if is_dispatch(call) {
            None
        } else {
            self.inner.consequence_in_sandbox(call, exposure, sandbox)
        }
    }

    fn describe(&self, call: &ToolCallReq) -> String {
        if is_dispatch(call) {
            self.dispatch.describe(call)
        } else {
            self.inner.describe(call)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use marlowe_crew::{Citation, Confidence, CrewBudget, CrewSpent, DiffSummary, Playbook};
    use marlowe_proto::GpuDevice;

    fn info(status: CrewRunStatus) -> CrewRunInfo {
        CrewRunInfo {
            id: RunId("0123456789abcdef0123456789abcdef".into()),
            session: Some(SessionId("s".into())),
            role: "engineer".into(),
            title: "fix the flaky test".into(),
            goal: "Fix the flaky test in tests/net.rs".into(),
            playbook: Some(Playbook::Build),
            status,
            workspace: None,
            sandbox: None,
            budget: CrewBudget::default(),
            spent: CrewSpent::default(),
            step: 4,
            current_action: None,
            note: Some("the test command failed 4 times".into()),
            tainted: false,
            created_ms: 0,
            updated_ms: 0,
            result: None,
        }
    }

    #[test]
    fn a_relay_leads_with_the_outcome_then_what_needs_the_user() {
        let d = Dispatch {
            summary: "Fixed: the test raced the listener; it now waits for the bind.".into(),
            artifacts: vec![],
            confidence: Confidence::High,
            citations: vec![Citation { title: "tests/net.rs".into(), url_or_path: "tests/net.rs".into() }],
            diff: Some(DiffSummary { branch: "crew/01234567".into(), files: vec!["tests/net.rs".into()], insertions: 3, deletions: 1 }),
        };
        let text = relay_text(&info(CrewRunStatus::NeedsYou), Some(&d));
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "[crew · run 01234567 · engineer · build · needs you]");
        assert!(lines[0].starts_with(RELAY_PREFIX));
        assert!(lines[1].starts_with("Outcome: Fixed: the test raced"), "{text}");
        assert_eq!(lines[2], "Confidence: high");
        assert!(lines[3].starts_with("Needs the user: merge approval for crew/01234567 (1 files, +3 −1)"), "{text}");
        assert!(text.contains("- tests/net.rs — tests/net.rs"));
    }

    #[test]
    fn a_failed_run_relays_its_reason() {
        let text = relay_text(&info(CrewRunStatus::Failed), None);
        assert!(text.lines().nth(1).unwrap().contains("failed without a result: the test command failed 4 times"), "{text}");
    }

    #[test]
    fn long_summaries_are_clipped_with_a_pointer() {
        let id = RunId("abcdef0123".into());
        let s = clip(&"x".repeat(50), 10, &id);
        assert!(s.starts_with("xxxxxxxxxx…") && s.ends_with("`marlowe crew show abcdef01`)"));
    }

    #[test]
    fn the_gpu_is_busy_for_foreign_compute_heavy_graphics_or_a_live_run() {
        let mut st = WatchState::default();
        assert!(!gpu_busy(&st, &[]), "no GPU reported: not busy");
        st.gpu.devices.push(GpuDevice {
            index: 0,
            name: "gpu".into(),
            util_pct: 3,
            mem_used_mb: 100,
            mem_total_mb: 8000,
            temp_c: None,
            processes: vec![],
        });
        assert!(!gpu_busy(&st, &[(0, vec![])]));
        // The desktop (compositor, browsers, a live wallpaper): ~20% and a quarter of the memory.
        st.gpu.devices[0].util_pct = 23;
        st.gpu.devices[0].mem_used_mb = 4_500;
        st.gpu.devices[0].processes = vec!["dwm.exe".into(), "wallpaper64.exe".into()];
        assert!(!gpu_busy(&st, &[(0, vec![])]), "desktop load is not a reason to wait");
        // Heavy graphics with no Ollama runner (a game) waits.
        st.gpu.devices[0].util_pct = 80;
        assert!(gpu_busy(&st, &[(0, vec![])]));
        // Ollama computing (a model a crew step left loaded) is Marlowe's own.
        assert!(!gpu_busy(&st, &[(0, vec!["ollama.exe".into()])]));
        // Current Ollama computes in its own llama-server (labelled by the Sentinel); a llama.cpp
        // server outside Ollama's install is someone else's.
        assert!(!gpu_busy(&st, &[(0, vec!["ollama:llama-server.exe".into()])]));
        assert!(gpu_busy(&st, &[(0, vec!["llama-server.exe".into()])]));
        // A training job waits however idle the device looks.
        st.gpu.devices[0].util_pct = 2;
        assert!(gpu_busy(&st, &[(0, vec!["python.exe".into()])]));
        assert!(gpu_busy(&st, &[(0, vec!["ollama.exe".into(), "python.exe".into()])]));
        // Compute on a device the Sentinel does not report is not counted against this one.
        assert!(!gpu_busy(&st, &[(1, vec!["python.exe".into()])]));
    }
}
