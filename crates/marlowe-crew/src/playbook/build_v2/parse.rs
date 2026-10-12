//! Reading structured answers out of model text, leniently: a fenced ```json block or the first
//! balanced `{...}` / `[...]`, trailing commas forgiven. What does not validate goes back to the
//! model with the reasons, once, before the playbook gives up on it.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use super::types::{BuildPlan, Finding, Lens, Severity, Task};
use crate::sandbox::safe_name;

/// Most tasks a plan may have.
pub const MAX_TASKS: usize = 12;

/// The JSON value in `text`: the last ```json fence, else the first balanced object or array.
pub fn json_value(text: &str) -> Option<Value> {
    let mut candidates: Vec<String> = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find("```") {
        let after = &rest[i + 3..];
        let body_start = after.find('\n').map(|n| n + 1).unwrap_or(0);
        let Some(end) = after[body_start..].find("```") else { break };
        candidates.push(after[body_start..body_start + end].to_string());
        rest = &after[body_start + end + 3..];
    }
    candidates.reverse();
    // Whichever opens first is the answer: `[{...}]` is an array, not its first object.
    let mut spans: Vec<(usize, String)> =
        ['{', '['].into_iter().filter_map(|open| Some((text.find(open)?, balanced(text, open)?))).collect();
    spans.sort_by_key(|(at, _)| *at);
    candidates.extend(spans.into_iter().map(|(_, s)| s));
    candidates.iter().find_map(|c| parse_lenient(c))
}

fn parse_lenient(s: &str) -> Option<Value> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    serde_json::from_str(t).ok().or_else(|| serde_json::from_str(&drop_trailing_commas(t)).ok())
}

