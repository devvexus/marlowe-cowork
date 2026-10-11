//! `gh{op}`: GitHub through the user's own `gh` login (design 3.1, 3.9).
//!
//! Reads (a PR, a list, a diff, checks, review threads, an issue) are free, taint the session
//! (other people wrote the text) and, after untrusted content, ask for a host not yet approved.
//! Everything that posts asks: `pr_create` (one approval covering the push and the PR, with
//! remote, branch, title and body shown), `pr_comment`, `pr_reply`, `pr_review`.

use marlowe_agent::{CallCtx, Consequence, Exposure, ToolCallReq, ToolOutcome};
use serde_json::{json, Value};

use super::git_tool::{flag, remote_for, Ctx};
use super::run::{body_arg, Out, TempFile, INLINE_MAX, MAX_RESULT_BYTES, WRITE_TIMEOUT};
use super::{pr, policy, threads, watch};
use crate::{first_line, int_arg, str_arg, text, BuiltinTools};

pub(crate) const OPS: &[&str] =
    &["pr_create", "pr_view", "pr_list", "pr_diff", "pr_checks", "pr_comments", "pr_comment", "pr_reply", "pr_review", "issue_view"];

/// Ops that read other people's words.
fn reads_untrusted(op: &str) -> bool {
    matches!(op, "pr_view" | "pr_list" | "pr_diff" | "pr_comments" | "issue_view")
}

fn posts(op: &str) -> bool {
    matches!(op, "pr_create" | "pr_comment" | "pr_reply" | "pr_review")
}

/// The GitHub host `gh` talks to.
pub(crate) fn host() -> String {
    std::env::var("GH_HOST").ok().filter(|h| !h.trim().is_empty()).unwrap_or_else(|| "github.com".into())
}

/// `number` as `gh` takes it: digits or a PR/issue URL.
fn number(args: &Value) -> Result<Option<String>, String> {
    let raw = match args.get("number") {
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::String(s)) => s.trim().to_string(),
        _ => return Ok(None),
    };
    if raw.is_empty() {
        return Ok(None);
    }
    let ok = raw.chars().all(|c| c.is_ascii_digit())
        || (raw.starts_with("https://") && !raw.contains(char::is_whitespace));
    if ok {
        Ok(Some(raw))
    } else {
        Err(format!("`number` must be a PR number or its URL, got `{}`", first_line(&raw, 40)))
    }
}

pub(super) fn parse(o: &Out, what: &str) -> Result<Value, String> {
    if let Some(e) = &o.spawn_error {
        return Err(format!("could not run {what}: {e}"));
    }
    if o.cancelled || o.timed_out {
        return Err(o.failure(what));
    }
    match serde_json::from_str::<Value>(o.stdout.trim()) {
        // `gh pr checks` exits 1 or 8 for failing or pending checks and still prints its JSON.
        Ok(v) if o.ok() || matches!(o.code, 1 | 8) => Ok(v),
        _ if !o.ok() => Err(auth_hint(o, what)),
        Ok(v) => Ok(v),
        Err(e) => Err(format!("{what} gave output that is not JSON ({e}): {}", first_line(&o.stdout, 120))),
    }
}

/// A failure message, with the fix when the cause is a missing login.
fn auth_hint(o: &Out, what: &str) -> String {
    let said = o.all();
    let mut msg = o.failure(what);
    if said.contains("auth login") || said.contains("not logged in") || said.contains("authentication") || said.contains("GH_TOKEN") {
        msg.push_str("\n(the GitHub CLI is not signed in: ask the user to run `gh auth login` themselves)");
    }
    msg
}

fn run_gh(c: &Ctx, args: &[&str], what: &str) -> Result<Value, String> {
    let o = c.env.gh(c.dir, args, WRITE_TIMEOUT, c.cancel);
    parse(&o, what)
}

