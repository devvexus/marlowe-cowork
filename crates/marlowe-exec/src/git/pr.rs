//! Opening a pull request: the plan the user approves, and the push plus `gh pr create` that run
//! once they have (design G3, 3.9).
//!
//! The plan is computed from the repository alone (no network): which remote and branch, what
//! base, how many commits and files, the title and the body that will be posted. The loop
//! middleware (`guard.rs`) writes the resolved values back into the call's arguments before the
//! approval gate sees it, so the modal shows exactly what will happen, and `expect_sha` makes the
//! run refuse if the branch moved after the user said yes. One approval covers both the push and
//! the PR. The daemon's `OpenPr` asks once and runs the same call.

use serde_json::{json, Value};

use super::git_tool::{flag, redact_url, register_ci, remote_for, Ctx};
use super::run::{body_arg, WRITE_TIMEOUT};
use super::{policy, watch};
use crate::{first_line, str_arg};
use marlowe_agent::ToolOutcome;

/// Commits listed in a default body and in the approval.
const LIST_MAX: usize = 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Plan {
    pub remote: String,
    /// The remote's URL with any credentials removed.
    pub url: String,
    pub slug: Option<String>,
    pub head: String,
    pub head_sha: String,
    pub base: String,
    pub base_ref: String,
    pub title: String,
    pub body: String,
    pub draft: bool,
    pub commits: Vec<String>,
    pub files: usize,
    pub ins: usize,
    pub del: usize,
    /// Changed files in the working tree that the PR will not include.
    pub dirty: usize,
    pub secrets: Vec<String>,
    /// The branch has to be created from `head_sha` first (an already merged crew branch).
    pub create_branch: bool,
}

fn verify(c: &Ctx, rev: &str) -> Option<String> {
    c.out(&["rev-parse", "--verify", "--quiet", &format!("{rev}^{{commit}}")]).filter(|s| !s.is_empty())
}

/// Resolves the PR a `pr_create` call describes, or says what is missing.
pub(crate) fn plan(c: &Ctx, args: &Value) -> Result<Plan, String> {
    if c.toplevel().is_none() {
        return Err("not inside a git repository".into());
    }
    let title = str_arg(args, "title").map(str::trim).unwrap_or("");
    if title.is_empty() {
        return Err("pr_create needs `title`".into());
    }
    let current = c.branch();
    let head = str_arg(args, "branch")
        .or_else(|| str_arg(args, "head"))
        .filter(|h| !h.is_empty())
        .map(String::from)
        .or_else(|| current.clone())
        .ok_or("HEAD is detached: switch to the branch to propose, or name it in `branch`")?;
    let (head_sha, create_branch) = match verify(c, &format!("refs/heads/{head}")) {
        Some(sha) => (sha, false),
        None => match str_arg(args, "sha").or_else(|| str_arg(args, "head_sha")).and_then(|s| verify(c, s)) {
            Some(sha) => (sha, true),
            None => return Err(format!("there is no branch `{head}` here")),
        },
    };
    let remote = remote_for(c, args, &head);
    let raw_url = c.out(&["remote", "get-url", &remote]).ok_or_else(|| format!("no remote named `{remote}` (git remote -v lists them)"))?;
    let slug = watch::github_slug(&raw_url);
    let base = match str_arg(args, "base").filter(|b| !b.is_empty()) {
        Some(b) => b.to_string(),
        None => c
            .out(&["symbolic-ref", "--short", &format!("refs/remotes/{remote}/HEAD")])
            .and_then(|s| s.strip_prefix(&format!("{remote}/")).map(String::from))
            .or_else(|| {
                ["main", "master", "develop"]
                    .into_iter()
                    .find(|b| verify(c, &format!("refs/remotes/{remote}/{b}")).is_some())
                    .map(String::from)
            })
            .ok_or("cannot tell the base branch: name it in `base` (or `git fetch` so the remote's default is known)")?,
    };
    if base == head {
        return Err(format!(
            "`{head}` is the base branch itself: a pull request needs a feature branch. \
             Create one with git{{op:\"switch\", name:\"feat/...\", create:true}}, commit there, then open the PR"
        ));
    }
    let base_ref = if verify(c, &format!("refs/remotes/{remote}/{base}")).is_some() {
        format!("{remote}/{base}")
    } else if verify(c, &format!("refs/heads/{base}")).is_some() {
        base.clone()
    } else {
        return Err(format!("the base branch `{base}` is not known here; `git fetch {remote}` first"));
    };
    let range = format!("{base_ref}..{head_sha}");
    let subjects = c.out(&["log", "--format=%s", &range]).unwrap_or_default();
    let commits: Vec<String> = subjects.lines().map(str::to_string).collect();
    if commits.is_empty() {
        return Err(format!("`{head}` has no commits beyond `{base_ref}`, so there is nothing to propose"));
    }
    let tri = format!("{base_ref}...{head_sha}");
    let numstat = c.out(&["diff", "--numstat", "--no-ext-diff", "--no-textconv", &tri]).unwrap_or_default();
    let (mut files, mut ins, mut del) = (0usize, 0usize, 0usize);
    let mut secrets: Vec<String> = Vec::new();
    for row in numstat.lines() {
        let mut p = row.splitn(3, '\t');
        let (a, d, name) = (p.next().unwrap_or("0"), p.next().unwrap_or("0"), p.next().unwrap_or(""));
        files += 1;
        ins += a.parse::<usize>().unwrap_or(0);
        del += d.parse::<usize>().unwrap_or(0);
        if let Some(what) = policy::secret_path(name) {
            secrets.push(format!("{name} ({what})"));
        }
    }
    if let Some(d) = c.out(&["diff", "-U0", "--no-color", "--no-ext-diff", "--no-textconv", &tri]) {
        let d = &d[..crate::text::floor_boundary(&d, 600_000)];
        for (file, what) in policy::secret_content(d) {
            let s = format!("{file} ({what})");
            if !secrets.contains(&s) {
                secrets.push(s);
            }
        }
    }
    let dirty = if current.as_deref() == Some(head.as_str()) {
        c.out(&["status", "--porcelain"]).map(|s| s.lines().count()).unwrap_or(0)
    } else {
        0
    };
    let body = match str_arg(args, "body") {
        Some(b) if !b.trim().is_empty() => b.trim().to_string(),
        _ => default_body(&commits),
    };
    Ok(Plan {
        remote,
        url: redact_url(&raw_url),
        slug,
        head,
        head_sha,
        base,
        base_ref,
        title: title.to_string(),
        body,
        draft: flag(args, "draft"),
        commits,
        files,
        ins,
        del,
        dirty,
        secrets,
        create_branch,
    })
}

