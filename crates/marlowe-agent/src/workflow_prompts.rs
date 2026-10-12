//! The slash commands built on workflows (track W): `/code-review` (G11), `/simplify` (G12),
//! `/workflow <name> [args]` and `/workflow convert <name>` (WF6), `/workflows` (WF7). Like the
//! git commands, each expands into a prompt that has the conductor call the `workflow` tool; a
//! command file of the same name wins over these.

use serde_json::{json, Map, Value};

/// The built-in workflow command `name`, if it is one: its expansion of the arguments.
pub fn builtin(name: &str) -> Option<fn(&str) -> String> {
    match name {
        "code-review" => Some(code_review),
        "simplify" => Some(simplify),
        "workflow" => Some(workflow),
        "workflows" => Some(workflows),
        _ => None,
    }
}

/// What `/code-review` was given: `[level] [target] [--fix] [--comment] [--max-findings n|all|default]`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ReviewArgs {
    pub level: Option<String>,
    pub target: Option<String>,
    pub fix: bool,
    pub comment: bool,
    pub max_findings: Option<Value>,
}

const LEVELS: [&str; 6] = ["low", "medium", "high", "xhigh", "max", "default"];

/// Parses `/code-review` arguments; an unknown flag is an error for the user.
pub fn parse_review(args: &str) -> Result<ReviewArgs, String> {
    let mut out = ReviewArgs::default();
    let words: Vec<&str> = args.split_whitespace().collect();
    let mut i = 0;
    while i < words.len() {
        let w = words[i];
        match w {
            "--fix" => out.fix = true,
            "--comment" => out.comment = true,
            "--max-findings" => {
                let v = words.get(i + 1).ok_or("--max-findings needs a number, `all` or `default`")?;
                out.max_findings = Some(match *v {
                    "all" | "default" => Value::String(v.to_string()),
                    n => Value::from(n.parse::<u64>().map_err(|_| format!("--max-findings takes a number, `all` or `default`, not `{n}`"))?),
                });
                i += 1;
            }
            // `ultra` is the cloud review; here the deepest local level is `max` (RM6).
            "ultra" => out.level = Some("max".into()),
            l if LEVELS.contains(&l) => out.level = Some(l.to_string()),
            f if f.starts_with("--") => return Err(format!("unknown flag {f}; flags are --fix, --comment and --max-findings")),
            t => out.target = Some(t.to_string()),
        }
        i += 1;
    }
    Ok(out)
}

/// The `args` the built-in `code-review` workflow gets.
pub fn review_args(r: &ReviewArgs) -> Value {
    let mut m = Map::new();
    if let Some(t) = &r.target {
        m.insert("target".into(), json!(t));
    }
    if let Some(l) = &r.level {
        m.insert("level".into(), json!(l));
    }
    if r.fix {
        m.insert("fix".into(), json!(true));
    }
    if r.comment {
        m.insert("comment".into(), json!(true));
    }
    if let Some(n) = &r.max_findings {
        m.insert("max_findings".into(), n.clone());
    }
    Value::Object(m)
}

/// `/code-review`.
pub fn code_review(args: &str) -> String {
    let r = match parse_review(args) {
        Ok(r) => r,
        Err(e) => return format!("Tell the user, in one line, that /code-review did not start: {e}. Usage: /code-review [low|medium|high|xhigh|max|default] [pr number or base ref] [--fix] [--comment] [--max-findings n|all|default]"),
    };
    let call = json!({"name": "code-review", "args": review_args(&r), "background": false});
    let mut text = format!(
        "Run the built-in code review now: call workflow{call} exactly. It reviews {} with finders per lens and adversarial verifiers; \
         a level or cap not given here is the one remembered for this project.\n\
         When it returns, call report_findings with its `findings` (unchanged), then give the user the findings ranked by severity, \
         each as file:line, the claim and the fix, and say how many were refuted.",
        match r.target.as_deref() {
            None => "the pending changes".to_string(),
            Some(t) if t.trim_start_matches('#').chars().all(|c| c.is_ascii_digit()) => format!("pull request {t}"),
            Some(t) => format!("the changes against {t}"),
        }
    );
    if r.fix {
        text.push_str(" Fixes were applied in the run's worktree: show the diff and that the merge needs the user's approval.");
    }
    if r.comment {
        text.push_str(" Then post the findings on the pull request with gh{op:\"pr_comment\"}, as one comment; that asks the user once.");
    }
    text
}