pub(crate) fn gh_call(tools: &BuiltinTools, args: &Value, cx: &CallCtx) -> ToolOutcome {
    if let Some(why) = policy::deny_structured("gh", args) {
        return ToolOutcome::error(format!("refused: {why}"));
    }
    let Some(op) = str_arg(args, "op") else {
        return ToolOutcome::error(format!("`op` is required: one of {}", OPS.join(", ")));
    };
    let env = super::Env::of(tools);
    let dir = match env.dir(cx.workspace.as_deref(), str_arg(args, "cwd")) {
        Ok(d) => d,
        Err(e) => return ToolOutcome::error(e),
    };
    let cancel = || cx.cancel.is_cancelled();
    let c = Ctx { env: &env, dir: &dir, cancel: &cancel };
    let result = match op {
        "pr_create" => return pr::create(&c, args),
        "pr_view" => pr_view(&c, args),
        "pr_list" => pr_list(&c, args),
        "pr_diff" => pr_diff(&c, args),
        "pr_checks" => pr_checks(&c, args),
        "pr_comments" => pr_comments(&c, args),
        "pr_comment" => pr_comment(&c, args),
        "pr_reply" => pr_reply(&c, args),
        "pr_review" => pr_review(&c, args),
        "issue_view" => issue_view(&c, args),
        other => Err(format!("unknown gh op `{other}`; use one of {}", OPS.join(", "))),
    };
    match result {
        Ok((content, summary)) => ToolOutcome::ok(content, summary),
        Err(e) => ToolOutcome::error(e),
    }
}

type Res = Result<(String, String), String>;

fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

fn login(v: &Value) -> &str {
    v.pointer("/author/login").and_then(Value::as_str).unwrap_or("ghost")
}

fn labels(v: &Value) -> String {
    v.get("labels")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|l| l.get("name").and_then(Value::as_str)).collect::<Vec<_>>().join(", "))
        .unwrap_or_default()
}

fn pr_view(c: &Ctx, args: &Value) -> Res {
    let n = number(args)?;
    let fields = "number,title,state,isDraft,author,headRefName,baseRefName,url,body,additions,deletions,changedFiles,mergeable,reviewDecision,labels,statusCheckRollup";
    let mut a = vec!["pr", "view"];
    a.extend(n.as_deref());
    a.extend(["--json", fields]);
    let v = run_gh(c, &a, "gh pr view")?;
    let state = if v.get("isDraft").and_then(Value::as_bool) == Some(true) { "draft".to_string() } else { s(&v, "state").to_ascii_lowercase() };
    let mut out = format!(
        "#{} {} [{}]  {} -> {}  by {}\n{}\n+{} −{} in {} files",
        v.get("number").and_then(Value::as_i64).unwrap_or(0),
        s(&v, "title"),
        state,
        s(&v, "headRefName"),
        s(&v, "baseRefName"),
        login(&v),
        s(&v, "url"),
        v.get("additions").and_then(Value::as_i64).unwrap_or(0),
        v.get("deletions").and_then(Value::as_i64).unwrap_or(0),
        v.get("changedFiles").and_then(Value::as_i64).unwrap_or(0),
    );
    for (label, key) in [("review", "reviewDecision"), ("mergeable", "mergeable")] {
        let val = s(&v, key);
        if !val.is_empty() {
            out.push_str(&format!(" · {label}: {val}"));
        }
    }
    let lab = labels(&v);
    if !lab.is_empty() {
        out.push_str(&format!(" · labels: {lab}"));
    }
    if let Some(checks) = v.get("statusCheckRollup").and_then(Value::as_array).filter(|a| !a.is_empty()) {
        let verdict = |c: &Value| -> String {
            let concl = s(c, "conclusion");
            if concl.is_empty() { s(c, "state").to_string() } else { concl.to_string() }
        };
        let failing: Vec<String> = checks
            .iter()
            .filter(|c| matches!(verdict(c).as_str(), "FAILURE" | "ERROR" | "TIMED_OUT" | "STARTUP_FAILURE"))
            .map(|c| if s(c, "name").is_empty() { s(c, "context").to_string() } else { s(c, "name").to_string() })
            .collect();
        let passed = checks.iter().filter(|c| matches!(verdict(c).as_str(), "SUCCESS" | "NEUTRAL" | "SKIPPED")).count();
        out.push_str(&format!("\nchecks: {passed} passed, {} failed{}, {} other", failing.len(), if failing.is_empty() { String::new() } else { format!(" ({})", failing.join(", ")) }, checks.len() - passed - failing.len()));
    }
    let body = s(&v, "body").trim();
    if !body.is_empty() {
        out.push_str(&format!("\n\n{}", text::head_and_tail(body, 6_000)));
    }
    let summary = format!("#{} {}", v.get("number").and_then(Value::as_i64).unwrap_or(0), state);
    Ok((out, summary))
}

