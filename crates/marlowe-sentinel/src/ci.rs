//! CI for pushed branches (design G8, 3.9): `NoticeSource::Ci`.
//!
//! The git tools record every pushed branch in `<home>\ci_watch.json` (`{"v":1,"watch":[{repo,
//! branch,sha,remote,slug,at_ms}]}`; `marlowe-exec/src/git/watch.rs` writes it, this only reads
//! it). Each tick looks at the ledger and, for each branch whose CI has not settled, asks `gh run
//! list --branch B --commit SHA` on a worker thread, so the engine never waits on a child. Runs
//! are polled every 60 s while any is still going, and a branch with no run yet gets ten minutes
//! to start one. When every run for the pushed commit has finished a single notice is raised:
//! failed (attention), passed (good) or cancelled (info). Read-only like every other probe: `gh
//! run list` changes nothing, and the ledger is only read.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::{unbounded, Receiver, Sender};
use marlowe_proto::{NoticeSource, Severity};
use serde_json::Value;

use crate::notice::NoticeDraft;
use crate::probe::{Probe, ProbeContext};

/// Entries older than this are ignored.
const HORIZON_MS: i64 = 6 * 3_600_000;
/// While any run is going.
const RUNNING_POLL_MS: i64 = 60_000;
/// A branch with no run yet may still be about to start one.
const NO_RUN_GRACE_MS: i64 = 10 * 60_000;
const NO_RUN_POLL_MS: i64 = 60_000;
/// After `gh` fails (not installed, not signed in, offline).
const ERROR_POLL_MS: i64 = 5 * 60_000;
const MAX_ERRORS: u32 = 3;
const GH_TIMEOUT: Duration = Duration::from_secs(30);
const GH_ENV: &[&str] = &["GH_TOKEN", "GITHUB_TOKEN", "GH_ENTERPRISE_TOKEN", "GITHUB_ENTERPRISE_TOKEN"];

/// Starts the CI probe.
pub fn probe(cx: &ProbeContext<'_>) -> Box<dyn Probe> {
    Box::new(CiProbe::new(cx.home.to_path_buf(), Arc::new(GhCli::new(None))))
}

/// One workflow run, as `gh run list --json` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunInfo {
    pub id: i64,
    pub workflow: String,
    pub title: String,
    pub status: String,
    pub conclusion: String,
    pub url: String,
    pub sha: String,
}

/// Lists the runs of a pushed commit. Production: `gh`; tests: a closure.
pub trait RunLister: Send + Sync {
    fn runs(&self, repo: &Path, branch: &str, sha: &str) -> Result<Vec<RunInfo>, String>;
}

impl<F> RunLister for F
where
    F: Fn(&Path, &str, &str) -> Result<Vec<RunInfo>, String> + Send + Sync,
{
    fn runs(&self, repo: &Path, branch: &str, sha: &str) -> Result<Vec<RunInfo>, String> {
        self(repo, branch, sha)
    }
}

/// `gh run list`, started through `marlowe_os::child` (scrubbed environment, kill-on-close job;
/// only GitHub's own tokens are allowed through).
pub struct GhCli {
    command: Vec<String>,
}

impl GhCli {
    /// `command` is the program and its leading arguments; `None` is `gh` on PATH.
    pub fn new(command: Option<Vec<String>>) -> Self {
        Self { command: command.filter(|c| !c.is_empty()).unwrap_or_else(|| vec!["gh".into()]) }
    }
}

impl RunLister for GhCli {
    fn runs(&self, repo: &Path, branch: &str, sha: &str) -> Result<Vec<RunInfo>, String> {
        let mut cmd = std::process::Command::new(&self.command[0]);
        cmd.args(&self.command[1..]).args(["run", "list", "--branch", branch, "--commit", sha, "--limit", "30", "--json"]);
        cmd.arg("databaseId,workflowName,displayTitle,status,conclusion,headSha,url");
        let spec = marlowe_os::ChildSpec::new()
            .with_env_allow(GH_ENV.iter().map(|s| s.to_string()))
            .with_env(
                [("GH_PROMPT_DISABLED", "1"), ("NO_COLOR", "1"), ("GH_NO_UPDATE_NOTIFIER", "1"), ("GIT_TERMINAL_PROMPT", "0")]
                    .map(|(k, v)| (k.to_string(), v.to_string())),
            )
            .with_cwd(repo);
        let limits = marlowe_os::child::RunLimits { timeout: GH_TIMEOUT, max_output_bytes: 1 << 20 };
        let out = marlowe_os::child::run(cmd, &spec, limits, &|| false).map_err(|e| e.to_string())?;
        if out.timed_out {
            return Err("gh run list timed out".into());
        }
        if out.code() != 0 {
            let said = String::from_utf8_lossy(&out.stderr);
            return Err(said.lines().next().unwrap_or("gh run list failed").trim().to_string());
        }
        parse_runs(&String::from_utf8_lossy(&out.stdout))
    }
}

