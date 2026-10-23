//! One connected client: a reader thread (requests) and a writer thread (its event queue).

use std::io::{BufReader, BufWriter, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crossbeam_channel::{bounded, Receiver};
use marlowe_proto::{auth, write_line, Envelope, Event, LineReader, Request, PROTO_VERSION};

use crate::bus::{ClientLink, Line};
use crate::daemon::{Core, VERSION};

/// Serves one connection until it closes, is dropped, or the daemon shuts down.
pub(crate) fn serve(core: Arc<Core>, stream: TcpStream) {
    let _ = stream.set_nodelay(true);
    let Ok(read_half) = stream.try_clone() else { return };
    // Timeouts go on the handle that is read: on Windows a `try_clone` is a separately duplicated
    // socket and does not share the original's SO_RCVTIMEO.
    let _ = read_half.set_read_timeout(Some(core.cfg.hello_timeout));
    let mut reader = LineReader::new(BufReader::new(read_half));
    let (since, name) = match reader.next::<Request>() {
        Ok(Some(Request::Hello { token, since_seq, client })) if auth::token_matches(&core.token, &token) => (since_seq, client),
        _ => return refuse(stream),
    };
    if reader.get_ref().get_ref().set_read_timeout(None).is_err() {
        return;
    }

    let id = core.next_client_id();
    let (tx, rx) = bounded::<Line>(core.cfg.client_queue.max(core.bus.min_client_queue()));
    let dropped = Arc::new(AtomicBool::new(false));
    let Ok(wsock) = stream.try_clone() else { return };
    let _ = wsock.set_write_timeout(Some(core.cfg.write_timeout));
    let wcore = core.clone();
    let wdropped = dropped.clone();
    match std::thread::Builder::new()
        .name("marlowe-client-writer".into())
        .spawn(move || writer(wcore, id, rx, wsock, wdropped))
    {
        Ok(h) => core.add_writer(h),
        Err(_) => return,
    }
    let link = ClientLink { id, tx, dropped: dropped.clone(), sock: stream.try_clone().ok() };
    let home = core.home.display().to_string();
    let welcome = |seq| Event::Welcome { daemon_version: VERSION.into(), proto_version: PROTO_VERSION, profile_home: home, seq };
    if !core.bus.attach(link, welcome, since) {
        return;
    }
    core.bus.reply(id, core.user.event());
    crate::mind_ops::offer_legacy(&core, id);
    crate::brief::on_hello(&core, id, &name);
    crate::close_ops::client_attached(&core, id, &name);

    loop {
        let next = reader.next::<Request>();
        if dropped.load(Ordering::SeqCst) || !core.bus.is_attached(id) {
            break;
        }
        match next {
            Ok(Some(req)) => dispatch(&core, id, &name, req),
            Ok(None) => break,
            Err(e) if e.is_fatal() => break,
            Err(e) => core.bus.reply(id, Event::Error { session: None, message: e.to_string() }),
        }
    }
    core.bus.detach(id);
    crate::brief::on_goodbye(&core, &name);
    crate::close_ops::client_gone(&core, id, &name);
}

/// Answers an unauthenticated connection and closes it.
fn refuse(mut stream: TcpStream) {
    let env = Envelope { seq: 0, at_ms: crate::now_ms(), event: Event::Error { session: None, message: auth::REFUSED.into() } };
    let _ = write_line(&mut stream, &env);
    let _ = stream.shutdown(Shutdown::Both);
}

fn writer(core: Arc<Core>, id: u64, rx: Receiver<Line>, sock: TcpStream, dropped: Arc<AtomicBool>) {
    {
        let mut w = BufWriter::with_capacity(64 * 1024, &sock);
        while let Ok(line) = rx.recv() {
            if dropped.load(Ordering::SeqCst) {
                break;
            }
            let wrote = w.write_all(line.as_bytes()).and_then(|_| w.write_all(b"\n"));
            if wrote.is_err() || (rx.is_empty() && w.flush().is_err()) {
                dropped.store(true, Ordering::SeqCst);
                core.bus.detach(id);
                break;
            }
        }
        let _ = w.flush();
    }
    let _ = sock.shutdown(Shutdown::Both);
}

fn dispatch(core: &Arc<Core>, client: u64, client_name: &str, req: Request) {
    let reply = |event: Event| core.bus.reply(client, event);
    let refuse = |(session, message): (Option<marlowe_proto::SessionId>, String)| reply(Event::Error { session, message });
    match req {
        Request::Hello { .. } => reply(Event::Error { session: None, message: "already greeted".into() }),
        // Images (stored first with `PutBlob`) join the user's message; `marlowe -p`'s options
        // apply to the turn.
        Request::Ask { session, text, workspace, images, options, client_ms } => {
            // The turn's clock starts at the client's Enter; this is the daemon's side of it.
            let extras = crate::session::AskExtras {
                images,
                options,
                received: Some(std::time::Instant::now()),
                sending_ms: client_ms.map(|c| (crate::now_ms() - c).clamp(0, 10_000) as u64),
            };
            if let Err(r) = core.ask_with(client, session, text, workspace, extras) {
                refuse(r);
            }
        }
        Request::Interrupt { session } => core.interrupt(&session),
        Request::ListSessions { limit } => match core.store.list(limit) {
            Ok(sessions) => reply(Event::SessionList { sessions }),
            Err(e) => refuse((None, e)),
        },
        Request::OpenSession { session } => {
            if let Err(r) = core.open_session(client, &session) {
                refuse(r);
            }
        }
        Request::NewSession { workspace } => {
            if let Err(r) = core.create_session(client, workspace) {
                refuse(r);
            }
        }
        Request::Approve { approval, decision, reason } => {
            if !core.approvals.resolve_with(approval, decision, reason) {
                refuse((None, format!("approval {approval} is not pending")));
            }
        }
        Request::Status => reply(Event::Status(core.status())),
        Request::SetUser { name, address } => {
            if let Err(m) = crate::user::set(core, name, address) {
                refuse((None, m));
            }
        }
        Request::Shutdown => core.request_shutdown(),
        lib @ (Request::LibraryState
        | Request::DetectProviders
        | Request::ListCatalog { .. }
        | Request::AddModel { .. }
        | Request::RemoveModel { .. }
        | Request::SetSlot { .. }
        | Request::SetCredential { .. }) => crate::library::spawn(core.clone(), client, lib),
        mem @ (Request::MemorySearch { .. }
        | Request::MemoryForget { .. }
        | Request::MemoryCorrect { .. }
        | Request::MemoryStats
        | Request::ImportLegacy { .. }
        | Request::LegacyStatus
        | Request::MemoryProbe { .. }
        | Request::MemoryProfile { .. }) => crate::mind_ops::dispatch(core, client, mem),
        watch @ (Request::Brief
        | Request::WatchList
        | Request::WatchAdd { .. }
        | Request::WatchRemove { .. }
        | Request::WatchCandidates
        | Request::ListNotices { .. }
        | Request::NoticeSeen { .. }
        | Request::Speak { .. }
        | Request::StopSpeaking
        | Request::VoiceSettings { .. }) => crate::watch_ops::dispatch(core, client, client_name, watch),
        crew @ (Request::Dispatch { .. }
        | Request::ListRuns { .. }
        | Request::WatchRun { .. }
        | Request::Steer { .. }
        | Request::CancelRun { .. }
        | Request::PauseRun { .. }
        | Request::ResumeRun { .. }
        | Request::MergeRun { .. }
        | Request::DiscardRun { .. }
        | Request::ListRoles
        | Request::ApprovePlan { .. }) => crate::crew_ops::dispatch(core, client, crew),
        ctx @ (Request::ContextPlan { .. }
        | Request::ContextPolicies
        | Request::SetContextPolicy { .. }
        | Request::SetModelEngine { .. }
        | Request::ApplyOllamaServer { .. }) => crate::context_ops::dispatch(core, client, ctx),
        // M4.6: one arm per owning track (design section 7). The stubs answer `Error` until the
        // track fills its `*_ops.rs`; no track edits this match.
        mode @ (Request::SetMode { .. }
        | Request::ListRules { .. }
        | Request::AddRule { .. }
        | Request::RemoveRule { .. }
        | Request::ListHooks { .. }
        | Request::TrustHooks { .. }
        | Request::GetSettings { .. }
        | Request::SetSetting { .. }) => crate::mode_ops::dispatch(core, client, mode),
        teams @ (Request::SetTeams { .. } | Request::GetTeams { .. }) => crate::teams_ops::dispatch(core, client, teams),
        cond @ (Request::Compact { .. }
        | Request::AddDir { .. }
        | Request::SwitchModel { .. }
        | Request::ListCommands { .. }
        | Request::ListAgentTypes { .. }
        | Request::AnswerQuestion { .. }
        | Request::ReloadInstructions { .. }
        | Request::DetachCall { .. }) => crate::conductor_ops::dispatch(core, client, cond),
        sched @ (Request::ListSchedules | Request::AddSchedule { .. } | Request::RemoveSchedule { .. }) => {
            crate::schedule_ops::dispatch(core, client, sched)
        }
        sess @ (Request::Rewind { .. }
        | Request::ListCheckpoints { .. }
        | Request::SessionDiff { .. }
        | Request::SessionCost { .. }
        | Request::RenameSession { .. }
        | Request::ForkSession { .. }
        | Request::ExportSession { .. }) => crate::session_ops::dispatch(core, client, sess),
        pr @ Request::OpenPr { .. } => crate::pr_ops::dispatch(core, client, pr),
        exp @ Request::ListExperiments { .. } => crate::exp_ops::dispatch(core, client, exp),
        mcp @ (Request::McpList { .. } | Request::McpAdd { .. } | Request::McpRemove { .. }) => {
            crate::mcp_ops::dispatch(core, client, mcp)
        }
        blob @ Request::PutBlob { .. } => crate::blob_ops::dispatch(core, client, blob),
        job @ (Request::ListJobs { .. } | Request::KillJob { .. } | Request::UserShell { .. }) => {
            crate::job_ops::dispatch(core, client, job)
        }
        // M4.6 wave 2 (seam2): same rule, one arm per owning track.
        task @ (Request::ListTasks { .. }
        | Request::StopTask { .. }
        | Request::TaskOutput { .. }
        | Request::CronCreate { .. }) => crate::task_ops::dispatch(core, client, task),
        notify @ (Request::NotifySettings { .. } | Request::OpenFile { .. }) => {
            crate::notify_ops::dispatch(core, client, notify)
        }
        flow @ (Request::ListWorkflows
        | Request::WatchWorkflow { .. }
        | Request::RunWorkflow { .. }
        | Request::SkipWorkflowAgent { .. }
        | Request::ListFindings { .. }) => crate::workflow_ops::dispatch(core, client, flow),
        // Closing the window (N): the daemon's own lifetime.
        close @ (Request::WindowClosed
        | Request::Detach { .. }
        | Request::ResumeInterrupted { .. }
        | Request::KeepRunning { .. }) => crate::close_ops::dispatch(core, client, close),
        f3 @ (Request::SetFast { .. } | Request::AnswerPlan { .. } | Request::SuggestRules { .. }) => {
            crate::f3_ops::dispatch(core, client, f3)
        }
    }
}

/// The answer a seam stub gives: an `Error` to this client naming the request, its session (if
/// any) and the track that will implement it.
pub(crate) fn not_yet(core: &Arc<Core>, client: u64, req: &Request, track: &str) {
    let v = serde_json::to_value(req).unwrap_or_default();
    let op = v.get("op").and_then(|o| o.as_str()).unwrap_or("request").to_string();
    let session = v.get("session").and_then(|s| s.as_str()).map(|s| marlowe_proto::SessionId(s.to_string()));
    let message = format!("`{op}` is not available yet (M4.6 track {track})");
    core.bus.reply(client, Event::Error { session, message });
}
