//! `monitor` (BG4): run a command and hear about each line it prints.
//!
//! The command starts through B's job runner (Job Object, scrubbed environment, kill-on-close;
//! the shell's rules and tripwire decide the call, as for `bash`) but as a hidden job: it is not
//! a `job_*` task and its exit queues no job notice. The monitor is its own task, `mon_<id>`,
//! and a thread follows its output:
//!
//! - every complete line goes to the monitor's output file (`<home>\jobs\mon_<id>.log`, the whole
//!   of it, however fast it comes) and, at most 50 per event, into the task registry as an
//!   [`Injection`]. The daemon hands an event to the model at its next step boundary, or starts
//!   a turn when the session is idle. A flood is batched: the event says how many lines it left
//!   out and where they are.
//! - it ends when the command exits or, unless `persistent`, after `timeout_ms` (default 5
//!   minutes, at most an hour). The final event says which.
//! - `task_stop` kills the command and the thread stops quietly.
//! - the same `id` again re-arms a finished monitor: the command runs again, its lines are
//!   numbered on from where the last run stopped, and the output file keeps growing.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use marlowe_agent::{
    CallCtx, Injection, InjectionSource, NewTask, TaskControl, TaskKind, TaskRegistry, TaskStatus, Tasks, ToolOutcome,
};
use marlowe_os::workspace::ExecRequest;
use serde_json::Value;

use crate::jobs::{HostJobs, Job, Launch, BG_LOG_CAP};
use crate::shell;
use crate::{first_line, int_arg, paths, str_arg, BuiltinTools};

/// Longest a non-persistent monitor runs unless the call says otherwise.
pub const DEFAULT_TIMEOUT_MS: u64 = 300_000;
/// Longest `timeout_ms` accepted.
pub const MAX_TIMEOUT_MS: u64 = 3_600_000;
/// Lines one event carries.
pub const MAX_LINES_PER_EVENT: usize = 50;
/// A line longer than this is cut in the event (the output file keeps it whole).
const MAX_LINE_CHARS: usize = 500;
/// How often the thread looks for output.
const POLL: Duration = Duration::from_millis(100);

/// What a host remembers of its monitors (for re-arming).
#[derive(Default)]
pub(crate) struct Monitors {
    map: Mutex<HashMap<String, Armed>>,
}

struct Armed {
    command: String,
    /// Lines delivered so far, over every run of this id.
    lines: u64,
    running: bool,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// A monitor id the model gave, made safe for a file name and a task id.
fn clean_id(raw: &str) -> Option<String> {
    let id: String = raw.trim().trim_start_matches("mon_").chars().take(40).collect();
    (!id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))).then_some(id)
}

struct MonitorControl {
    stop: Arc<AtomicBool>,
    job: Arc<Job>,
}

impl TaskControl for MonitorControl {
    fn stop(&self) -> Result<(), String> {
        self.stop.store(true, Ordering::SeqCst);
        self.job.kill();
        Ok(())
    }
}

