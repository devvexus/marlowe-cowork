//! `WorkflowIntent` (conductor hosts only): when the user's message asks for a workflow or a
//! fan-out, the turn gets a short reminder in its tail with the exact short-form call, and the
//! harness answers what a local model does instead:
//!
//! - `before_turn`: deterministic detection ([`detect`]), then a `<system-reminder>` pushed into
//!   `convo.pending_context`, so it rides in this turn's user message. The system prompt, the tool
//!   list and every earlier message are untouched (cache-safe).
//! - `before_approval`: in a reminded turn with no workflow call yet, the first batch of background
//!   `agent` / `dispatch` calls is answered "Not run: ..." with the call filled from their prompt.
//!   Once per turn: a second batch runs (the user may mean it). Nothing is converted silently.
//! - `before_final`: one nudge with the exact call when the turn would end in prose with no call
//!   and the last paragraph asks no question.

use std::path::Path;

use marlowe_agent::{CallCtx, CallFlow, Conversation, FinalFlow, StepInfo, ToolMiddleware, ToolOutcome, TurnFlow};
use marlowe_model::{Message, ToolCall};
use serde_json::{json, Value};

use super::items;

/// What the request asks the short form to fan out over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Over {
    /// `items_from` with this selector (`folders`, `packages`, `commits:5`).
    From(&'static str),
    /// A topic: the model names the angles in `items`.
    Topic,
}

/// One reminded turn's state, in `Conversation::ext`.
#[derive(Debug, Clone, Default)]
struct Turn {
    reminded: bool,
    over: Option<Over>,
    /// The user's request, clipped, for the nudge's prompt.
    request: String,
    /// A `workflow` call was made this turn.
    workflow_called: bool,
    /// Any `workflow`, `agent` or `dispatch` call was made this turn.
    launched: bool,
    /// The first batch was answered; later batches run.
    redirected: bool,
    /// The current batch is being answered (becomes `redirected` at the step's end).
    redirecting: bool,
    nudged: bool,
}

#[derive(Debug, Default)]
pub struct WorkflowIntent;

// ---------------------------------------------------------------- detection

/// Removes `<system-reminder>` blocks, fenced code and inline code.
fn strip(text: &str) -> String {
    fn cut(s: &str, open: &str, close: &str) -> String {
        let mut out = String::new();
        let mut rest = s;
        while let Some(i) = rest.find(open) {
            out.push_str(&rest[..i]);
            let after = &rest[i + open.len()..];
            match after.find(close) {
                Some(j) => rest = &after[j + close.len()..],
                None => return out,
            }
        }
        out.push_str(rest);
        out
    }
    let s = cut(text, "<system-reminder>", "</system-reminder>");
    let s = cut(&s, "```", "```");
    cut(&s, "`", "`")
}

fn words(text: &str) -> Vec<String> {
    text.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_string).collect()
}

const COUNTS: [&str; 16] =
    ["two", "three", "four", "five", "six", "seven", "eight", "several", "few", "multiple", "many", "2", "3", "4", "5", "6"];
const LAUNCH: [&str; 14] = [
    "spawn", "run", "start", "launch", "use", "kick", "fire", "create", "make", "do", "execute", "trigger", "set", "build",
];
const INTERROGATIVE: [&str; 15] =
    ["what", "why", "how", "when", "where", "which", "who", "whose", "explain", "describe", "is", "are", "does", "did", "was"];
const STOPPING: [&str; 8] = ["cancel", "stop", "kill", "abort", "pause", "resume", "status", "progress"];

