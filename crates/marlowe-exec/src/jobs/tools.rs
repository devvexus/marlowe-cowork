//! The tool-facing half of the jobs: running a shell call as a foreground or background job,
//! `job_output`, `job_watch`, `job_kill`, and the middleware that delivers notices.

use std::cell::RefCell;
use std::sync::Arc;
use std::time::{Duration, Instant};

use marlowe_agent::{CallCtx, CallFlow, Conversation, StepInfo, ToolCallReq, ToolMiddleware, ToolOutcome, TurnFlow};
use marlowe_model::Message;
use marlowe_os::workspace::ExecRequest;
use marlowe_os::JobObject;
use regex::Regex;
use serde_json::Value;

use super::{fmt_secs, HostJobs, Job, Launch, BG_LOG_CAP};
use crate::shell::ShellLimits;
use crate::summarize;
use crate::{int_arg, str_arg, BuiltinTools};

thread_local! {
    /// The id of the shell call this thread is about to run, set by [`JobsMiddleware::before_call`]
    /// (the tool table does not hand the call id to a tool) so Ctrl+B can name it.
    static CURRENT_CALL: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// The call id for the shell tool running on this thread, once.
pub(crate) fn take_current_call() -> Option<String> {
    CURRENT_CALL.with(|c| c.borrow_mut().take())
}

fn set_current_call(id: Option<String>) {
    CURRENT_CALL.with(|c| *c.borrow_mut() = id);
}

/// How a foreground call ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FgEnd {
    Exited,
    TimedOut,
    Cancelled,
    Flooded,
    /// The user moved it to the background; the job runs on as `FgRun::job`.
    Detached,
}

/// A finished (or detached) foreground call.
pub(crate) struct FgRun {
    pub end: FgEnd,
    pub code: i32,
    /// The job's output so far: raw, or (a big log) condensed to its head, tail and failure
    /// blocks with `total_lines` giving the true line count.
    pub text: String,
    pub total_lines: Option<usize>,
    pub log: std::path::PathBuf,
    pub job: String,
    /// Processes the command left running (settled by the caller per the host's policy).
    pub leftover: Option<JobObject>,
}

/// Runs the command as a job and waits for it: until it exits, the timeout, an interrupt, the
/// output cap, or the user detaches it.
pub(crate) fn run_foreground(
    tools: &BuiltinTools,
    jobs: &HostJobs,
    cx: &CallCtx,
    req: ExecRequest,
    display: String,
    limits: ShellLimits,
) -> std::io::Result<FgRun> {
    let session = cx.session.0.clone();
    jobs.note_session(&session);
    let job = jobs.launch(
        tools.workspace_port(),
        Launch {
            req,
            display,
            session,
            call_id: take_current_call(),
            foreground: true,
            cap: limits.max_output_bytes as u64,
            user: false,
            hidden: false,
            leftovers: tools.leftovers,
            timeout: None,
        },
    )?;
    let deadline = Instant::now() + Duration::from_millis(limits.timeout_ms);
    let end = loop {
        if job.exit_code().is_some() {
            break FgEnd::Exited;
        }
        if job.detach_requested() {
            break FgEnd::Detached;
        }
        if job.flooded() {
            break FgEnd::Flooded;
        }
        if cx.cancel.is_cancelled() {
            break FgEnd::Cancelled;
        }
        if Instant::now() >= deadline {
            break FgEnd::TimedOut;
        }
        job.wait_tick(Duration::from_millis(15));
    };
    // A detach can lose the race with an exit: the exit wins, the call answers for the job.
    let end = if end == FgEnd::Detached && job.exit_code().is_some() { FgEnd::Exited } else { end };
    if matches!(end, FgEnd::TimedOut | FgEnd::Cancelled | FgEnd::Flooded) {
        job.kill();
    }
    let code = if end == FgEnd::Detached { -1 } else { job.wait_exit(Duration::from_secs(5)).unwrap_or(-1) };
    let failed = code != 0 || end != FgEnd::Exited;
    let (text, total_lines) = job.summary_source(failed);
    let leftover = match end {
        FgEnd::Exited => job.take_proc().and_then(|p| p.into_leftover()),
        _ => None,
    };
    if end != FgEnd::Detached {
        jobs.remove(&job.id);
    }
    Ok(FgRun { end, code, text, total_lines, log: job.log.clone(), job: job.id.clone(), leftover })
}