/// The `monitor` tool.
pub(crate) fn start(tools: &BuiltinTools, args: &Value, cx: &CallCtx) -> ToolOutcome {
    let Some(tasks) = cx.scope.services.get::<Tasks>() else {
        return ToolOutcome::error("monitors need the daemon's task registry, which this session does not have");
    };
    let registry: Arc<dyn TaskRegistry> = tasks.0.clone();
    let monitors = tools.state::<Monitors>();

    let given = str_arg(args, "id").map(str::trim).filter(|s| !s.is_empty());
    let raw_id = match given {
        Some(g) => match clean_id(g) {
            Some(id) => id,
            None => return ToolOutcome::error("`id` may hold letters, digits, `-` and `_` (40 at most)"),
        },
        None => format!("m{}", NEXT.fetch_add(1, Ordering::Relaxed)),
    };
    let task_id = TaskKind::Monitor.id(&raw_id);

    // Re-arming reuses the command when none is given.
    let (command, first_line_no) = {
        let map = lock(&monitors.map);
        let prior = map.get(&raw_id);
        if prior.is_some_and(|p| p.running) {
            return ToolOutcome::error(format!("{task_id} is still armed; task_stop it first, or give a different id"));
        }
        let command = str_arg(args, "command").map(str::trim).filter(|c| !c.is_empty()).map(str::to_string);
        match (command, prior) {
            (Some(c), p) => (c, p.map_or(0, |p| p.lines)),
            (None, Some(p)) => (p.command.clone(), p.lines),
            (None, None) => return ToolOutcome::error("`command` is required (re-arming an id reuses its earlier command)"),
        }
    };
    let persistent = args.get("persistent").and_then(Value::as_bool).unwrap_or(false);
    let timeout_ms = int_arg(args, "timeout_ms").filter(|m| *m > 0).map(|m| (m as u64).min(MAX_TIMEOUT_MS)).unwrap_or(DEFAULT_TIMEOUT_MS);
    let description = str_arg(args, "label").map(str::trim).filter(|d| !d.is_empty()).map(str::to_string);

    // The same working directory rules as `bash`.
    let root = tools.shell_root(cx);
    let dir = match str_arg(args, "cwd") {
        Some(c) => paths::resolve(root.as_deref(), c),
        None => root.clone().unwrap_or_else(|| paths::resolve(None, ".")),
    };
    if !dir.is_dir() {
        return ToolOutcome::error(format!("cwd {} is not a directory", dir.display()));
    }
    if let Some(pin) = tools.shell_dir.as_deref().filter(|p| !paths::inside(p, &dir)) {
        return ToolOutcome::error(format!("cwd {} is outside the sandbox {}; commands run inside it", dir.display(), pin.display()));
    }
    let cmd = match shell::bash_command(&command) {
        Ok(c) => c,
        Err(e) => return ToolOutcome::error(e.to_string()),
    };
    let mut req = ExecRequest::from_command(&cmd);
    req.spec.env_remove.extend(tools.scrub.iter().cloned());
    req.spec.env.extend(tools.child_env.iter().cloned());
    req.spec.cwd = Some(dir);

    let jobs = tools.state::<HostJobs>();
    jobs.bind(tools);
    let session = cx.session.0.clone();
    jobs.note_session(&session);
    let job = match jobs.launch(
        tools.workspace_port(),
        Launch {
            req,
            display: command.clone(),
            session: session.clone(),
            call_id: None,
            foreground: false,
            cap: BG_LOG_CAP,
            user: false,
            hidden: true,
            leftovers: tools.leftovers,
            timeout: None,
        },
    ) {
        Ok(j) => j,
        Err(e) => return ToolOutcome::error(format!("could not start the monitor: {e}")),
    };

    let out_dir = jobs.dir();
    let _ = std::fs::create_dir_all(&out_dir);
    let out_file = out_dir.join(format!("{task_id}.log"));
    let stop = Arc::new(AtomicBool::new(false));
    let title = description.clone().unwrap_or_else(|| first_line(&command, 80));
    let registered = registry.register(
        NewTask::new(TaskKind::Monitor, title.clone())
            .raw_id(raw_id.clone())
            .session(Some(cx.session.clone()))
            .output_file(out_file.to_string_lossy())
            .control(Arc::new(MonitorControl { stop: stop.clone(), job: job.clone() })),
    );
    debug_assert_eq!(registered, task_id);
    lock(&monitors.map).insert(raw_id.clone(), Armed { command: command.clone(), lines: first_line_no, running: true });

    let pump = Pump {
        job,
        registry,
        monitors: monitors.clone(),
        task_id: task_id.clone(),
        raw_id,
        session: cx.session.clone(),
        out_file: out_file.clone(),
        stop,
        deadline: (!persistent).then(|| Instant::now() + Duration::from_millis(timeout_ms)),
        timeout_ms,
        line_no: first_line_no,
        command,
    };
    let spawned = std::thread::Builder::new().name(format!("marlowe-{task_id}")).spawn(move || pump.run());
    if let Err(e) = spawned {
        return ToolOutcome::error(format!("could not start the monitor's thread: {e}"));
    }
    let ends = if persistent {
        "It stays armed across turns until it exits or you stop it with task_stop.".to_string()
    } else {
        format!("It ends when the command exits or after {} s, and you will be told which.", timeout_ms / 1000)
    };
    ToolOutcome::ok(
        format!(
            "Started monitor {task_id}: {title}. Each line it prints reaches you as an event between your steps (or \
             starts a turn when you are idle); do not poll. {ends} Its whole output is in {}.",
            out_file.display()
        ),
        format!("{task_id} started"),
    )
}