fn pr_list(c: &Ctx, args: &Value) -> Res {
    let limit = int_arg(args, "limit").unwrap_or(20).clamp(1, 100).to_string();
    let mut a: Vec<String> = ["pr", "list", "--json", "number,title,headRefName,baseRefName,state,isDraft,author,url,updatedAt", "--limit"]
        .map(String::from)
        .to_vec();
    a.push(limit);
    for (key, opt) in [("state", "--state"), ("author", "--author"), ("search", "--search"), ("head", "--head"), ("base", "--base")] {
        if let Some(v) = str_arg(args, key).filter(|v| !v.is_empty()) {
            if v.starts_with('-') {
                return Err(format!("`{key}` must not start with `-`"));
            }
            a.push(opt.into());
            a.push(v.to_string());
        }
    }
    let refs: Vec<&str> = a.iter().map(String::as_str).collect();
    let v = run_gh(c, &refs, "gh pr list")?;
    let rows = v.as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        return Ok(("(no pull requests match)".into(), "0 PRs".into()));
    }
    let mut out = String::new();
    for r in &rows {
        let state = if r.get("isDraft").and_then(Value::as_bool) == Some(true) { "draft".to_string() } else { s(r, "state").to_ascii_lowercase() };
        out.push_str(&format!(
            "#{} {} [{}] {} -> {} by {}  {}\n",
            r.get("number").and_then(Value::as_i64).unwrap_or(0),
            s(r, "title"),
            state,
            s(r, "headRefName"),
            s(r, "baseRefName"),
            login(r),
            s(r, "url")
        ));
    }
    Ok((text::head_and_tail(&out, MAX_RESULT_BYTES), format!("{} PRs", rows.len())))
}

fn pr_diff(c: &Ctx, args: &Value) -> Res {
    let n = number(args)?;
    let mut a = vec!["pr", "diff"];
    a.extend(n.as_deref());
    if flag(args, "names_only") {
        a.push("--name-only");
    }
    let o = c.env.gh(c.dir, &a, WRITE_TIMEOUT, c.cancel);
    if !o.ok() {
        return Err(auth_hint(&o, "gh pr diff"));
    }
    let lines = o.out().lines().count();
    Ok((text::head_and_tail(o.out(), MAX_RESULT_BYTES), format!("{lines} lines")))
}

pub(super) fn pr_checks(c: &Ctx, args: &Value) -> Res {
    let n = number(args)?;
    let mut a = vec!["pr", "checks"];
    a.extend(n.as_deref());
    a.extend(["--json", "name,state,bucket,workflow,link"]);
    let v = run_gh(c, &a, "gh pr checks")?;
    let rows = v.as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        return Ok(("(no checks reported)".into(), "no checks".into()));
    }
    let count = |b: &str| rows.iter().filter(|r| s(r, "bucket") == b).count();
    let mut out = String::new();
    for r in &rows {
        let wf = if s(r, "workflow").is_empty() { String::new() } else { format!("{} / ", s(r, "workflow")) };
        out.push_str(&format!("{:<8} {}{}  {}\n", s(r, "bucket"), wf, s(r, "name"), s(r, "link")));
    }
    let summary = format!("{} pass, {} fail, {} pending", count("pass"), count("fail"), count("pending"));
    Ok((out, summary))
}

