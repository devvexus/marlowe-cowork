//! The session's working set (design 2.1 T22-T23, 2.4 C7, 2.10 B2-B4), kept in
//! `Conversation::ext`:
//! - [`TodoState`] behind the `todo` tool, broadcast through the [`TodoSink`] service; a
//!   reminder when open todos go [`TODO_STALE_STEPS`] steps without an update.
//! - `ask_user` through the [`Questions`] service (the daemon's modal; refused where nobody can
//!   answer, so a headless run continues on its own judgement).
//! - [`PlanState`] for `exit_plan`.
//! - [`Pinned`]: the edit ledger and the last failing output, which with the todos, the plan
//!   and the user's own words form the pinned working set that compaction never drops
//!   ([`pinned_block`]).
//!
//! [`WorkingSet`] is the loop middleware: it records edits, checks and failures as calls run,
//! reminds about stale todos after a step, and before the final answer applies the stop check
//! ("you changed files and ran no check since") and keeps a turn with open todos going.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use marlowe_model::{CancelToken, Message, Part, Role};
use marlowe_proto::{Question, QuestionOption, SessionId, TodoItem, TodoStatus};
use serde_json::Value;

use crate::convo::Conversation;
use crate::middleware::{FinalFlow, StepInfo, ToolMiddleware};
use crate::tools::{CallCtx, Mutation, ToolCallReq, ToolOutcome};

/// Steps with open todos and no `todo` update before the reminder.
pub const TODO_STALE_STEPS: u32 = 10;
/// Times one turn is pushed on because todos are still open.
pub const MAX_TODO_NUDGES: u32 = 2;
/// Longest single user message carried in the pinned working set, and all of them together
/// (the first message and the most recent ones are kept when they do not all fit).
const PINNED_USER_CHARS: usize = 600;
const PINNED_USER_TOTAL: usize = 6_000;
const PINNED_FAILURE_CHARS: usize = 2_000;

/// The todo list and how fresh it is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TodoState {
    pub items: Vec<TodoItem>,
    /// Steps since the last `todo` call.
    pub steps_since_update: u32,
    /// The step count at which the last stale reminder was sent (no repeat until it moves on).
    pub reminded: bool,
}

impl TodoState {
    pub fn open(&self) -> Vec<&TodoItem> {
        self.items.iter().filter(|i| i.status != TodoStatus::Completed).collect()
    }
}