/// Starts the command as a background job and answers at once with its id.
pub(crate) fn run_background(
    tools: &BuiltinTools,
    jobs: &HostJobs,
    cx: &CallCtx,
    req: ExecRequest,
    display: String,
    timeout: Option<Duration>,
) -> std::io::Result<ToolOutcome> {
    let session = cx.session.0.clone();
    jobs.note_session(&session);
    let job = jobs.launch(
        tools.workspace_port(),
        Launch {
            req,
            display: display.clone(),
            session,
            call_id: take_current_call(),
            foreground: false,
            cap: BG_LOG_CAP,
            user: false,
            hidden: false,
            leftovers: tools.leftovers,
            timeout,
        },
    )?;
    // A command that fails at once (a typo, a missing tool) should say so in this result.
    let settle = Instant::now() + Duration::from_millis(300);
    while Instant::now() < settle && job.exit_code().is_none() {
        job.wait_tick(Duration::from_millis(20));
    }
    let id = &job.id;
    let mut text = format!("started background job {id}: `{display}`\noutput is logged to {}\n", slash(&job.log));
    match job.exit_code() {
        Some(code) => {
            jobs.forget_notices(id);
            let (raw, _) = job.summary_source(code != 0);
            let out = summarize::clean(&raw);
            let body = summarize::summarize(&out, code != 0, 3_000);
            text.push_str(&format!("it has already exited with code {code}:\n{}", if body.text.is_empty() { "(no output)" } else { &body.text }));
            return Ok(ToolOutcome { content: text, is_error: code != 0, summary: format!("{id} · exit {code}"), parts: Vec::new() });
        }
        None if jobs.delivers_notices() => text.push_str(&format!(
            "you will be told when it exits. job_output {{\"id\":\"{id}\"}} reads its output, job_watch {{\"id\":\"{id}\",\"until_regex\":\"...\"}} \
             tells you when a line appears, job_kill {{\"id\":\"{id}\"}} stops it."
        )),
        None => text.push_str(&format!(
            "you will NOT be told when it exits in this session: poll with job_output {{\"id\":\"{id}\"}}. job_kill \
             {{\"id\":\"{id}\"}} stops it."
        )),
    }
    Ok(ToolOutcome::ok(text, format!("{id} running")))
}

fn slash(p: &std::path::Path) -> String {
    p.display().to_string().replace('\\', "/")
}

/// The job `args.id` names, if it belongs to the calling session.
fn own_job(tools: &BuiltinTools, args: &Value, cx: &CallCtx) -> Result<(Arc<HostJobs>, Arc<Job>), ToolOutcome> {
    let jobs = tools.state::<HostJobs>();
    let Some(id) = str_arg(args, "id").map(str::trim).filter(|i| !i.is_empty()) else {
        return Err(ToolOutcome::error("`id` is required: the job id a background `bash` call returned, e.g. \"j3\""));
    };
    match jobs.find(id).filter(|j| j.session == cx.session.0) {
        Some(j) => Ok((jobs, j)),
        None => {
            let known: Vec<String> = jobs
                .all()
                .iter()
                .filter(|j| j.session == cx.session.0)
                .map(|j| match j.exit_code() {
                    Some(c) => format!("{} (exited {c})", j.id),
                    None => format!("{} (running)", j.id),
                })
                .collect();
            let list = if known.is_empty() { "no jobs have been started in this session".to_string() } else { format!("jobs: {}", known.join(", ")) };
            Err(ToolOutcome::error(format!("no job `{id}` here; {list}")))
        }
    }
}

const NO_NOTICES: &str = "this session does not receive job notices between steps; poll with job_output";

fn status(job: &Job, next: u64) -> String {
    let len = job.len();
    match job.exit_code() {
        None => format!("[job {} running {} · {len} bytes so far · next since={next}]", job.id, fmt_secs(job.started.elapsed())),
        Some(c) => format!("[job {} exited {c} · {len} bytes · next since={next}]", job.id),
    }
}

/// Output the window of one explicit `since` read.
const WINDOW: usize = 30_000;