/// `owner/repo` for API paths: the push remote's GitHub slug, else gh's own placeholders.
fn repo_path(c: &Ctx, args: &Value) -> (String, bool) {
    let branch = c.branch().unwrap_or_default();
    let remote = remote_for(c, args, &branch);
    match c.out(&["remote", "get-url", &remote]).and_then(|u| watch::github_slug(&u)) {
        Some(slug) => (slug, true),
        None => ("{owner}/{repo}".to_string(), false),
    }
}

fn current_number(c: &Ctx) -> Result<String, String> {
    let v = run_gh(c, &["pr", "view", "--json", "number"], "gh pr view")
        .map_err(|e| format!("{e}\n(name the PR with `number`)"))?;
    v.get("number").and_then(Value::as_i64).map(|n| n.to_string()).ok_or_else(|| "this branch has no pull request; name one with `number`".to_string())
}

fn need_number(c: &Ctx, args: &Value) -> Result<String, String> {
    match number(args)? {
        Some(n) => Ok(n),
        None => current_number(c),
    }
}

fn pr_comments(c: &Ctx, args: &Value) -> Res {
    let n = need_number(c, args)?;
    let n_num = n.rsplit('/').next().unwrap_or(&n).to_string();
    if n_num.parse::<u64>().is_err() {
        return Err("`number` must be the PR number".into());
    }
    let (repo, known) = repo_path(c, args);
    let (owner, name) = repo.split_once('/').unwrap_or(("{owner}", "{repo}"));
    let number_field = format!("number={n_num}");
    let query = format!("query={}", threads::QUERY);
    let (owner_f, name_f) = (format!("owner={owner}"), format!("name={name}"));
    let a: Vec<&str> = if known {
        vec!["api", "graphql", "-f", &owner_f, "-f", &name_f, "-F", &number_field, "-f", &query]
    } else {
        vec!["api", "graphql", "-F", "owner={owner}", "-F", "name={repo}", "-F", &number_field, "-f", &query]
    };
    let v = run_gh(c, &a, "gh api graphql")?;
    threads::render(&v, flag(args, "include_resolved"))
}

fn body_of(args: &Value, what: &str) -> Result<String, String> {
    let b = str_arg(args, "body").map(str::trim).unwrap_or("");
    if b.is_empty() {
        return Err(format!("{what} needs `body`"));
    }
    if b.len() > 60_000 {
        return Err("the comment is over 60,000 characters".into());
    }
    Ok(b.to_string())
}

fn pr_comment(c: &Ctx, args: &Value) -> Res {
    let body = body_of(args, "pr_comment")?;
    let n = number(args)?;
    let (body_flag, _file) = body_arg(&body)?;
    let mut a = vec!["pr", "comment"];
    a.extend(n.as_deref());
    a.push(&body_flag);
    let o = c.env.gh(c.dir, &a, WRITE_TIMEOUT, c.cancel);
    if !o.ok() {
        return Err(auth_hint(&o, "gh pr comment"));
    }
    let url = pr::find_pr_url(&o.all()).or_else(|| o.all().split_whitespace().find(|w| w.starts_with("http")).map(String::from));
    Ok((format!("Posted the comment{}", url.map(|u| format!(": {u}")).unwrap_or_default()), "commented".into()))
}

fn pr_reply(c: &Ctx, args: &Value) -> Res {
    let body = body_of(args, "pr_reply")?;
    let n = number(args)?.ok_or("pr_reply needs `number` (the PR)")?;
    let id = int_arg(args, "comment_id").filter(|i| *i > 0).ok_or("pr_reply needs `comment_id` (the thread's first comment id from pr_comments)")?;
    let (repo, _) = repo_path(c, args);
    let endpoint = format!("repos/{repo}/pulls/{n}/comments/{id}/replies");
    let field = format!("body={body}");
    let long = (body.len() > INLINE_MAX).then(|| TempFile::new("reply.json", &json!({"body": body}).to_string())).transpose().map_err(|e| format!("could not stage the reply: {e}"))?;
    let input = long.as_ref().map(TempFile::path).unwrap_or_default();
    let o = match &long {
        Some(_) => c.env.gh(c.dir, &["api", "-X", "POST", &endpoint, "--input", &input], WRITE_TIMEOUT, c.cancel),
        None => c.env.gh(c.dir, &["api", "-X", "POST", &endpoint, "-f", &field], WRITE_TIMEOUT, c.cancel),
    };
    if !o.ok() {
        return Err(auth_hint(&o, "gh api (reply)"));
    }
    let url = serde_json::from_str::<Value>(o.out()).ok().and_then(|v| v.get("html_url").and_then(Value::as_str).map(String::from));
    Ok((format!("Replied to comment {id}{}", url.map(|u| format!(": {u}")).unwrap_or_default()), "replied".into()))
}