/// Whether `text` asks the conductor to run a workflow or a fan-out, and over what.
pub fn detect(text: &str) -> Option<Over> {
    let raw = text.to_lowercase();
    // The user already names the call, or a slash command expanded into instructions.
    if raw.trim_start().starts_with('/')
        || ["workflow{", "workflow {", "workflow(", "workflow tool", "`workflow`", "dispatch"].iter().any(|k| raw.contains(k))
    {
        return None;
    }
    let plain = strip(text);
    let w = words(&plain);
    if w.is_empty() {
        return None;
    }
    // "can you ...", "please ..." are requests; a bare question word is a question.
    let mut start = 0;
    while start < w.len() && matches!(w[start].as_str(), "please" | "hey" | "ok" | "okay" | "so" | "and" | "now" | "then" | "marlowe") {
        start += 1;
    }
    let polite = matches!(w.get(start).map(String::as_str), Some("can" | "could" | "would" | "will")) && w.get(start + 1).is_some_and(|x| x == "you");
    if polite {
        start += 2;
    }
    let first = w.get(start).map(String::as_str).unwrap_or("");
    let do_question = first == "do" && matches!(w.get(start + 1).map(String::as_str), Some("we" | "you" | "i" | "they" | "workflows" | "the"));
    if !polite && (INTERROGATIVE.contains(&first) || do_question) {
        return None;
    }
    if !polite && plain.trim_end().ends_with('?') {
        return None;
    }
    if w.iter().any(|x| STOPPING.contains(&x.as_str())) || w.iter().any(|x| matches!(x.as_str(), "script" | "scripts" | "star" | "starlark")) {
        return None;
    }
    let has = |a: &str, b: &str| w.windows(2).any(|p| p[0] == a && p[1] == b);
    let workflow = w.iter().any(|x| x == "workflow" || x == "workflows") || has("work", "flow");
    let fan_out = has("fan", "out") || w.iter().any(|x| x == "fanout");
    let per_item = w.windows(3).any(|p| matches!(p[0].as_str(), "agent" | "subagent") && matches!(p[1].as_str(), "per" | "for") && p[2] != "this")
        && w.windows(2).any(|p| p[0] == "one" && matches!(p[1].as_str(), "agent" | "subagent"));
    let parallel = has("in", "parallel") && w.iter().any(|x| matches!(x.as_str(), "agent" | "agents" | "subagents"))
        || has("parallel", "agents")
        || w.windows(3).any(|p| COUNTS.contains(&p[0].as_str()) && p[1] == "parallel" && p[2].starts_with("agent"))
        || w.windows(2).any(|p| COUNTS.contains(&p[0].as_str()) && matches!(p[1].as_str(), "agents" | "subagents"));
    let verb = w.iter().any(|x| LAUNCH.contains(&x.as_str()))
        || w.windows(2).any(|p| matches!(p[0].as_str(), "as" | "with" | "via") && matches!(p[1].as_str(), "a" | "workflow" | "the"))
        || w.windows(2).any(|p| p[0] == "workflow" && matches!(p[1].as_str(), "to" | "that" | "for" | "on" | "over" | "comparing" | "across"));
    let asked = (workflow && verb) || fan_out || per_item || (parallel && verb);
    if !asked {
        return None;
    }
    let any = |ws: &[&str]| w.iter().any(|x| ws.contains(&x.as_str()));
    Some(if any(&["commit", "commits"]) {
        Over::From("commits:5")
    } else if any(&["crate", "crates", "package", "packages", "module", "modules"]) {
        Over::From("packages")
    } else if any(&[
        "repo", "repository", "workspace", "codebase", "code", "folder", "folders", "directory", "directories", "file", "files", "project", "todo", "todos",
    ]) {
        Over::From("folders")
    } else {
        Over::Topic
    })
}

// ---------------------------------------------------------------- the text

/// The folders line: the real top-level folders, from the same resolver `items_from` uses.
fn folders_line(workspace: Option<&Path>) -> Option<String> {
    let found = items::resolve("folders", workspace?).ok()?;
    let dirs: Vec<&str> = found.items.iter().map(String::as_str).filter(|i| !i.starts_with("the top-level files")).collect();
    if dirs.is_empty() {
        return None;
    }
    let more = if dirs.len() > 12 { format!(", and {} more", dirs.len() - 12) } else { String::new() };
    Some(format!("Top-level folders here: {}{more}.", dirs.iter().take(12).copied().collect::<Vec<_>>().join(", ")))
}

fn call_text(over: &Over, prompt: &str) -> String {
    let then = "<how to combine the answers>";
    match over {
        Over::From(sel) => format!("workflow{}", json!({"prompt": prompt, "items_from": sel, "then": then})),
        Over::Topic => format!(
            "workflow{}",
            json!({"prompt": prompt, "items": ["<angle 1>", "<angle 2>", "<angle 3>"], "then": then})
        ),
    }
}

/// The reminder for a reminded turn.
pub fn reminder(over: &Over, workspace: Option<&Path>) -> String {
    let mut s = String::from(
        "<system-reminder>\nThe user asked for a workflow. Call the `workflow` tool now, once, in its short form (not `agent`, not `dispatch`, no script, nothing to look up first):\n",
    );
    match over {
        Over::From(_) => {
            s.push_str(&call_text(over, "<one agent's task for {item}>"));
            s.push_str("\nitems_from: folders, packages, commits:N or a glob; the harness lists the real items, so do not name paths yourself.");
            if let Some(line) = folders_line(workspace) {
                s.push('\n');
                s.push_str(&line);
            }
        }
        Over::Topic => {
            s.push_str(&call_text(over, "<one agent's task on {item}>"));
            s.push_str("\nitems: the angles or topics, one agent each; pick them yourself, do not ask the user for them.");
        }
    }
    s.push_str("\n</system-reminder>");
    s
}