/// `,` directly before `}` or `]` (outside strings) removed.
fn drop_trailing_commas(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut in_str = false;
    let mut escaped = false;
    for (i, &c) in chars.iter().enumerate() {
        if in_str {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        if c == '"' {
            in_str = true;
        }
        if c == ',' {
            let next = chars[i + 1..].iter().find(|n| !n.is_whitespace());
            if matches!(next, Some('}') | Some(']')) {
                continue;
            }
        }
        out.push(c);
    }
    out
}

/// The first balanced `open ... close` span in `text`, strings respected.
fn balanced(text: &str, open: char) -> Option<String> {
    let close = if open == '{' { '}' } else { ']' };
    let start = text.find(open)?;
    let mut depth = 0i32;
    let mut in_str = false;
    let mut escaped = false;
    for (i, c) in text[start..].char_indices() {
        if in_str {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            c if c == open => depth += 1,
            c if c == close => {
                depth -= 1;
                if depth == 0 {
                    return Some(text[start..start + i + c.len_utf8()].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

fn strings(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::Array(a)) => a.iter().filter_map(|x| x.as_str()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
        Some(Value::String(s)) if !s.trim().is_empty() => s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect(),
        _ => Vec::new(),
    }
}

fn text_of(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Array(a)) => a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join("; "),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

/// A relative path in a plan: forward slashes, no `..`, not absolute.
fn rel_path(p: &str) -> Result<String, String> {
    let t = p.trim().replace('\\', "/");
    let t = t.trim_start_matches("./").to_string();
    if t.is_empty() {
        return Err("an empty path".into());
    }
    if t.starts_with('/') || t.contains(':') || t.split('/').any(|s| s == "..") {
        return Err(format!("`{p}` must be a path relative to the repository root, inside it"));
    }
    Ok(t)
}

/// Reads and validates the lead's plan. `known_hubs` (from the project profile) join the plan's
/// own hub list. Overlapping non-hub `files_owned` between tasks with no dependency between them
/// serialise (the later task waits for the earlier) and say so in `notes`. The error lists every
/// problem, for the lead to fix in one go.
pub fn parse_plan(text: &str, known_hubs: &[String]) -> Result<BuildPlan, Vec<String>> {
    let Some(v) = json_value(text) else {
        return Err(vec!["no JSON plan found: reply with one ```json block holding {\"tasks\": [...], \"hubs\": [...]}".into()]);
    };
    let (tasks_v, hubs_v) = match &v {
        Value::Array(_) => (Some(&v), None),
        Value::Object(o) => (o.get("tasks"), o.get("hubs").or_else(|| o.get("hub_files"))),
        _ => (None, None),
    };
    let mut errors = Vec::new();
    let Some(Value::Array(raw)) = tasks_v else {
        return Err(vec!["the plan has no `tasks` array".into()]);
    };
    if raw.is_empty() {
        errors.push("the plan has no tasks".into());
    }
    if raw.len() > MAX_TASKS {
        errors.push(format!("{} tasks is too many; merge them into at most {MAX_TASKS}", raw.len()));
    }
    let mut tasks: Vec<Task> = Vec::new();
    for (i, t) in raw.iter().enumerate() {
        let n = i + 1;
        let Some(o) = t.as_object() else {
            errors.push(format!("task {n} is not an object"));
            continue;
        };
        let id = text_of(o.get("id"));
        if id.is_empty() {
            errors.push(format!("task {n} has no `id`"));
            continue;
        }
        if safe_name(&id) != id || id.len() > 24 {
            errors.push(format!("task id `{id}` must be short lower-case letters, digits, `-` or `_`"));
        }
        let goal = text_of(o.get("goal").or_else(|| o.get("description")));
        if goal.is_empty() {
            errors.push(format!("task `{id}` has no `goal`"));
        }
        let mut files = Vec::new();
        for f in strings(o.get("files_owned").or_else(|| o.get("files"))) {
            match rel_path(&f) {
                Ok(p) => files.push(p),
                Err(e) => errors.push(format!("task `{id}`: {e}")),
            }
        }
        let accept = text_of(o.get("accept_cmd").or_else(|| o.get("accept")));
        tasks.push(Task {
            id,
            goal,
            files_owned: files,
            interfaces: text_of(o.get("interfaces")),
            depends_on: strings(o.get("depends_on").or_else(|| o.get("deps"))),
            accept_cmd: (!accept.is_empty()).then_some(accept),
        });
    }
    let mut seen = BTreeSet::new();
    for t in &tasks {
        if !seen.insert(t.id.clone()) {
            errors.push(format!("task id `{}` is used twice", t.id));
        }
    }
    for t in &tasks {
        for d in &t.depends_on {
            if d == &t.id {
                errors.push(format!("task `{}` depends on itself", t.id));
            } else if !seen.contains(d) {
                errors.push(format!("task `{}` depends on `{d}`, which is not a task", t.id));
            }
        }
    }
    if errors.is_empty() {
        if let Some(cycle) = find_cycle(&tasks) {
            errors.push(format!("the dependencies form a cycle: {}", cycle.join(" -> ")));
        }
    }
    let mut hubs: Vec<String> = Vec::new();
    for h in strings(hubs_v).into_iter().chain(known_hubs.iter().cloned()) {
        match rel_path(&h) {
            Ok(p) if !hubs.iter().any(|x| x.eq_ignore_ascii_case(&p)) => hubs.push(p),
            Ok(_) => {}
            Err(e) => errors.push(format!("hub: {e}")),
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    let mut plan = BuildPlan { tasks, hubs, notes: Vec::new() };
    serialise_overlaps(&mut plan);
    Ok(plan)
}

/// A dependency cycle, as ids, if there is one.
fn find_cycle(tasks: &[Task]) -> Option<Vec<String>> {
    let deps: BTreeMap<&str, Vec<&str>> =
        tasks.iter().map(|t| (t.id.as_str(), t.depends_on.iter().map(String::as_str).collect())).collect();
    fn visit<'a>(
        n: &'a str,
        deps: &BTreeMap<&'a str, Vec<&'a str>>,
        state: &mut BTreeMap<&'a str, u8>,
        path: &mut Vec<&'a str>,
    ) -> Option<Vec<String>> {
        match state.get(n) {
            Some(2) => return None,
            Some(1) => {
                let from = path.iter().position(|p| *p == n).unwrap_or(0);
                let mut c: Vec<String> = path[from..].iter().map(|s| s.to_string()).collect();
                c.push(n.to_string());
                return Some(c);
            }
            _ => {}
        }
        state.insert(n, 1);
        path.push(n);
        for d in deps.get(n).cloned().unwrap_or_default() {
            if let Some(c) = visit(d, deps, state, path) {
                return Some(c);
            }
        }
        path.pop();
        state.insert(n, 2);
        None
    }
    let mut state = BTreeMap::new();
    for t in tasks {
        if let Some(c) = visit(&t.id, &deps, &mut state, &mut Vec::new()) {
            return Some(c);
        }
    }
    None
}

/// Whether `a` reaches `b` through `depends_on`.
fn reaches(plan: &BuildPlan, a: &str, b: &str) -> bool {
    let mut stack = vec![a.to_string()];
    let mut seen = BTreeSet::new();
    while let Some(n) = stack.pop() {
        if n == b {
            return true;
        }
        if !seen.insert(n.clone()) {
            continue;
        }
        if let Some(t) = plan.task(&n) {
            stack.extend(t.depends_on.iter().cloned());
        }
    }
    false
}

fn serialise_overlaps(plan: &mut BuildPlan) {
    let is_hub = |f: &str, hubs: &[String]| hubs.iter().any(|h| h.eq_ignore_ascii_case(f));
    for j in 0..plan.tasks.len() {
        for i in 0..j {
            let (a, b) = (plan.tasks[i].clone(), plan.tasks[j].clone());
            let shared: Vec<&String> = b
                .files_owned
                .iter()
                .filter(|f| !is_hub(f, &plan.hubs) && a.files_owned.iter().any(|g| g.eq_ignore_ascii_case(f)))
                .collect();
            if shared.is_empty() || reaches(plan, &b.id, &a.id) || reaches(plan, &a.id, &b.id) {
                continue;
            }
            let note = format!(
                "`{}` and `{}` both own {}; `{}` now waits for `{}`",
                a.id,
                b.id,
                shared.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "),
                b.id,
                a.id
            );
            plan.tasks[j].depends_on.push(a.id.clone());
            plan.notes.push(note);
        }
    }
}

/// Reads a reviewer's findings: a JSON array (or `{"findings": [...]}`). An empty array, or a
/// reply that says plainly there is nothing, is no findings. `Err` when nothing parses.
pub fn parse_findings(text: &str, lens: Lens) -> Result<Vec<Finding>, String> {
    let v = match json_value(text) {
        Some(v) => v,
        None => {
            let t = text.trim().to_ascii_lowercase();
            if t.is_empty() || t.contains("no findings") || t.contains("no blocking issues") || t.contains("nothing to report") {
                return Ok(Vec::new());
            }
            return Err("no JSON array of findings found".into());
        }
    };
    let items = match &v {
        Value::Array(a) => a.clone(),
        Value::Object(o) => match o.get("findings") {
            Some(Value::Array(a)) => a.clone(),
            _ => return Err("expected a JSON array of findings".into()),
        },
        _ => return Err("expected a JSON array of findings".into()),
    };
    let mut out = Vec::new();
    for (i, f) in items.iter().enumerate() {
        let Some(o) = f.as_object() else { return Err(format!("finding {} is not an object", i + 1)) };
        let sev = text_of(o.get("severity"));
        let severity = Severity::parse(&sev).ok_or_else(|| format!("finding {}: severity `{sev}` is not blocker, major, minor or nit", i + 1))?;
        let claim = text_of(o.get("claim").or_else(|| o.get("issue")).or_else(|| o.get("message")));
        if claim.is_empty() {
            return Err(format!("finding {} has no `claim`", i + 1));
        }
        let line = o.get("line").and_then(|l| l.as_u64().or_else(|| l.as_str().and_then(|s| s.trim().parse().ok()))).unwrap_or(0);
        out.push(Finding {
            lens,
            severity,
            file: text_of(o.get("file")).replace('\\', "/"),
            line: line.min(u64::from(u32::MAX)) as u32,
            claim,
            evidence: text_of(o.get("evidence")),
            fix: text_of(o.get("fix")),
        });
    }
    Ok(out)
}

/// `NEED_HUB_CHANGE: <file>: <what>` lines in an engineer's reply.
pub fn hub_requests(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|l| {
            let t = l.trim().trim_start_matches(['-', '*', ' ']).trim();
            let rest = t.strip_prefix("NEED_HUB_CHANGE:").or_else(|| t.strip_prefix("NEED_HUB_CHANGE"))?;
            let (file, what) = rest.split_once(':').unwrap_or((rest, ""));
            let file = file.trim().trim_matches('`').replace('\\', "/");
            (!file.is_empty()).then(|| (file, what.trim().to_string()))
        })
        .collect()
}