/// Reads `job_output {id, since, filter}`.
pub fn job_output(tools: &BuiltinTools, args: &Value, cx: &CallCtx) -> ToolOutcome {
    let (_, job) = match own_job(tools, args, cx) {
        Ok(j) => j,
        Err(e) => return e,
    };
    let filter = match str_arg(args, "filter").filter(|f| !f.is_empty()) {
        Some(f) => match Regex::new(f) {
            Ok(r) => Some(r),
            Err(e) => return ToolOutcome::error(format!("`filter` is not a valid regular expression: {e}")),
        },
        None => None,
    };
    let explicit = match args.get("since") {
        None | Some(Value::Null) => None,
        Some(_) => match int_arg(args, "since") {
            Some(n) if n >= 0 => Some(n as u64),
            _ => return ToolOutcome::error("`since` must be a byte offset, 0 or more (the `next since` of the last read)"),
        },
    };
    let len = job.len();
    let from = explicit.unwrap_or_else(|| job.cursor()).min(len);
    let failed = job.exit_code().is_some_and(|c| c != 0);
    let lines_of = |bytes: &[u8]| -> String {
        let text = summarize::clean(&String::from_utf8_lossy(bytes));
        match &filter {
            Some(f) => text.split('\n').filter(|l| f.is_match(l)).collect::<Vec<_>>().join("\n"),
            None => text,
        }
    };
    let (text, next, abridged) = if explicit.is_some() {
        // A stream read: exactly the bytes from `since`, a window at a time, cut at a line end.
        let mut raw = job.read_range(from, WINDOW);
        if from + (raw.len() as u64) < len {
            if let Some(nl) = raw.iter().rposition(|b| *b == b'\n') {
                raw.truncate(nl + 1);
            }
        }
        let next = from + raw.len() as u64;
        (lines_of(&raw), next, false)
    } else {
        // Everything new since the last read; a flood is summarised and the log holds the rest.
        let raw = job.read_range(from, 8 << 20);
        let next = from + raw.len() as u64;
        let text = lines_of(&raw);
        if text.len() > summarize::BUDGET {
            let s = summarize::summarize(&text, failed, summarize::BUDGET);
            (s.text, next, true)
        } else {
            (text, next, false)
        }
    };
    if explicit.is_none() {
        job.set_cursor(next);
    }
    let mut out = if text.is_empty() {
        match (&filter, job.exit_code()) {
            (Some(_), _) => "(no lines match `filter` in this range)".to_string(),
            (None, None) => "(no new output)".to_string(),
            (None, Some(_)) => "(no new output; the job has exited)".to_string(),
        }
    } else {
        text
    };
    if abridged {
        let total = summarize::clean(&String::from_utf8_lossy(&job.read_range(from, 8 << 20))).split('\n').count();
        out.push_str(&format!("\n(abridged) {}", summarize::pointer(&job.log, total)));
    } else if next < len && explicit.is_some() {
        out.push_str(&format!("\n[{} more bytes after this window]", len - next));
    }
    out.push('\n');
    out.push_str(&status(&job, next));
    let summary = format!("{} · {}", job.id, if job.exit_code().is_some() { "exited" } else { "running" });
    ToolOutcome::ok(out, summary)
}