fn clip(s: &str, n: usize) -> String {
    let one: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() <= n {
        one
    } else {
        format!("{}…", one.chars().take(n).collect::<String>())
    }
}

fn redirect_text(over: &Over, prompt: &str) -> String {
    let prompt = if prompt.trim().is_empty() { "<one agent's task for {item}>".to_string() } else { clip(prompt, 400) };
    format!(
        "Not run: the user asked for a workflow; one `workflow` call runs these agents in parallel and combines them: {}",
        call_text(over, &prompt)
    )
}

// ---------------------------------------------------------------- the middleware

fn background(call: &ToolCall) -> bool {
    let flag = call.args.get("background").or_else(|| call.args.get("run_in_background")).and_then(Value::as_bool);
    flag.unwrap_or(true)
}

fn prompt_of(call: &ToolCall) -> String {
    ["prompt", "goal", "task"].iter().find_map(|k| call.args.get(*k).and_then(Value::as_str)).unwrap_or("").to_string()
}

impl ToolMiddleware for WorkflowIntent {
    fn name(&self) -> &str {
        "workflow-intent"
    }

    fn before_turn(&self, convo: &mut Conversation, text: &mut String) -> TurnFlow {
        let over = detect(text);
        let turn = match &over {
            Some(o) => Turn { reminded: true, over: Some(o.clone()), request: clip(&strip(text), 160), ..Turn::default() },
            None => Turn::default(),
        };
        convo.ext.insert(turn);
        if let Some(o) = over {
            let note = reminder(&o, convo.workspace.as_deref());
            convo.pending_context.push(note);
        }
        TurnFlow::Continue
    }

    fn before_approval(&self, call: &mut ToolCall, cx: &CallCtx) -> CallFlow {
        let launching = matches!(call.name.as_str(), "workflow" | "agent" | "dispatch");
        if !launching {
            return CallFlow::Continue;
        }
        cx.scope.with_ext(|e| {
            let Some(t) = e.get_mut::<Turn>() else { return CallFlow::Continue };
            if call.name == "workflow" {
                t.workflow_called = true;
                t.launched = true;
                return CallFlow::Continue;
            }
            let background_call = call.name == "dispatch" || background(call);
            // Only a reminded turn, only before any workflow call, only the first batch.
            if !t.reminded || t.workflow_called || t.redirected || !background_call {
                t.launched = true;
                return CallFlow::Continue;
            }
            t.redirecting = true;
            t.launched = true;
            let over = t.over.clone().unwrap_or(Over::From("folders"));
            CallFlow::Done(ToolOutcome::error(redirect_text(&over, &prompt_of(call))))
        })
    }

    fn after_step(&self, _info: StepInfo, convo: &mut Conversation) -> Vec<Message> {
        if let Some(t) = convo.ext.get_mut::<Turn>() {
            if std::mem::take(&mut t.redirecting) {
                t.redirected = true;
            }
        }
        Vec::new()
    }

    fn before_final(&self, _info: StepInfo, convo: &mut Conversation, text: &str) -> FinalFlow {
        let Some(t) = convo.ext.get_mut::<Turn>() else { return FinalFlow::Accept };
        if !t.reminded || t.launched || t.nudged {
            return FinalFlow::Accept;
        }
        if text.trim().rsplit("\n\n").next().unwrap_or("").contains('?') {
            return FinalFlow::Accept;
        }
        t.nudged = true;
        let over = t.over.clone().unwrap_or(Over::From("folders"));
        let prompt = match over {
            Over::From(_) => format!("For {{item}}: {}", t.request),
            Over::Topic => format!("Investigate {{item}}: {}", t.request),
        };
        // A call typed as prose is the common slip: say so.
        let lead = if text.contains("workflow{") {
            "That workflow{...} was written as text, so nothing ran. Make the tool call itself, once"
        } else {
            "The user asked for a workflow and no workflow ran. Call it now, once"
        };
        FinalFlow::Continue(format!("{lead}: {} (change `then` to say how to combine the answers).", call_text(&over, &prompt)))
    }
}

#[cfg(test)]
mod tests;