/// `/simplify [target]`.
pub fn simplify(args: &str) -> String {
    let target = args.trim();
    let call = if target.is_empty() {
        json!({"name": "simplify", "background": false})
    } else {
        json!({"name": "simplify", "args": {"target": target}, "background": false})
    };
    format!(
        "Run the built-in simplify workflow now: call workflow{call} exactly. It looks at the changed code for reuse, simplification, \
         efficiency and altitude cleanups and applies them in the run's worktree; it does not hunt for bugs. When it returns, tell the user \
         what was applied and skipped, and that the diff needs their approval to merge."
    )
}

/// `/workflow <name> [json args]` and `/workflow convert <name>`.
pub fn workflow(args: &str) -> String {
    let a = args.trim();
    if a.is_empty() {
        return "List the saved workflows for the user (they are in the workflow tool's description and /workflows) and ask which to run.".into();
    }
    if let Some(name) = a.strip_prefix("convert ").map(str::trim) {
        let js = format!(".claude/workflows/{name}.js");
        let target = format!(".marlowe/workflows/{name}.star");
        return marlowe_workflow::convert::conversion_prompt(name, &js, &target);
    }
    let (name, rest) = a.split_once(char::is_whitespace).map(|(n, r)| (n, r.trim())).unwrap_or((a, ""));
    let args_json = if rest.is_empty() {
        Value::Object(Map::new())
    } else {
        match serde_json::from_str::<Value>(rest) {
            Ok(v @ Value::Object(_)) => v,
            _ => json!({"input": rest}),
        }
    };
    let call = json!({"name": name, "args": args_json});
    format!(
        "Start the saved workflow `{name}` now: call workflow{call}. It runs in the background; tell the user it started (and its run id), \
         then carry on. Its result arrives as a notice."
    )
}

/// `/workflows`: the TUI shows the view; a client without it gets the list.
pub fn workflows(_args: &str) -> String {
    "Tell the user which workflows are saved (name and one line each) and which workflow runs are going or finished, from what you know; \
     the /workflows view in the terminal shows live progress."
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parity_g11_code_review_arguments() {
        let r = parse_review("high 42 --fix --comment --max-findings 5").unwrap();
        assert_eq!(review_args(&r), json!({"target": "42", "level": "high", "fix": true, "comment": true, "max_findings": 5}));
        assert_eq!(review_args(&parse_review("").unwrap()), json!({}), "nothing given: the remembered level and cap apply");
        assert_eq!(review_args(&parse_review("default --max-findings default").unwrap()), json!({"level": "default", "max_findings": "default"}));
        assert_eq!(review_args(&parse_review("ultra").unwrap())["level"], "max");
        assert!(parse_review("--fixx").unwrap_err().contains("unknown flag"));
        assert!(parse_review("--max-findings lots").is_err());
        let text = code_review("medium --comment");
        assert!(text.contains("\"name\":\"code-review\"") && text.contains("\"background\":false") && text.contains("pr_comment"), "{text}");
        assert!(text.contains("report_findings"));
    }

    #[test]
    fn workflow_commands_expand_to_tool_calls() {
        assert!(simplify("main").contains("\"target\":\"main\""));
        let t = workflow("triage {\"files\": [\"a.rs\"]}");
        assert!(t.contains("\"name\":\"triage\"") && t.contains("\"files\":[\"a.rs\"]"), "{t}");
        assert!(workflow("convert portfolio-assess").contains(".marlowe/workflows/portfolio-assess.star"));
        assert!(builtin("code-review").is_some() && builtin("review").is_none());
    }
}