/// The JSON array `gh run list --json` prints.
pub fn parse_runs(json: &str) -> Result<Vec<RunInfo>, String> {
    let v: Value = serde_json::from_str(json.trim()).map_err(|e| format!("gh run list gave unreadable output: {e}"))?;
    let s = |r: &Value, k: &str| r.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    Ok(v.as_array()
        .map(|a| {
            a.iter()
                .map(|r| RunInfo {
                    id: r.get("databaseId").and_then(Value::as_i64).unwrap_or(0),
                    workflow: s(r, "workflowName"),
                    title: s(r, "displayTitle"),
                    status: s(r, "status").to_ascii_lowercase(),
                    conclusion: s(r, "conclusion").to_ascii_lowercase(),
                    url: s(r, "url"),
                    sha: s(r, "headSha"),
                })
                .collect()
        })
        .unwrap_or_default())
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Key {
    repo: String,
    branch: String,
    sha: String,
}

#[derive(Debug, Clone)]
struct Watch {
    key: Key,
    slug: Option<String>,
    at_ms: i64,
}

/// What the worker found for one branch.
struct Polled {
    key: Key,
    result: Result<Vec<RunInfo>, String>,
}

#[derive(Debug, Default)]
struct Track {
    next_poll_ms: i64,
    errors: u32,
    done: bool,
}

pub struct CiProbe {
    home: PathBuf,
    lister: Arc<dyn RunLister>,
    tracks: HashMap<Key, Track>,
    in_flight: HashSet<Key>,
    slugs: HashMap<Key, (Option<String>, i64)>,
    jobs: Option<Sender<Key>>,
    results: Option<Receiver<Polled>>,
    explained_error: bool,
}

impl CiProbe {
    pub fn new(home: PathBuf, lister: Arc<dyn RunLister>) -> Self {
        Self {
            home,
            lister,
            tracks: HashMap::new(),
            in_flight: HashSet::new(),
            slugs: HashMap::new(),
            jobs: None,
            results: None,
            explained_error: false,
        }
    }

    fn ledger(&self, now_ms: i64) -> Vec<Watch> {
        let Some(v) = std::fs::read_to_string(self.home.join("ci_watch.json")).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) else {
            return Vec::new();
        };
        v.get("watch")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|e| {
                        let s = |k: &str| e.get(k).and_then(Value::as_str).map(str::to_string);
                        let at_ms = e.get("at_ms").and_then(Value::as_i64)?;
                        (now_ms - at_ms < HORIZON_MS).then_some(())?;
                        Some(Watch { key: Key { repo: s("repo")?, branch: s("branch")?, sha: s("sha")? }, slug: s("slug"), at_ms })
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Starts the worker thread on first use.
    fn worker(&mut self) -> Option<&Sender<Key>> {
        if self.jobs.is_none() {
            let (job_tx, job_rx) = unbounded::<Key>();
            let (res_tx, res_rx) = unbounded::<Polled>();
            let lister = self.lister.clone();
            let spawned = std::thread::Builder::new().name("marlowe-ci-probe".into()).spawn(move || {
                for key in job_rx {
                    let result = lister.runs(Path::new(&key.repo), &key.branch, &key.sha);
                    if res_tx.send(Polled { key, result }).is_err() {
                        break;
                    }
                }
            });
            if spawned.is_err() {
                return None;
            }
            self.jobs = Some(job_tx);
            self.results = Some(res_rx);
        }
        self.jobs.as_ref()
    }

    fn settle(&mut self, polled: Polled, now_ms: i64, out: &mut Vec<NoticeDraft>) {
        let Polled { key, result } = polled;
        self.in_flight.remove(&key);
        let (slug, at_ms) = self.slugs.get(&key).cloned().unwrap_or((None, now_ms));
        let track = self.tracks.entry(key.clone()).or_default();
        let runs = match result {
            Err(e) => {
                track.errors += 1;
                track.next_poll_ms = now_ms + ERROR_POLL_MS;
                if track.errors >= MAX_ERRORS {
                    track.done = true;
                }
                if !self.explained_error {
                    self.explained_error = true;
                    out.push(NoticeDraft::new(
                        NoticeSource::Ci,
                        Severity::Info,
                        "unreadable",
                        "CI status could not be read",
                        format!("gh said: {e}. If it is not signed in, run `gh auth login`."),
                        "gh",
                    ));
                }
                return;
            }
            Ok(r) => r,
        };
        track.errors = 0;
        let runs: Vec<RunInfo> = runs.into_iter().filter(|r| r.sha.is_empty() || r.sha == key.sha).collect();
        if runs.is_empty() {
            if now_ms - at_ms > NO_RUN_GRACE_MS {
                track.done = true;
            } else {
                track.next_poll_ms = now_ms + NO_RUN_POLL_MS;
            }
            return;
        }
        if runs.iter().any(|r| r.status != "completed") {
            track.next_poll_ms = now_ms + RUNNING_POLL_MS;
            return;
        }
        track.done = true;
        out.push(verdict_notice(&key, slug.as_deref(), &runs));
    }
}

fn is_failure(r: &RunInfo) -> bool {
    matches!(r.conclusion.as_str(), "failure" | "timed_out" | "startup_failure" | "action_required")
}

/// The notice for a settled set of runs.
fn verdict_notice(key: &Key, slug: Option<&str>, runs: &[RunInfo]) -> NoticeDraft {
    let repo_name = slug.map(str::to_string).unwrap_or_else(|| {
        Path::new(&key.repo).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| key.repo.clone())
    });
    let subject = format!("{repo_name}@{}", key.branch);
    let short: String = key.sha.chars().take(7).collect();
    let failed: Vec<&RunInfo> = runs.iter().filter(|r| is_failure(r)).collect();
    if !failed.is_empty() {
        let names: Vec<String> = failed.iter().map(|r| r.workflow.clone()).collect();
        let first = failed[0];
        return NoticeDraft::new(
            NoticeSource::Ci,
            Severity::Attention,
            "failed",
            format!("CI failed on {} ({repo_name})", key.branch),
            format!(
                "{} of {} runs failed at {short}: {} · {} · `ci_status` with log reads the failure",
                failed.len(),
                runs.len(),
                names.join(", "),
                first.url
            ),
            subject,
        );
    }
    if runs.iter().any(|r| r.conclusion == "cancelled") {
        return NoticeDraft::new(
            NoticeSource::Ci,
            Severity::Info,
            "cancelled",
            format!("CI was cancelled on {} ({repo_name})", key.branch),
            format!("{} runs at {short}, at least one cancelled", runs.len()),
            subject,
        );
    }
    NoticeDraft::new(
        NoticeSource::Ci,
        Severity::Good,
        "passed",
        format!("CI passed on {} ({repo_name})", key.branch),
        format!("{} runs at {short}, all green", runs.len()),
        subject,
    )
}

impl Probe for CiProbe {
    fn name(&self) -> &str {
        "ci"
    }

    fn tick(&mut self, now_ms: i64, _roots: &[PathBuf]) -> Vec<NoticeDraft> {
        let mut out = Vec::new();
        // What the worker finished since the last tick.
        let finished: Vec<Polled> = self.results.as_ref().map(|rx| rx.try_iter().collect()).unwrap_or_default();
        for p in finished {
            self.settle(p, now_ms, &mut out);
        }
        // What the ledger says now.
        let watches = self.ledger(now_ms);
        let mut due: Vec<Key> = Vec::new();
        for w in watches {
            self.slugs.insert(w.key.clone(), (w.slug.clone(), w.at_ms));
            let track = self.tracks.entry(w.key.clone()).or_default();
            if !track.done && track.next_poll_ms <= now_ms && !self.in_flight.contains(&w.key) {
                due.push(w.key);
            }
        }
        for key in due {
            if let Some(tx) = self.worker() {
                if tx.send(key.clone()).is_ok() {
                    self.in_flight.insert(key);
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests;