/// Renders a todo list, one line per item.
pub fn render_todos(items: &[TodoItem]) -> String {
    items
        .iter()
        .map(|i| {
            let mark = match i.status {
                TodoStatus::Pending => "[ ]",
                TodoStatus::InProgress => "[~]",
                TodoStatus::Completed => "[x]",
            };
            let id = i.id.as_deref().map(|x| format!("#{x} ")).unwrap_or_default();
            let mut notes: Vec<String> = Vec::new();
            if let Some(o) = &i.owner {
                notes.push(format!("owner {o}"));
            }
            if !i.blocked_by.is_empty() {
                notes.push(format!("blocked by {}", i.blocked_by.iter().map(|b| format!("#{b}")).collect::<Vec<_>>().join(", ")));
            }
            let notes = if notes.is_empty() { String::new() } else { format!(" ({})", notes.join("; ")) };
            format!("{mark} {id}{}{notes}", i.content.trim())
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Broadcasts a session's todo list (the daemon publishes `Event::Todos`).
#[derive(Clone)]
pub struct TodoSink(pub Arc<dyn Fn(&SessionId, &[TodoItem]) + Send + Sync>);

/// Parses the `todo` tool's items (`items`, or Claude Code's `todos`).
pub fn parse_todos(args: &Value) -> Result<Vec<TodoItem>, String> {
    let list = args.get("items").or_else(|| args.get("todos")).and_then(Value::as_array).ok_or("`items` must be an array of {content, status, active_form}")?;
    let mut out = Vec::new();
    for (n, it) in list.iter().enumerate() {
        let content = it.get("content").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());
        let content = content.ok_or_else(|| format!("item {}: `content` is required", n + 1))?;
        let status = match it.get("status").and_then(Value::as_str).unwrap_or("pending") {
            "pending" => TodoStatus::Pending,
            "in_progress" | "in-progress" => TodoStatus::InProgress,
            "completed" | "done" => TodoStatus::Completed,
            other => return Err(format!("item {}: status `{other}` is not pending, in_progress or completed", n + 1)),
        };
        let active_form = it.get("active_form").or_else(|| it.get("activeForm")).and_then(Value::as_str).map(str::to_string);
        out.push(TodoItem { content: content.to_string(), status, active_form, ..Default::default() });
    }
    Ok(out)
}

/// The `todo` tool: replaces the list in the session's extensions and broadcasts it.
pub fn todo_call(args: &Value, cx: &CallCtx) -> ToolOutcome {
    let items = match parse_todos(args) {
        Ok(i) => i,
        Err(e) => return ToolOutcome::error(e),
    };
    // The conductor's list is the shared task list (T34): it keeps item ids, owners and blockers
    // across a rewrite and is what subagents update. A subagent's `todo` stays its own.
    let shared = crate::tasklist::is_conductor(&cx.session).then(|| cx.scope.services.get::<crate::tasklist::TaskLists>()).flatten();
    let items = match shared {
        Some(lists) => lists.replace(&cx.session, items),
        None => items,
    };
    cx.scope.with_ext(|e| {
        let st = e.get_or_default::<TodoState>();
        st.items = items.clone();
        st.steps_since_update = 0;
        st.reminded = false;
    });
    // The shared list announces its own changes.
    if shared.is_none() {
        if let Some(sink) = cx.scope.services.get::<TodoSink>() {
            (sink.0)(&cx.session, &items);
        }
    }
    let open = items.iter().filter(|i| i.status != TodoStatus::Completed).count();
    let doing = items.iter().filter(|i| i.status == TodoStatus::InProgress).count();
    let mut msg = format!("Todo list updated ({} items, {open} open).", items.len());
    if doing > 1 {
        msg.push_str(" More than one item is in progress; keep exactly one in progress at a time.");
    }
    if open > 0 {
        msg.push_str(" Keep it current: mark each item completed as soon as it is done.");
    }
    ToolOutcome::ok(msg, format!("{} todos, {open} open", items.len()))
}

/// Answers `ask_user` questions: the daemon shows a modal and waits for `AnswerQuestion`.
pub trait QuestionAsker: Send + Sync {
    /// One answer per question, in order; `Err` when nobody answered (timeout, cancel).
    fn ask(&self, session: &SessionId, questions: &[Question], cancel: &CancelToken) -> Result<Vec<String>, String>;
}

/// The `ask_user` service in `Services`.
#[derive(Clone)]
pub struct Questions(pub Arc<dyn QuestionAsker>);

/// What `ask_user` returns where nobody can answer (headless, crew).
pub const NO_ONE_TO_ASK: &str = "No one can answer questions in this session. Do not ask again: make the most \
reasonable choice, state the assumption in your reply, and continue.";

/// Parses `ask_user` questions (Claude Code's `AskUserQuestion` shape: `question`, `header`,
/// `options` as strings or `{label, description}`, `multiSelect`).
pub fn parse_questions(args: &Value) -> Result<Vec<Question>, String> {
    let list = args.get("questions").and_then(Value::as_array).ok_or("`questions` must be an array")?;
    if list.is_empty() || list.len() > 4 {
        return Err("ask 1 to 4 questions at a time".into());
    }
    let mut out = Vec::new();
    for (n, q) in list.iter().enumerate() {
        let question = q.get("question").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());
        let question = question.ok_or_else(|| format!("question {}: `question` is required", n + 1))?;
        let header = q.get("header").and_then(Value::as_str).unwrap_or("").chars().take(24).collect();
        // Options are labels, or `{label, description, preview}` (T33): the labels go in
        // `options`, always, and the rest in `option_details` when any option has more.
        let mut details: Vec<QuestionOption> = Vec::new();
        if let Some(opts) = q.get("options").and_then(Value::as_array) {
            for o in opts {
                match o {
                    Value::String(s) if !s.trim().is_empty() => details.push(QuestionOption { label: s.trim().to_string(), ..Default::default() }),
                    Value::Object(_) => {
                        let Some(label) = o.get("label").and_then(Value::as_str).map(str::trim).filter(|l| !l.is_empty()) else { continue };
                        let text = |k: &str| o.get(k).and_then(Value::as_str).map(str::trim).filter(|d| !d.is_empty()).map(str::to_string);
                        details.push(QuestionOption {
                            label: label.to_string(),
                            description: text("description").unwrap_or_default(),
                            preview: text("preview"),
                        });
                    }
                    _ => {}
                }
            }
        }
        let options: Vec<String> = details.iter().map(|d| d.label.clone()).collect();
        let has_more = details.iter().any(|d| !d.description.is_empty() || d.preview.is_some());
        let option_details = if has_more { details } else { Vec::new() };
        let multi_select = q.get("multi_select").or_else(|| q.get("multiSelect")).and_then(Value::as_bool).unwrap_or(false);
        out.push(Question { question: question.to_string(), header, options, multi_select, option_details });
    }
    Ok(out)
}

/// Whether `answer` is free text ("Other") rather than a pick: some part of it (a multi-select
/// answer is its labels joined with `, ` or on separate lines) is not one of the question's
/// options. A question with no options is all free text.
fn own_words(q: &Question, answer: &str) -> bool {
    if q.options.is_empty() || answer.is_empty() || answer == NO_ANSWER {
        return false;
    }
    let is_label = |s: &str| q.options.iter().any(|o| o.trim().eq_ignore_ascii_case(s.trim()));
    if is_label(answer) {
        return false;
    }
    if q.multi_select {
        let parts: Vec<&str> = answer.split(['\n', ';']).flat_map(|p| p.split(", ")).collect();
        return !parts.iter().all(|p| is_label(p));
    }
    true
}

/// What a client sends for a question nobody answered.
const NO_ANSWER: &str = "(no answer)";

/// The `ask_user` tool.
pub fn ask_user_call(args: &Value, cx: &CallCtx) -> ToolOutcome {
    let questions = match parse_questions(args) {
        Ok(q) => q,
        Err(e) => return ToolOutcome::error(e),
    };
    let Some(asker) = cx.scope.services.get::<Questions>() else {
        return ToolOutcome { summary: "no one to ask".into(), ..ToolOutcome::error(NO_ONE_TO_ASK) };
    };
    match asker.0.ask(&cx.session, &questions, &cx.cancel) {
        Ok(answers) => {
            let pairs: Vec<String> = questions
                .iter()
                .zip(answers.iter().map(String::as_str).chain(std::iter::repeat("(no answer)")))
                .map(|(q, a)| {
                    let a = a.trim();
                    let own = if own_words(q, a) { " (their own words, not one of the options)" } else { "" };
                    format!("\"{}\" = \"{}\"{own}", q.question, a)
                })
                .collect();
            ToolOutcome::ok(
                format!("The user answered: {}. Continue with these answers in mind.", pairs.join("; ")),
                format!("{} answered", questions.len()),
            )
        }
        Err(e) => ToolOutcome::error(format!("The question was not answered ({e}). {NO_ONE_TO_ASK}")),
    }
}

/// The approved plan (`exit_plan`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlanState {
    pub plan: Option<String>,
    /// Set when the user approved leaving plan mode with it (the mode switch is track E's).
    pub approved: bool,
}