static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

struct Pump {
    job: Arc<Job>,
    registry: Arc<dyn TaskRegistry>,
    monitors: Arc<Monitors>,
    task_id: String,
    raw_id: String,
    session: marlowe_proto::SessionId,
    out_file: std::path::PathBuf,
    stop: Arc<AtomicBool>,
    deadline: Option<Instant>,
    timeout_ms: u64,
    line_no: u64,
    command: String,
}

impl Pump {
    fn event(&self, text: String) {
        self.registry.inject(Injection::new(self.session.clone(), InjectionSource::Monitor, text).task(self.task_id.clone()));
    }

    fn run(mut self) {
        let mut offset = 0u64;
        let mut carry: Vec<u8> = Vec::new();
        let mut out = OpenOptions::new().create(true).append(true).open(&self.out_file).ok();
        let started = Instant::now();
        let end = loop {
            // The exit first, then the log: the exit is recorded after the output has been copied
            // in, so a read that follows it sees every line (the other order can miss the last).
            let exited = self.job.exit_code();
            let more = self.job.read_range(offset, 1 << 20);
            offset += more.len() as u64;
            carry.extend_from_slice(&more);
            // After the exit, whatever is left unterminated is a last line.
            let all_read = exited.is_some() && more.is_empty();
            let lines = take_lines(&mut carry, all_read);
            if !lines.is_empty() {
                if let Some(f) = out.as_mut() {
                    for l in &lines {
                        let _ = writeln!(f, "{l}");
                    }
                }
                self.deliver(&lines);
            }
            if self.stop.load(Ordering::SeqCst) {
                break End::Stopped;
            }
            if let Some(code) = exited {
                if all_read {
                    break End::Exited(code);
                }
                continue;
            }
            if self.deadline.is_some_and(|d| Instant::now() >= d) {
                self.job.kill();
                break End::TimedOut;
            }
            self.job.wait_tick(POLL);
        };
        // A kill or a stop is a moment, not a boundary: what the command printed before it still
        // belongs in the file, and a timeout still reports it.
        if matches!(end, End::TimedOut | End::Stopped) {
            self.job.wait_exit(Duration::from_millis(500));
            loop {
                let more = self.job.read_range(offset, 1 << 20);
                if more.is_empty() {
                    break;
                }
                offset += more.len() as u64;
                carry.extend_from_slice(&more);
            }
            let lines = take_lines(&mut carry, true);
            if !lines.is_empty() {
                if let Some(f) = out.as_mut() {
                    for l in &lines {
                        let _ = writeln!(f, "{l}");
                    }
                }
                if matches!(end, End::TimedOut) {
                    self.deliver(&lines);
                } else {
                    self.line_no += lines.len() as u64;
                }
            }
        }
        lock(&self.monitors.map).entry(self.raw_id.clone()).and_modify(|a| {
            a.running = false;
            a.lines = self.line_no;
        });
        let took = crate::jobs::fmt_secs(started.elapsed());
        match end {
            End::Stopped => {}
            End::Exited(code) => {
                self.event(format!(
                    "[monitor {} ended: `{}` exited with code {code} after {took}; {} lines in all, in {}]",
                    self.task_id,
                    first_line(&self.command, 80),
                    self.line_no,
                    self.out_file.display()
                ));
                let status = if code == 0 { TaskStatus::Done } else { TaskStatus::Failed };
                self.registry.set_status(&self.task_id, status, Some(format!("exit {code}")));
            }
            End::TimedOut => {
                self.event(format!(
                    "[monitor {} timed out after {} s with {} lines; it is no longer watching. Re-arm it with monitor \
                     {{\"id\":\"{}\"}}, or persistent:true to keep it armed]",
                    self.task_id,
                    self.timeout_ms / 1000,
                    self.line_no,
                    self.raw_id
                ));
                self.registry.set_status(&self.task_id, TaskStatus::Stopped, Some(format!("timed out after {} s", self.timeout_ms / 1000)));
            }
        }
    }