fn default_body(commits: &[String]) -> String {
    let mut lines: Vec<String> = commits.iter().rev().take(LIST_MAX).map(|s| format!("- {s}")).collect();
    if commits.len() > LIST_MAX {
        lines.push(format!("- … and {} more commits", commits.len() - LIST_MAX));
    }
    lines.join("\n")
}

impl Plan {
    /// The approval line: what is pushed where, and what is opened.
    pub fn summary(&self) -> String {
        let target = self.slug.clone().unwrap_or_else(|| self.url.clone());
        let mut s = format!(
            "open PR \"{}\": push {} to {} ({}) and propose it into {}",
            first_line(&self.title, 70),
            self.head,
            self.remote,
            target,
            self.base
        );
        if self.draft {
            s.push_str(" as a draft");
        }
        if !self.secrets.is_empty() {
            s.push_str(&format!("; WARNING secret-shaped in the diff: {}", self.secrets.join(", ")));
        }
        s
    }

    /// The whole plan, as the dry run prints it.
    pub fn render(&self) -> String {
        let mut out = String::from("Pull request to open (one approval covers the push and the PR):\n");
        out.push_str(&format!("  remote: {} ({})\n", self.remote, self.url));
        out.push_str(&format!(
            "  branch: {} @ {} ({} commits, {} files, +{} −{})\n",
            self.head,
            &self.head_sha[..self.head_sha.len().min(7)],
            self.commits.len(),
            self.files,
            self.ins,
            self.del
        ));
        out.push_str(&format!("  commit: {}\n", self.head_sha));
        out.push_str(&format!("  into:   {} ({}){}\n", self.base, self.base_ref, if self.draft { ", as a draft" } else { "" }));
        out.push_str(&format!("  title:  {}\n  body:\n", self.title));
        for l in self.body.lines() {
            out.push_str(&format!("    {l}\n"));
        }
        if self.dirty > 0 {
            out.push_str(&format!("  note: {} uncommitted changes in the working tree are not part of the PR\n", self.dirty));
        }
        for s in &self.secrets {
            out.push_str(&format!("  WARNING: secret-shaped content in the diff: {s}\n"));
        }
        out
    }

    /// `args` with everything this plan resolved filled in, for the approval modal and for
    /// `expect_sha`.
    pub fn materialize(&self, args: &Value) -> Value {
        let mut v = args.clone();
        if let Some(o) = v.as_object_mut() {
            o.insert("remote".into(), json!(self.remote));
            o.insert("branch".into(), json!(self.head));
            o.insert("base".into(), json!(self.base));
            o.insert("title".into(), json!(self.title));
            o.insert("body".into(), json!(self.body));
            o.insert("expect_sha".into(), json!(self.head_sha));
            if self.draft {
                o.insert("draft".into(), json!(true));
            }
        }
        v
    }
}