/// The edit ledger and the last failure.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pinned {
    /// Files changed by tools this session.
    pub edits: BTreeSet<PathBuf>,
    /// `(call id, tool, output excerpt)` of the last call that failed.
    pub last_failure: Option<(String, String, String)>,
}

/// Whether files changed since the last check (build, test, lint) ran, and if the stop check
/// already fired this turn.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VerifyState {
    pub dirty: BTreeSet<PathBuf>,
    pub reminded_turn: Option<u32>,
    /// `(turn, nudges)` for open-todo continuations.
    pub nudges: (u32, u32),
}

/// Commands that check work (a build, test, lint or type check). A shell call matching one
/// clears the stop check.
const GATE_COMMANDS: &[&str] = &[
    "cargo test", "cargo check", "cargo build", "cargo clippy", "cargo nextest", "cargo-idle.ps1", "pytest", "py.test",
    "unittest", "tox", "nox", "npm test", "npm run test", "npm run build", "npm run lint", "npm run check",
    "npm run typecheck", "pnpm test", "pnpm run", "yarn test", "yarn build", "yarn lint", "bun test", "deno test", "npx tsc", "tsc ",
    "jest", "vitest", "eslint", "go test", "go build", "go vet", "ruff", "mypy", "pyright", "make", "ctest", "cmake --build",
    "dotnet test", "dotnet build", "mvn ", "gradle", "swift test", "swift build", "rspec", "phpunit",
];