    /// One event for these lines: at most [`MAX_LINES_PER_EVENT`] of them.
    fn deliver(&mut self, lines: &[String]) {
        let first = self.line_no + 1;
        self.line_no += lines.len() as u64;
        let shown = lines.len().min(MAX_LINES_PER_EVENT);
        let mut text = format!("[monitor {} · lines {first}-{}]", self.task_id, self.line_no);
        for l in &lines[..shown] {
            text.push('\n');
            text.push_str(&clip_line(l));
        }
        if lines.len() > shown {
            text.push_str(&format!(
                "\n[... {} more lines not shown; all of them are in {}]",
                lines.len() - shown,
                self.out_file.display()
            ));
        }
        self.event(text);
    }
}

enum End {
    Exited(i32),
    TimedOut,
    Stopped,
}

fn clip_line(l: &str) -> String {
    if l.chars().count() <= MAX_LINE_CHARS {
        return l.to_string();
    }
    let cut: String = l.chars().take(MAX_LINE_CHARS).collect();
    format!("{cut}…")
}

/// Removes the complete lines from `carry` (all of it when `flush`, so an unterminated last line
/// is delivered), cleaned of control characters and carriage returns.
pub(crate) fn take_lines(carry: &mut Vec<u8>, flush: bool) -> Vec<String> {
    let end = if flush { carry.len() } else { carry.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1) };
    if end == 0 {
        return Vec::new();
    }
    let chunk: Vec<u8> = carry.drain(..end).collect();
    let text = String::from_utf8_lossy(&chunk);
    let mut lines: Vec<String> = text
        .split('\n')
        .map(|l| crate::summarize::clean(l.trim_end_matches('\r')))
        .collect();
    // `split` leaves an empty tail after a final newline.
    if lines.last().is_some_and(|l| l.is_empty()) && chunk.last() == Some(&b'\n') {
        lines.pop();
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complete_lines_only_until_the_flush() {
        let mut carry = b"one\ntwo\r\nthr".to_vec();
        assert_eq!(take_lines(&mut carry, false), ["one", "two"]);
        assert_eq!(carry, b"thr");
        assert!(take_lines(&mut carry, false).is_empty());
        carry.extend_from_slice(b"ee\n");
        assert_eq!(take_lines(&mut carry, false), ["three"]);
        carry.extend_from_slice(b"tail");
        assert_eq!(take_lines(&mut carry, true), ["tail"]);
        assert!(carry.is_empty());
    }

    #[test]
    fn ids_are_file_name_safe() {
        assert_eq!(clean_id("mon_deploy"), Some("deploy".into()));
        assert_eq!(clean_id(" a-b_9 "), Some("a-b_9".into()));
        assert_eq!(clean_id("../x"), None);
        assert_eq!(clean_id(""), None);
        assert_eq!(clean_id("mon_"), None);
    }

    #[test]
    fn long_lines_are_cut_in_the_event() {
        let long = "x".repeat(900);
        let c = clip_line(&long);
        assert_eq!(c.chars().count(), MAX_LINE_CHARS + 1);
        assert!(c.ends_with('…'));
        assert_eq!(clip_line("short"), "short");
    }
}