fn pr_review(c: &Ctx, args: &Value) -> Res {
    let n = number(args)?.ok_or("pr_review needs `number` (the PR)")?;
    let event = match str_arg(args, "event").unwrap_or("comment").to_ascii_lowercase().as_str() {
        "comment" => "COMMENT",
        "approve" => "APPROVE",
        "request_changes" | "request-changes" => "REQUEST_CHANGES",
        other => return Err(format!("`event` is comment, approve or request_changes, not `{other}`")),
    };
    let body = str_arg(args, "body").map(str::trim).unwrap_or("");
    let mut comments = Vec::new();
    if let Some(Value::Array(items)) = args.get("comments") {
        for (i, it) in items.iter().enumerate() {
            let path = it.get("path").and_then(Value::as_str).filter(|p| !p.is_empty());
            let line = it.get("line").and_then(Value::as_i64).filter(|l| *l >= 1);
            let text = it.get("body").and_then(Value::as_str).map(str::trim).filter(|b| !b.is_empty());
            let (Some(path), Some(line), Some(text)) = (path, line, text) else {
                return Err(format!("comments[{i}] needs `path`, `line` (>= 1) and `body`"));
            };
            let side = if it.get("side").and_then(Value::as_str) == Some("LEFT") { "LEFT" } else { "RIGHT" };
            comments.push(json!({"path": path, "line": line, "side": side, "body": text}));
        }
    }
    if body.is_empty() && comments.is_empty() {
        return Err("pr_review needs `body` or `comments`".into());
    }
    if event == "REQUEST_CHANGES" && body.is_empty() {
        return Err("a review that requests changes needs a `body`".into());
    }
    let (repo, _) = repo_path(c, args);
    let endpoint = format!("repos/{repo}/pulls/{n}/reviews");
    let payload = json!({"event": event, "body": body, "comments": comments});
    let file = TempFile::new("review.json", &payload.to_string()).map_err(|e| format!("could not stage the review: {e}"))?;
    let input = file.path();
    let o = c.env.gh(c.dir, &["api", "-X", "POST", &endpoint, "--input", &input], WRITE_TIMEOUT, c.cancel);
    if !o.ok() {
        return Err(auth_hint(&o, "gh api (review)"));
    }
    Ok((format!("Posted a {} review with {} inline comments on PR #{n}", event.to_ascii_lowercase().replace('_', " "), comments.len()), "reviewed".into()))
}

fn issue_view(c: &Ctx, args: &Value) -> Res {
    let n = number(args)?.ok_or("issue_view needs `number`")?;
    let v = run_gh(c, &["issue", "view", &n, "--json", "number,title,state,author,body,labels,comments,url"], "gh issue view")?;
    let mut out = format!(
        "#{} {} [{}] by {}\n{}",
        v.get("number").and_then(Value::as_i64).unwrap_or(0),
        s(&v, "title"),
        s(&v, "state").to_ascii_lowercase(),
        login(&v),
        s(&v, "url")
    );
    let lab = labels(&v);
    if !lab.is_empty() {
        out.push_str(&format!("\nlabels: {lab}"));
    }
    out.push_str(&format!("\n\n{}", text::head_and_tail(s(&v, "body").trim(), 8_000)));
    let comments = v.get("comments").and_then(Value::as_array).cloned().unwrap_or_default();
    out.push_str(&format!("\n\nCOMMENTS: {}\n", comments.len()));
    for cm in comments.iter().take(30) {
        out.push_str(&format!("  {} ({}):\n{}\n", login(cm), s(cm, "createdAt").chars().take(10).collect::<String>(), text::head_and_tail(s(cm, "body").trim(), 1_500).lines().map(|l| format!("    {l}")).collect::<Vec<_>>().join("\n")));
    }
    Ok((text::head_and_tail(&out, MAX_RESULT_BYTES), format!("#{} {}", v.get("number").and_then(Value::as_i64).unwrap_or(0), s(&v, "state").to_ascii_lowercase())))
}

