use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use super::*;

const T0: i64 = 1_760_000_000_000;
const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

fn home(name: &str) -> PathBuf {
    let h = std::env::temp_dir().join(format!("marlowe-ci-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&h);
    std::fs::create_dir_all(&h).unwrap();
    h
}

fn ledger(home: &Path, entries: &[(&str, &str, i64)]) {
    let watch: Vec<Value> = entries
        .iter()
        .map(|(branch, sha, at)| {
            serde_json::json!({"repo": "C:\\work\\proj", "branch": branch, "sha": sha, "remote": "origin", "slug": "o/r", "at_ms": at})
        })
        .collect();
    std::fs::write(home.join("ci_watch.json"), serde_json::json!({"v": 1, "watch": watch}).to_string()).unwrap();
}

fn run(workflow: &str, status: &str, conclusion: &str, sha: &str) -> RunInfo {
    RunInfo {
        id: 1,
        workflow: workflow.into(),
        title: "t".into(),
        status: status.into(),
        conclusion: conclusion.into(),
        url: format!("https://github.com/o/r/actions/runs/{workflow}"),
        sha: sha.into(),
    }
}

/// A lister that answers from a queue (the last answer repeats) and counts its calls.
struct Script {
    answers: Mutex<Vec<Result<Vec<RunInfo>, String>>>,
    calls: AtomicUsize,
}

impl Script {
    fn new(answers: Vec<Result<Vec<RunInfo>, String>>) -> Arc<Self> {
        Arc::new(Self { answers: Mutex::new(answers), calls: AtomicUsize::new(0) })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl RunLister for Script {
    fn runs(&self, _repo: &Path, _branch: &str, _sha: &str) -> Result<Vec<RunInfo>, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut a = self.answers.lock().unwrap();
        if a.len() > 1 {
            a.remove(0)
        } else {
            a[0].clone()
        }
    }
}

/// Ticks at `now` until the worker has answered, and returns what was raised (an answer is
/// handled by the tick after it arrives).
fn pump(p: &mut CiProbe, now: i64) -> Vec<NoticeDraft> {
    let mut raised = p.tick(now, &[]);
    let end = Instant::now() + Duration::from_secs(5);
    while !p.in_flight.is_empty() && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(5));
        raised.extend(p.tick(now, &[]));
    }
    raised
}

#[test]
fn parity_g8_a_failed_run_raises_an_attention_notice() {
    let h = home("failed");
    ledger(&h, &[("feat/x", SHA, T0)]);
    let lister = Script::new(vec![Ok(vec![run("build", "completed", "failure", SHA), run("lint", "completed", "success", SHA)])]);
    let mut p = CiProbe::new(h.clone(), lister.clone());
    let notices = pump(&mut p, T0 + 1_000);
    assert_eq!(notices.len(), 1, "{notices:?}");
    let n = &notices[0];
    assert_eq!((n.source, n.severity, n.kind.as_str()), (NoticeSource::Ci, Severity::Attention, "failed"));
    assert_eq!(n.subject, "o/r@feat/x");
    assert!(n.title.contains("CI failed on feat/x"), "{}", n.title);
    assert!(n.detail.contains("1 of 2 runs failed at 0123456") && n.detail.contains("build") && n.detail.contains("actions/runs/build"), "{}", n.detail);
    assert!(n.severity.is_urgent(), "failures are spoken outside quiet hours");
    // Settled: no more polls, no repeat notice.
    assert!(pump(&mut p, T0 + 10 * 60_000).is_empty());
    assert_eq!(lister.calls(), 1);
    // The ledger is only read.
    assert!(std::fs::read_to_string(h.join("ci_watch.json")).unwrap().contains("feat/x"));
}

#[test]
fn a_green_branch_is_good_news_and_only_after_every_run_finished() {
    let h = home("green");
    ledger(&h, &[("main", SHA, T0)]);
    let lister = Script::new(vec![
        Ok(vec![run("build", "completed", "success", SHA), run("test", "in_progress", "", SHA)]),
        Ok(vec![run("build", "completed", "success", SHA), run("test", "completed", "success", SHA)]),
    ]);
    let mut p = CiProbe::new(h, lister.clone());
    assert!(pump(&mut p, T0 + 1_000).is_empty(), "one run is still going");
    // Not due again for a minute.
    assert!(pump(&mut p, T0 + 30_000).is_empty());
    assert_eq!(lister.calls(), 1);
    let notices = pump(&mut p, T0 + 62_000);
    assert_eq!(lister.calls(), 2);
    assert_eq!(notices.len(), 1);
    assert_eq!((notices[0].severity, notices[0].kind.as_str()), (Severity::Good, "passed"));
    assert!(!notices[0].severity.is_urgent());
}

#[test]
fn a_cancelled_run_is_info_and_runs_for_other_commits_are_ignored() {
    let h = home("cancelled");
    ledger(&h, &[("b", SHA, T0)]);
    let other = "ffffffffffffffffffffffffffffffffffffffff";
    let lister = Script::new(vec![Ok(vec![run("build", "completed", "cancelled", SHA), run("old", "completed", "failure", other)])]);
    let mut p = CiProbe::new(h, lister);
    let notices = pump(&mut p, T0 + 1_000);
    assert_eq!((notices[0].severity, notices[0].kind.as_str()), (Severity::Info, "cancelled"), "{notices:?}");
}

#[test]
fn a_branch_with_no_run_yet_gets_ten_minutes_then_is_dropped_quietly() {
    let h = home("norun");
    ledger(&h, &[("b", SHA, T0)]);
    let lister = Script::new(vec![Ok(vec![])]);
    let mut p = CiProbe::new(h, lister.clone());
    assert!(pump(&mut p, T0 + 1_000).is_empty());
    assert!(pump(&mut p, T0 + 62_000).is_empty());
    assert_eq!(lister.calls(), 2, "polled again after a minute");
    assert!(pump(&mut p, T0 + 11 * 60_000).is_empty());
    let calls = lister.calls();
    assert!(pump(&mut p, T0 + 20 * 60_000).is_empty());
    assert_eq!(lister.calls(), calls, "given up");
}

#[test]
fn old_ledger_entries_are_ignored() {
    let h = home("old");
    ledger(&h, &[("b", SHA, T0 - 7 * 3_600_000)]);
    let lister = Script::new(vec![Ok(vec![run("build", "completed", "failure", SHA)])]);
    let mut p = CiProbe::new(h, lister.clone());
    assert!(pump(&mut p, T0).is_empty());
    assert_eq!(lister.calls(), 0);
}

#[test]
fn gh_failures_back_off_explain_once_and_give_up() {
    let h = home("errors");
    ledger(&h, &[("a", SHA, T0), ("b", "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", T0)]);
    let lister = Script::new(vec![Err("To get started with GitHub CLI, please run:  gh auth login".into())]);
    let mut p = CiProbe::new(h, lister.clone());
    let first = pump(&mut p, T0 + 1_000);
    assert_eq!(first.len(), 1, "one explanation for both branches: {first:?}");
    assert_eq!((first[0].kind.as_str(), first[0].severity), ("unreadable", Severity::Info));
    assert!(first[0].detail.contains("gh auth login"));
    assert!(pump(&mut p, T0 + 2 * 60_000).is_empty(), "backing off");
    assert_eq!(lister.calls(), 2);
    for step in 1..=4 {
        assert!(pump(&mut p, T0 + step * 6 * 60_000).is_empty());
    }
    assert_eq!(lister.calls(), 6, "three tries per branch, then done");
}

#[test]
fn a_slow_gh_never_blocks_the_engine_tick() {
    struct Slow;
    impl RunLister for Slow {
        fn runs(&self, _: &Path, _: &str, _: &str) -> Result<Vec<RunInfo>, String> {
            std::thread::sleep(Duration::from_millis(400));
            Ok(vec![run("build", "completed", "success", SHA)])
        }
    }
    let h = home("slow");
    ledger(&h, &[("b", SHA, T0)]);
    let mut p = CiProbe::new(h, Arc::new(Slow));
    let started = Instant::now();
    assert!(p.tick(T0 + 1, &[]).is_empty());
    assert!(started.elapsed() < Duration::from_millis(200), "tick took {:?}", started.elapsed());
    let notices = pump(&mut p, T0 + 1);
    assert_eq!(notices.len(), 1);
}

#[test]
fn run_list_json_parses() {
    let runs = parse_runs(
        r#"[{"databaseId": 7, "workflowName": "CI", "displayTitle": "fix", "status": "COMPLETED", "conclusion": "FAILURE", "headSha": "abc", "url": "u"}]"#,
    )
    .unwrap();
    assert_eq!(
        runs,
        [RunInfo { id: 7, workflow: "CI".into(), title: "fix".into(), status: "completed".into(), conclusion: "failure".into(), url: "u".into(), sha: "abc".into() }]
    );
    assert!(parse_runs("[]").unwrap().is_empty());
    assert!(parse_runs("not json").is_err());
}

#[test]
fn the_real_runner_asks_gh_for_the_commit_and_keeps_only_the_github_token() {
    // A stand-in gh: writes its arguments and two environment variables to a file, answers JSON.
    let h = home("ghcli");
    let script = h.join("fake_gh.py");
    let log = h.join("seen.txt");
    std::fs::write(
        &script,
        format!(
            "import json, os, sys\nopen(r'{}', 'w').write(json.dumps({{'argv': sys.argv[1:], 'cwd': os.getcwd(), 'gh': os.environ.get('GH_TOKEN'), 'or': os.environ.get('OPENROUTER_API_KEY')}}))\nprint(json.dumps([{{'databaseId': 5, 'workflowName': 'CI', 'displayTitle': 't', 'status': 'completed', 'conclusion': 'success', 'headSha': '{SHA}', 'url': 'u'}}]))\n",
            log.display()
        ),
    )
    .unwrap();
    std::env::set_var("GH_TOKEN", "canary-gh");
    std::env::set_var("OPENROUTER_API_KEY", "canary-or");
    let runner = GhCli::new(Some(vec!["python".into(), script.to_string_lossy().into_owned()]));
    let runs = runner.runs(&h, "feat/x", SHA).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].conclusion, "success");
    let seen: Value = serde_json::from_str(&std::fs::read_to_string(&log).unwrap()).unwrap();
    let argv: Vec<&str> = seen["argv"].as_array().unwrap().iter().filter_map(|v| v.as_str()).collect();
    assert_eq!(&argv[..7], ["run", "list", "--branch", "feat/x", "--commit", SHA, "--limit"], "{argv:?}");
    assert_eq!(seen["gh"], "canary-gh", "GitHub's own token is allowed through");
    assert_eq!(seen["or"], Value::Null, "model keys never are");
    // A failing gh reports its first line.
    let failing = GhCli::new(Some(vec![
        "python".into(),
        "-c".into(),
        "import sys; sys.stderr.write('HTTP 401: Bad credentials\\nmore'); sys.exit(1)".into(),
    ]));
    assert_eq!(failing.runs(&h, "b", SHA).unwrap_err(), "HTTP 401: Bad credentials");
    let missing = GhCli::new(Some(vec!["no-such-gh-binary".into()]));
    assert!(missing.runs(&h, "b", SHA).is_err());
}