/// `pr_create`: with `dry_run` the plan, else push and open.
pub(crate) fn create(c: &Ctx, args: &Value) -> ToolOutcome {
    let plan = match plan(c, args) {
        Ok(p) => p,
        Err(e) => return ToolOutcome::error(e),
    };
    if flag(args, "dry_run") {
        return ToolOutcome::ok(plan.render(), "plan");
    }
    if let Some(expected) = str_arg(args, "expect_sha").filter(|s| !s.is_empty()) {
        if expected != plan.head_sha {
            return ToolOutcome::error(format!(
                "`{}` moved after the plan was shown (approved {}, now {}); nothing was pushed. Ask again to propose what is there now",
                plan.head,
                expected.chars().take(7).collect::<String>(),
                plan.head_sha.chars().take(7).collect::<String>()
            ));
        }
    }
    if plan.create_branch {
        let o = c.write(&["branch", &plan.head, &plan.head_sha]);
        if !o.ok() {
            return ToolOutcome::error(o.failure("git branch"));
        }
    }
    let spec = format!("refs/heads/{h}:refs/heads/{h}", h = plan.head);
    let push = ["push", "--set-upstream", plan.remote.as_str(), spec.as_str()];
    if let Some(why) = policy::deny_git(&push) {
        return ToolOutcome::error(format!("refused: {why}"));
    }
    let pushed = c.net(&push);
    if !pushed.ok() {
        let mut msg = pushed.failure("git push");
        if pushed.all().contains("rejected") || pushed.all().contains("non-fast-forward") {
            msg.push_str("\n(the remote branch has commits this one lacks: fetch and rebase or merge, then ask again; force pushes are not available)");
        }
        return ToolOutcome::error(msg);
    }
    register_ci(c, args, &plan.head, &plan.remote);
    if plan.title.chars().count() > 256 {
        return ToolOutcome::error("the PR title is over 256 characters; make it one short line");
    }
    let (body_arg, _body_file) = match body_arg(&plan.body) {
        Ok(b) => b,
        Err(e) => return ToolOutcome::error(e),
    };
    let mut gh: Vec<String> = vec![
        "pr".into(),
        "create".into(),
        format!("--title={}", plan.title),
        body_arg,
        format!("--base={}", plan.base),
        format!("--head={}", plan.head),
    ];
    if let Some(slug) = &plan.slug {
        gh.push(format!("--repo={slug}"));
    }
    if plan.draft {
        gh.push("--draft".into());
    }
    let refs: Vec<&str> = gh.iter().map(String::as_str).collect();
    let o = c.env.gh(c.dir, &refs, WRITE_TIMEOUT, c.cancel);
    let said = o.all();
    let url = find_pr_url(&said);
    if !o.ok() {
        if said.contains("already exists") {
            if let Some(url) = url {
                return ToolOutcome::ok(format!("A pull request for {} already exists: {url}\n(the branch was pushed)", plan.head), "already open");
            }
        }
        return ToolOutcome::error(format!(
            "{}\n(`{}` was pushed to {}; only the pull request failed)",
            o.failure("gh pr create"),
            plan.head,
            plan.remote
        ));
    }
    match url {
        Some(url) => {
            let n = url.rsplit('/').next().unwrap_or("").to_string();
            ToolOutcome::ok(
                format!("Opened pull request {url}\npushed {} to {} and proposed it into {}", plan.head, plan.remote, plan.base),
                format!("PR #{n}"),
            )
        }
        None => ToolOutcome::ok(format!("gh pr create finished:\n{said}"), "PR opened"),
    }
}

/// The pull request URL in `gh`'s output, if any.
pub(crate) fn find_pr_url(text: &str) -> Option<String> {
    text.split_whitespace()
        .filter_map(|w| {
            let w = w.trim_matches(|c: char| matches!(c, '"' | '\'' | '<' | '>' | '(' | ')' | ',' | '.'));
            (w.starts_with("http") && w.contains("/pull/")).then(|| w.to_string())
        })
        .last()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_pull_request_url() {
        assert_eq!(find_pr_url("Creating pull request for feat into main\n\nhttps://github.com/o/r/pull/12\n").as_deref(), Some("https://github.com/o/r/pull/12"));
        assert_eq!(
            find_pr_url("a pull request for branch \"x\" into branch \"main\" already exists:\nhttps://github.com/o/r/pull/7").as_deref(),
            Some("https://github.com/o/r/pull/7")
        );
        assert_eq!(find_pr_url("nothing here"), None);
    }

    #[test]
    fn default_body_lists_commits_oldest_first_and_caps() {
        let commits: Vec<String> = (0..25).map(|i| format!("c{i}")).collect();
        let b = default_body(&commits);
        assert!(b.starts_with("- c24"), "{b}");
        assert!(b.ends_with("… and 5 more commits"), "{b}");
        assert_eq!(default_body(&["fix".into()]), "- fix");
    }
}