/// Starts `job_watch {id, until_regex, filter}`.
pub fn job_watch(tools: &BuiltinTools, args: &Value, cx: &CallCtx) -> ToolOutcome {
    let (jobs, job) = match own_job(tools, args, cx) {
        Ok(j) => j,
        Err(e) => return e,
    };
    let re = |key: &str| -> Result<Option<(Regex, String)>, ToolOutcome> {
        match str_arg(args, key).filter(|f| !f.is_empty()) {
            Some(f) => Regex::new(f)
                .map(|r| Some((r, f.to_string())))
                .map_err(|e| ToolOutcome::error(format!("`{key}` is not a valid regular expression: {e}"))),
            None => Ok(None),
        }
    };
    let (until, filter) = match (re("until_regex"), re("filter")) {
        (Ok(u), Ok(f)) => (u, f),
        (Err(e), _) | (_, Err(e)) => return e,
    };
    if let Some(code) = job.exit_code() {
        return ToolOutcome::ok(
            format!("job {} has already exited with code {code}; job_output {{\"id\":\"{}\"}} reads what it wrote.", job.id, job.id),
            format!("{} · exited", job.id),
        );
    }
    if !jobs.delivers_notices() {
        return ToolOutcome::error(format!("job_watch cannot work here: {NO_NOTICES} {{\"id\":\"{}\"}}", job.id));
    }
    let until_src = until.as_ref().map(|(_, s)| s.clone()).unwrap_or_default();
    if let Some(line) = job.watch(filter.map(|(r, _)| r), until.map(|(r, _)| r), until_src.clone(), true) {
        return ToolOutcome::ok(
            format!(
                "already matched /{until_src}/ in output you have not read: {line}
no watch was started; job {} is still running.",
                job.id
            ),
            format!("{} matched", job.id),
        );
    }
    let what = match (until_src.is_empty(), str_arg(args, "filter").filter(|f| !f.is_empty())) {
        (false, Some(f)) => format!("lines matching /{f}/, until a line matches /{until_src}/"),
        (false, None) => format!("every line, until one matches /{until_src}/"),
        (true, Some(f)) => format!("lines matching /{f}/"),
        (true, None) => "every line".to_string(),
    };
    ToolOutcome::ok(
        format!(
            "watching job {}: {what}. They arrive as notices between your steps while you work, and the job's exit is reported \
             too. You do not need to poll.",
            job.id
        ),
        format!("{} watched", job.id),
    )
}

/// Stops `job_kill {id}`.
pub fn job_kill(tools: &BuiltinTools, args: &Value, cx: &CallCtx) -> ToolOutcome {
    let (jobs, job) = match own_job(tools, args, cx) {
        Ok(j) => j,
        Err(e) => return e,
    };
    if let Some(code) = job.exit_code() {
        return ToolOutcome::ok(format!("job {} had already exited with code {code}", job.id), format!("{} · exited", job.id));
    }
    job.kill();
    let code = job.wait_exit(Duration::from_secs(5));
    // The model asked for this; it does not also need a notice.
    jobs.forget_notices(&job.id);
    ToolOutcome::ok(
        format!(
            "stopped job {} (it ran {}; the process tree is gone). Its output is at {}.",
            job.id,
            fmt_secs(job.started.elapsed()),
            slash(&job.log)
        ),
        format!("{} killed{}", job.id, code.map_or(String::new(), |c| format!(" · {c}"))),
    )
}

/// Delivers job notices to the model and tells Ctrl+B which call is a shell call.
pub(crate) struct JobsMiddleware {
    pub jobs: Arc<HostJobs>,
}

impl JobsMiddleware {
    fn reminder(&self, session: &str) -> Option<Message> {
        let notices = self.jobs.drain(session);
        if notices.is_empty() {
            return None;
        }
        let texts: Vec<String> = notices.into_iter().map(|n| n.text).collect();
        Some(Message::user(texts.join("\n\n")))
    }
}

fn is_shell_tool(name: &str) -> bool {
    matches!(name, "bash" | "powershell" | "python")
}

impl ToolMiddleware for JobsMiddleware {
    fn name(&self) -> &str {
        "jobs"
    }

    fn before_turn(&self, convo: &mut Conversation, _text: &mut String) -> TurnFlow {
        self.jobs.note_session(&convo.session.0);
        // Anything that happened between turns (a job exited while the session was idle and no
        // daemon woke it, or a `!` command's output) goes in ahead of the user's message.
        if let Some(m) = self.reminder(&convo.session.0) {
            convo.push(m);
        }
        TurnFlow::Continue
    }

    fn before_call(&self, call: &ToolCallReq, _cx: &CallCtx) -> CallFlow {
        set_current_call(is_shell_tool(&call.name).then(|| call.id.clone()));
        CallFlow::Continue
    }

    fn after_call(&self, call: &ToolCallReq, _cx: &CallCtx, _outcome: &mut ToolOutcome) {
        if is_shell_tool(&call.name) {
            set_current_call(None);
        }
    }

    fn after_step(&self, _info: StepInfo, convo: &mut Conversation) -> Vec<Message> {
        self.jobs.note_session(&convo.session.0);
        self.reminder(&convo.session.0).into_iter().collect()
    }
}