/// Whether a shell command runs a check.
pub fn is_gate_command(cmd: &str) -> bool {
    let c = cmd.to_ascii_lowercase();
    GATE_COMMANDS.iter().any(|g| c.contains(g)) || c.trim() == "tsc"
}

/// Files that name a project's checks: the stop check reminds only where there is something to run.
const PROJECT_MARKERS: [&str; 16] = [
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "setup.py",
    "setup.cfg",
    "requirements.txt",
    "go.mod",
    "Makefile",
    "CMakeLists.txt",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "deno.json",
    "tsconfig.json",
    "justfile",
    ".marlowe/project.toml",
];

/// The workspace has a build, test or check to run (a project file at its root).
fn has_checks(root: &Path) -> bool {
    PROJECT_MARKERS.iter().any(|m| root.join(m).is_file())
}

/// A change a build or test could check: documentation, notes and logs are not.
fn checkable(p: &Path) -> bool {
    let ext = p.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase);
    !matches!(ext.as_deref(), Some("md" | "markdown" | "txt" | "rst" | "log" | "csv" | "tsv"))
}

fn is_gate_call(call: &ToolCallReq) -> bool {
    match call.name.as_str() {
        // The verify group's project runner (track D) is a check by definition.
        "project" => true,
        _ => ["command", "code", "script"].iter().filter_map(|k| call.args.get(*k).and_then(Value::as_str)).any(is_gate_command),
    }
}

/// The user's own words from before earlier compactions, so they outlive any number of them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CarriedWords(pub Vec<String>);

/// The pinned working set, rendered for a compaction summary (empty when there is nothing).
pub fn pinned_block(convo: &Conversation) -> String {
    render_pinned(convo, &all_user_words(convo))
}

/// [`pinned_block`] for the compaction about to run: the user's words are also carried to the
/// next one, since the messages holding them are about to be folded away.
pub fn pin_for_compaction(convo: &mut Conversation) -> String {
    let words = all_user_words(convo);
    let block = render_pinned(convo, &words);
    convo.ext.insert(CarriedWords(words));
    block
}

fn all_user_words(convo: &Conversation) -> Vec<String> {
    let mut all = convo.ext.get::<CarriedWords>().map(|c| c.0.clone()).unwrap_or_default();
    for w in user_words(&convo.messages) {
        if !all.contains(&w) {
            all.push(w);
        }
    }
    let total: usize = all.iter().map(|w| w.chars().count()).sum();
    if total <= PINNED_USER_TOTAL || all.len() < 2 {
        return all;
    }
    // The first message (the task) and then the most recent ones that fit.
    let first = all.remove(0);
    let mut used = first.chars().count();
    let mut recent: Vec<String> = Vec::new();
    for w in all.into_iter().rev() {
        let n = w.chars().count();
        if used + n > PINNED_USER_TOTAL {
            break;
        }
        used += n;
        recent.push(w);
    }
    recent.reverse();
    std::iter::once(first).chain(recent).collect()
}