// ---------------------------------------------------------------- consequence

pub(crate) fn gh_taints(_tools: &BuiltinTools, call: &ToolCallReq) -> bool {
    reads_untrusted(str_arg(&call.args, "op").unwrap_or(""))
}

/// A network read: free, but a host not yet approved asks once untrusted content is in.
pub(crate) fn net_read(what: &str, exposure: &Exposure) -> Consequence {
    let host = host();
    if exposure.tainted && !exposure.approved_hosts.contains(&host) {
        return Consequence::Ask { summary: format!("{what} {host}: a new host, after untrusted content") };
    }
    Consequence::Free
}

/// Posting (a PR, a comment, a reply, a review) asks every time: a session allow keyed on `gh`
/// never covers it.
pub(crate) fn gh_consequence(tools: &BuiltinTools, call: &ToolCallReq, exposure: &Exposure) -> Consequence {
    if policy::deny_structured("gh", &call.args).is_some() {
        return Consequence::Free;
    }
    let args = &call.args;
    let op = str_arg(args, "op").unwrap_or("");
    if !posts(op) {
        return net_read(&format!("gh {op}"), exposure);
    }
    let target = match number(args) {
        Ok(Some(n)) => format!("PR #{}", n.rsplit('/').next().unwrap_or(&n)),
        _ => "the current branch's PR".to_string(),
    };
    match op {
        "pr_create" => {
            if flag(args, "dry_run") {
                return Consequence::Free;
            }
            let env = super::Env::of(tools);
            let Ok(dir) = env.dir(None, str_arg(args, "cwd")) else { return Consequence::Free };
            let cancel = || false;
            let c = Ctx { env: &env, dir: &dir, cancel: &cancel };
            match pr::plan(&c, args) {
                Ok(plan) => Consequence::AskEvery { summary: plan.summary() },
                // The call reports what is missing; asking first would only be noise.
                Err(_) => Consequence::Free,
            }
        }
        "pr_comment" => Consequence::AskEvery { summary: format!("post a comment on {target}: {}", first_line(str_arg(args, "body").unwrap_or(""), 80)) },
        "pr_reply" => Consequence::AskEvery {
            summary: format!("reply on {target} (comment {}): {}", int_arg(args, "comment_id").unwrap_or(0), first_line(str_arg(args, "body").unwrap_or(""), 80)),
        },
        _ => {
            let n = args.get("comments").and_then(Value::as_array).map_or(0, Vec::len);
            Consequence::AskEvery {
                summary: format!(
                    "post a {} review on {target} with {n} inline comments: {}",
                    str_arg(args, "event").unwrap_or("comment"),
                    first_line(str_arg(args, "body").unwrap_or(""), 60)
                ),
            }
        }
    }
}

pub(crate) fn describe(call: &ToolCallReq) -> String {
    let a = &call.args;
    let op = str_arg(a, "op").unwrap_or("");
    let detail = match op {
        "pr_create" => first_line(str_arg(a, "title").unwrap_or(""), 50),
        _ => match number(a) {
            Ok(Some(n)) => format!("#{}", n.rsplit('/').next().unwrap_or(&n)),
            _ => String::new(),
        },
    };
    if detail.is_empty() {
        format!("gh {op}")
    } else {
        format!("gh {op} {detail}")
    }
}