fn render_pinned(convo: &Conversation, users: &[String]) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(t) = convo.ext.get::<TodoState>().filter(|t| !t.items.is_empty()) {
        parts.push(format!("Todos:
{}", render_todos(&t.items)));
    }
    if let Some(p) = convo.ext.get::<PlanState>().and_then(|p| p.plan.as_deref()) {
        parts.push(format!("Approved plan:
{}", p.trim()));
    }
    if let Some(pin) = convo.ext.get::<Pinned>() {
        if !pin.edits.is_empty() {
            let files: Vec<String> = pin.edits.iter().map(|p| p.display().to_string()).collect();
            parts.push(format!("Files changed this session:
{}", files.join("
")));
        }
        if let Some((_, tool, out)) = &pin.last_failure {
            parts.push(format!("Last failing output ({tool}):
{}", out.trim()));
        }
    }
    if !users.is_empty() {
        let lines: Vec<String> = users.iter().map(|u| format!("- {u}")).collect();
        parts.push(format!("The user's own words this session, verbatim:
{}", lines.join("
")));
    }
    if parts.is_empty() {
        return String::new();
    }
    format!("<pinned_working_set>
{}
</pinned_working_set>", parts.join("

"))
}

/// The user's own messages (not reminders, relays or tool results), most recent last.
fn user_words(messages: &[Message]) -> Vec<String> {
    let out: Vec<String> = messages
        .iter()
        .filter(|m| m.role == Role::User)
        .filter_map(|m| {
            let t: String = m
                .content
                .iter()
                .filter_map(|p| match p {
                    Part::Text { text } if !is_harness_text(text) => Some(text.trim()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(" ");
            let t = t.trim();
            (!t.is_empty()).then(|| {
                let mut s: String = t.chars().take(PINNED_USER_CHARS).collect();
                if t.chars().count() > PINNED_USER_CHARS {
                    s.push('…');
                }
                s
            })
        })
        .collect();
    out
}

fn is_harness_text(t: &str) -> bool {
    let t = t.trim_start();
    t.starts_with("<system-reminder>") || t.starts_with("<reminder>") || t.starts_with("[crew · run ") || t.starts_with("[agent ")
}

/// Call ids whose results compaction must not mask.
pub fn pinned_ids(convo: &Conversation) -> HashSet<String> {
    convo.ext.get::<Pinned>().and_then(|p| p.last_failure.as_ref()).map(|(id, _, _)| id.clone()).into_iter().collect()
}

/// The working-set middleware (see the module docs).
#[derive(Debug, Default)]
pub struct WorkingSet;

impl WorkingSet {
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
    }
}

impl ToolMiddleware for WorkingSet {
    fn name(&self) -> &str {
        "working-set"
    }

    fn pre_mutation(&self, _call: &ToolCallReq, cx: &CallCtx, mutation: &Mutation) -> Result<(), ToolOutcome> {
        if let Mutation::Paths(paths) = mutation {
            cx.scope.with_ext(|e| {
                e.get_or_default::<Pinned>().edits.extend(paths.iter().cloned());
                e.get_or_default::<VerifyState>().dirty.extend(paths.iter().cloned());
            });
        }
        Ok(())
    }

    fn after_call(&self, call: &ToolCallReq, cx: &CallCtx, outcome: &mut ToolOutcome) {
        let gate = is_gate_call(call);
        cx.scope.with_ext(|e| {
            if outcome.is_error {
                let excerpt: String = outcome.content.chars().rev().take(PINNED_FAILURE_CHARS).collect::<Vec<_>>().into_iter().rev().collect();
                e.get_or_default::<Pinned>().last_failure = Some((call.id.clone(), call.name.clone(), excerpt));
            } else if gate {
                // A passing check after the last failure: the failure is history now.
                if let Some(p) = e.get_mut::<Pinned>() {
                    p.last_failure = None;
                }
            }
            if gate {
                e.get_or_default::<VerifyState>().dirty.clear();
            }
        });
    }

    fn after_step(&self, _info: StepInfo, convo: &mut Conversation) -> Vec<Message> {
        let Some(st) = convo.ext.get_mut::<TodoState>() else { return Vec::new() };
        st.steps_since_update += 1;
        if st.open().is_empty() || st.reminded || st.steps_since_update < TODO_STALE_STEPS {
            return Vec::new();
        }
        st.reminded = true;
        vec![Message::user(format!(
            "<system-reminder>\nThe todo list has not been updated in {} steps. If items are done, mark them completed; if \
             the plan changed, rewrite the list. Current list:\n{}\n</system-reminder>",
            st.steps_since_update,
            render_todos(&st.items)
        ))]
    }

    fn before_final(&self, info: StepInfo, convo: &mut Conversation, _text: &str) -> FinalFlow {
        let project = convo.workspace.as_deref().is_some_and(has_checks);
        let vs = convo.ext.get_or_default::<VerifyState>();
        let unchecked: Vec<&PathBuf> = vs.dirty.iter().filter(|p| checkable(p)).collect();
        if project && !unchecked.is_empty() && vs.reminded_turn != Some(info.turn) {
            vs.reminded_turn = Some(info.turn);
            let files: Vec<String> = unchecked.iter().take(8).map(|p| p.display().to_string()).collect();
            return FinalFlow::Continue(format!(
                "<system-reminder>\nYou changed files ({}) and have not run a build, test or check since. Run the project's \
                 checks and fix what fails before you finish, or say plainly why they cannot be run.\n</system-reminder>",
                files.join(", ")
            ));
        }
        let open: Vec<String> = convo
            .ext
            .get::<TodoState>()
            .map(|t| t.open().iter().map(|i| format!("- {}", i.content)).collect())
            .unwrap_or_default();
        if open.is_empty() {
            return FinalFlow::Accept;
        }
        let vs = convo.ext.get_or_default::<VerifyState>();
        if vs.nudges.0 != info.turn {
            vs.nudges = (info.turn, 0);
        }
        if vs.nudges.1 >= MAX_TODO_NUDGES {
            return FinalFlow::Accept;
        }
        vs.nudges.1 += 1;
        FinalFlow::Continue(format!(
            "<system-reminder>\nThese todos are still open:\n{}\nKeep working on them. If one is done, mark it completed; if \
             it no longer applies or you are blocked, update the list and say what is blocking.\n</system-reminder>",
            open.join("\n")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn todos_parse_both_spellings_and_reject_bad_status() {
        let a = parse_todos(&json!({"todos": [{"content": "x", "status": "in_progress", "activeForm": "Doing x"}]})).unwrap();
        assert_eq!(a[0].status, TodoStatus::InProgress);
        assert_eq!(a[0].active_form.as_deref(), Some("Doing x"));
        assert!(parse_todos(&json!({"items": [{"content": "x", "status": "maybe"}]})).is_err());
    }

    #[test]
    fn questions_take_claude_code_options() {
        let q = parse_questions(&json!({"questions": [{"question": "Which DB?", "header": "DB", "options": [{"label": "sqlite", "description": "local"}, "postgres"], "multiSelect": true}]})).unwrap();
        assert_eq!(q[0].options, ["sqlite", "postgres"]);
        assert_eq!(q[0].option_details[0].description, "local");
        assert!(q[0].multi_select);
        assert!(parse_questions(&json!({"questions": []})).is_err());
        assert!(parse_questions(&json!({"questions": [{"question": "a"}, {"question": "b"}, {"question": "c"}, {"question": "d"}, {"question": "e"}]})).is_err());
    }

    #[test]
    fn plain_labels_leave_option_details_empty_so_old_clients_see_the_same_question() {
        let q = parse_questions(&json!({"questions": [{"question": "Which?", "options": ["a", {"label": "b"}]}]})).unwrap();
        assert_eq!(q[0].options, ["a", "b"]);
        assert!(q[0].option_details.is_empty());
        assert!(serde_json::to_string(&q[0]).unwrap().find("option_details").is_none());
    }

    #[derive(Default)]
    struct Canned {
        answers: Vec<String>,
        seen: std::sync::Mutex<Vec<Question>>,
    }

    impl QuestionAsker for Canned {
        fn ask(&self, _s: &SessionId, questions: &[Question], _c: &CancelToken) -> Result<Vec<String>, String> {
            *self.seen.lock().unwrap() = questions.to_vec();
            Ok(self.answers.clone())
        }
    }

    fn ask_cx(asker: Arc<Canned>) -> CallCtx {
        use crate::tools::{SessionScope, ToolCtx};
        let tool = ToolCtx::new(SessionId("s".into()), 0, None, CancelToken::new(), false);
        CallCtx::new(tool, SessionScope { ext: Default::default(), services: crate::Services::new().with(Questions(asker)) })
    }

    #[test]
    fn parity_t33_descriptions_previews_multi_select_and_other_round_trip() {
        let asker = Arc::new(Canned { answers: vec!["sqlite, postgres".into(), "a third way".into(), "yes".into()], ..Default::default() });
        let cx = ask_cx(asker.clone());
        let args = json!({"questions": [
            {"question": "Which stores?", "header": "Stores", "multiSelect": true, "options": [
                {"label": "sqlite", "description": "embedded", "preview": "CREATE TABLE t (id INTEGER);"},
                {"label": "postgres", "description": "server"}]},
            {"question": "Which layout?", "options": [{"label": "flat", "preview": "src/\n  a.rs"}, {"label": "nested"}]},
            {"question": "Proceed?", "options": ["yes", "no"]}]});
        let out = ask_user_call(&args, &cx);
        assert!(!out.is_error, "{}", out.content);
        // The asker (the daemon's modal) gets every field.
        let seen = asker.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 3);
        assert_eq!(seen[0].options, ["sqlite", "postgres"]);
        assert_eq!(seen[0].option_details[0].preview.as_deref(), Some("CREATE TABLE t (id INTEGER);"));
        assert_eq!(seen[1].option_details[0].preview.as_deref(), Some("src/\n  a.rs"));
        assert!(seen[0].multi_select);
        // Two picks come back as two labels; typed text comes back verbatim and is marked.
        assert!(out.content.contains("\"Which stores?\" = \"sqlite, postgres\""), "{}", out.content);
        assert!(!out.content.contains("\"sqlite, postgres\" (their own"), "a multi-select of labels is not free text");
        assert!(out.content.contains("\"Which layout?\" = \"a third way\" (their own words"), "{}", out.content);
        assert!(out.content.contains("\"Proceed?\" = \"yes\"") && !out.content.contains("\"yes\" (their"), "{}", out.content);
    }

    #[test]
    fn headless_the_question_json_carries_every_field() {
        let q = parse_questions(&json!({"questions": [{"question": "Which?", "options": [{"label": "a", "description": "d", "preview": "p"}]}]})).unwrap();
        let wire = serde_json::to_value(&q[0]).unwrap();
        assert_eq!(wire["option_details"][0], json!({"label": "a", "description": "d", "preview": "p"}));
        assert_eq!(wire["multi_select"], json!(false));
    }

    #[test]
    fn a_shared_list_item_renders_with_its_id_owner_and_blockers() {
        let items = vec![
            TodoItem { content: "design".into(), status: TodoStatus::Completed, id: Some("1".into()), ..Default::default() },
            TodoItem { content: "build".into(), id: Some("2".into()), owner: Some("worker".into()), blocked_by: vec!["1".into()], ..Default::default() },
            TodoItem { content: "plain".into(), ..Default::default() },
        ];
        assert_eq!(render_todos(&items), "[x] #1 design\n[ ] #2 build (owner worker; blocked by #1)\n[ ] plain");
    }

    #[test]
    fn gate_commands_are_recognised() {
        assert!(is_gate_command("cargo test -p x"));
        assert!(is_gate_command("python -m pytest -q"));
        assert!(is_gate_command("powershell -NoProfile -File scripts/cargo-idle.ps1 check"));
        assert!(!is_gate_command("git status"));
        assert!(!is_gate_command("ls -la"));
    }
}
