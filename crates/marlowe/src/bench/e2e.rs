//! The suites that run the whole product: memory across a restart, research citations over local
//! notes, and the micro coding tasks. Each starts `marlowe -p --standalone` (or the Python
//! harness) as a child, so the numbers are for the shipped path: persona, tools, daemon,
//! repair, memory. Children run under a private `MARLOWE_HOME` so the bench never writes into
//! Matthew's memory or sessions.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use marlowe_os::child::{self, ChildSpec, RunLimits};
use regex::Regex;
use serde::Serialize;
use serde_json::Value;

/// What a child produced.
#[derive(Debug, Clone, Default)]
pub struct Out {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

/// Starts a child. The real one goes through `marlowe_os::child` (scrubbed environment, Job
/// Object, tree kill on timeout); the tests' stands in for `marlowe -p`.
pub trait Spawn {
    fn run(&self, argv: &[String], env: &[(String, String)], cwd: &Path, timeout: Duration) -> Result<Out, String>;
}

pub struct RealSpawn;

impl Spawn for RealSpawn {
    fn run(&self, argv: &[String], env: &[(String, String)], cwd: &Path, timeout: Duration) -> Result<Out, String> {
        let (program, rest) = argv.split_first().ok_or("empty command")?;
        let mut cmd = Command::new(program);
        cmd.args(rest).current_dir(cwd);
        let spec = ChildSpec::new().with_env(env.iter().cloned());
        let out = child::run(cmd, &spec, RunLimits { timeout, max_output_bytes: 16 << 20 }, &|| false).map_err(|e| e.to_string())?;
        Ok(Out {
            code: out.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            timed_out: out.timed_out,
        })
    }
}

/// Where and as whom the children run.
pub struct Setup<'a> {
    pub exe: &'a Path,
    /// The private profile home for the run.
    pub home: &'a Path,
    pub workspace: &'a Path,
    pub model: &'a str,
    pub timeout: Duration,
}

/// What one `marlowe -p --output-format json` turn came to.
#[derive(Debug, Clone, Default)]
pub struct Turn {
    pub text: String,
    pub is_error: bool,
    pub ms: u128,
}

/// One headless turn in a fresh process.
pub fn turn(sp: &dyn Spawn, s: &Setup<'_>, prompt: &str) -> Result<Turn, String> {
    let argv: Vec<String> = [
        &*s.exe.to_string_lossy(),
        "-p",
        "--standalone",
        "--output-format",
        "json",
        "--model",
        s.model,
        "--permission-mode",
        "bypass",
        "--workspace",
        &*s.workspace.to_string_lossy(),
        "--max-steps",
        "12",
        prompt,
    ]
    .iter()
    .map(|a| a.to_string())
    .collect();
    let env = [("MARLOWE_HOME".to_string(), s.home.to_string_lossy().into_owned())];
    let started = Instant::now();
    let out = sp.run(&argv, &env, s.workspace, s.timeout)?;
    let ms = started.elapsed().as_millis();
    if out.timed_out {
        return Err(format!("timed out after {}s", s.timeout.as_secs()));
    }
    let json = out.stdout.lines().rev().find_map(|l| serde_json::from_str::<Value>(l.trim()).ok().filter(Value::is_object)).or_else(|| serde_json::from_str::<Value>(out.stdout.trim()).ok());
    let Some(v) = json else {
        return Err(format!("exit {} without a result: {}", out.code, out.stderr.lines().last().unwrap_or("").trim()));
    };
    Ok(Turn {
        text: v.get("result").and_then(Value::as_str).unwrap_or("").to_string(),
        is_error: v.get("is_error").and_then(Value::as_bool).unwrap_or(false),
        ms,
    })
}

// ---- memory -----------------------------------------------------------------------------

pub struct Fact {
    pub statement: &'static str,
    pub question: &'static str,
    pub answer: &'static str,
}

pub const FACTS: [Fact; 6] = [
    Fact { statement: "my current project codename is VELVET-OTTER-41.", question: "What is my current project codename?", answer: "VELVET-OTTER-41" },
    Fact { statement: "my GPU cluster node is called basalt-7.", question: "What is my GPU cluster node called?", answer: "basalt-7" },
    Fact { statement: "I keep the best checkpoint at step 18250.", question: "At which step do I keep the best checkpoint?", answer: "18250" },
    Fact { statement: "my advisor's favourite optimizer is Adafactor.", question: "What is my advisor's favourite optimizer?", answer: "Adafactor" },
    Fact { statement: "the dataset shard I always hold out is shard-0xC3.", question: "Which dataset shard do I always hold out?", answer: "shard-0xC3" },
    Fact { statement: "my weekly sync with the infra team is on Thursdays at 14:30.", question: "When is my weekly sync with the infra team?", answer: "14:30" },
];

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct MemoryReport {
    pub facts: usize,
    /// Turns that stored a fact without an error.
    pub stored: usize,
    /// Facts the model gave back after a restart.
    pub recalled: usize,
    pub recall_rate: f64,
    pub errors: Vec<String>,
    pub mean_ms: f64,
}

/// Tells `n` facts in one process each, then asks for each in a new process (a restart) with
/// the same home.
pub fn memory(sp: &dyn Spawn, s: &Setup<'_>, n: usize) -> MemoryReport {
    let facts = &FACTS[..n.min(FACTS.len())];
    let mut r = MemoryReport { facts: facts.len(), ..MemoryReport::default() };
    let (mut ms, mut turns) = (0u128, 0u32);
    for f in facts {
        match turn(sp, s, &format!("Please remember this about me: {}", f.statement)) {
            Ok(t) if !t.is_error => {
                r.stored += 1;
                ms += t.ms;
                turns += 1;
            }
            Ok(t) => r.errors.push(format!("remember {:?}: {}", f.answer, t.text.chars().take(120).collect::<String>())),
            Err(e) => r.errors.push(format!("remember {:?}: {e}", f.answer)),
        }
    }
    for f in facts {
        match turn(sp, s, &format!("{} Answer with the value only, from what you remember about me.", f.question)) {
            Ok(t) => {
                ms += t.ms;
                turns += 1;
                if t.text.to_lowercase().contains(&f.answer.to_lowercase()) {
                    r.recalled += 1;
                } else {
                    r.errors.push(format!("recall {:?}: answered {:?}", f.answer, t.text.chars().take(120).collect::<String>()));
                }
            }
            Err(e) => r.errors.push(format!("recall {:?}: {e}", f.answer)),
        }
    }
    r.recall_rate = if r.facts == 0 { 0.0 } else { r.recalled as f64 / r.facts as f64 };
    r.mean_ms = if turns == 0 { 0.0 } else { ms as f64 / f64::from(turns) };
    r
}

// ---- research ---------------------------------------------------------------------------

/// The notes the research questions are about (file name, content). Invented facts, so a model
/// cannot answer from training.
pub fn notes() -> BTreeMap<&'static str, String> {
    let optim = "# Optimizer notes\n\nThe Lantern-350M run used a peak learning rate of 3e-4.\nWarmup lasted 2000 steps before the cosine decay began.\nGradient clipping was set to a global norm of 0.8.\nThe weight decay applied to matrices only was 0.07.\nThe run used a batch of 1.2 million tokens per step.\nThe Lantern-1.3B run lowered the peak learning rate to 1.8e-4.\nBoth runs kept Adam beta2 at 0.95.\nLoss spikes were fixed by skipping updates whose gradient norm exceeded 5.0.\nNo run used dropout.\nCheckpoints were written every 500 steps.\n";
    let scaling = "# Scaling notes\n\nThe fitted exponent for loss against compute was 0.048.\nThe fit covered runs from 20M to 1.3B parameters.\nThe irreducible loss term was estimated at 1.69.\nData was repeated at most 4 times for the larger runs.\nThe 1.3B run saw 52 billion tokens.\nThe compute-optimal tokens-per-parameter ratio came out at 38.\nThe fit error on held-out sizes stayed under 1.5 percent.\nThe smallest run, 20M, trained for 4 hours on one GPU.\n";
    let evals = "# Evaluation notes\n\nThe held-out benchmark has 1840 questions.\nScores are exact match after lowercasing and stripping punctuation.\nLantern-350M scored 41.2 on it.\nLantern-1.3B scored 55.7 on it.\nThe baseline without the curriculum scored 38.9.\nEach score is the mean of three seeds.\nThe seed spread was at most 0.9 points.\nContamination checks removed 17 questions before scoring.\n";
    BTreeMap::from([("notes/optimizers.md", optim.to_string()), ("notes/scaling.md", scaling.to_string()), ("notes/evals.md", evals.to_string())])
}

pub struct Question {
    pub text: &'static str,
    pub answer: &'static str,
}

pub const QUESTIONS: [Question; 5] = [
    Question { text: "What peak learning rate did the Lantern-350M run use, and how long was its warmup?", answer: "3e-4" },
    Question { text: "How many tokens did the 1.3B run see, and what was the compute-optimal tokens-per-parameter ratio?", answer: "52 billion" },
    Question { text: "What did Lantern-1.3B score on the held-out benchmark, and how many questions does it have?", answer: "55.7" },
    Question { text: "What gradient norm threshold triggered skipping an update?", answer: "5.0" },
    Question { text: "What irreducible loss term did the scaling fit estimate?", answer: "1.69" },
];

/// A citation the model wrote: `[notes/evals.md:4] "Lantern-1.3B scored 55.7 on it."`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Citation {
    pub file: String,
    pub line: usize,
    pub quote: String,
}

pub fn parse_citations(answer: &str) -> Vec<Citation> {
    let re = Regex::new(r#"\[([\w./\\-]+):(\d+)(?:-(\d+))?\]\s*[:,-]?\s*(?:"([^"]+)"|“([^”]+)”|'([^']+)')"#).expect("static pattern");
    re.captures_iter(answer)
        .filter_map(|c| {
            let quote = c.get(4).or_else(|| c.get(5)).or_else(|| c.get(6))?.as_str().to_string();
            Some(Citation { file: c[1].replace('\\', "/"), line: c[2].parse().ok()?, quote })
        })
        .collect()
}

fn normal(s: &str) -> String {
    s.to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A citation is verified when its quote is on the cited line or the one either side of it.
pub fn verified(c: &Citation, files: &BTreeMap<&'static str, String>) -> bool {
    let Some(text) = files.iter().find(|(k, _)| k.ends_with(c.file.trim_start_matches("./"))).map(|(_, v)| v) else { return false };
    let lines: Vec<&str> = text.lines().collect();
    if c.line == 0 || c.line > lines.len() {
        return false;
    }
    let lo = c.line.saturating_sub(2);
    let hi = (c.line + 1).min(lines.len());
    normal(&lines[lo..hi].join(" ")).contains(&normal(&c.quote))
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct ResearchReport {
    pub questions: usize,
    pub answers_correct: usize,
    pub citations: usize,
    pub citations_verified: usize,
    /// Verified over written; 0 when nothing was cited.
    pub citation_accuracy: f64,
    pub answer_rate: f64,
    pub errors: Vec<String>,
}

/// Asks `n` questions over the notes (written into the workspace first) and checks the answers
/// and every quoted citation against the files.
pub fn research(sp: &dyn Spawn, s: &Setup<'_>, n: usize) -> ResearchReport {
    let files = notes();
    for (name, body) in &files {
        let path: PathBuf = s.workspace.join(name);
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = std::fs::write(&path, body) {
            return ResearchReport { errors: vec![format!("cannot write {name}: {e}")], ..ResearchReport::default() };
        }
    }
    let qs = &QUESTIONS[..n.min(QUESTIONS.len())];
    let mut r = ResearchReport { questions: qs.len(), ..ResearchReport::default() };
    for q in qs {
        let prompt = format!(
            "Using only the files under notes/, answer: {} Cite every claim as [file:line] followed by the exact quoted line, for example [notes/x.md:3] \"the quoted line\". Do not cite anything you did not read.",
            q.text
        );
        match turn(sp, s, &prompt) {
            Ok(t) => {
                if t.text.to_lowercase().contains(&q.answer.to_lowercase()) {
                    r.answers_correct += 1;
                }
                let cites = parse_citations(&t.text);
                r.citations += cites.len();
                let ok = cites.iter().filter(|c| verified(c, &files)).count();
                r.citations_verified += ok;
                for c in cites.iter().filter(|c| !verified(c, &files)).take(2) {
                    r.errors.push(format!("unsupported citation [{}:{}] {:?}", c.file, c.line, c.quote.chars().take(60).collect::<String>()));
                }
            }
            Err(e) => r.errors.push(format!("{:?}: {e}", q.text)),
        }
    }
    r.citation_accuracy = if r.citations == 0 { 0.0 } else { r.citations_verified as f64 / r.citations as f64 };
    r.answer_rate = if r.questions == 0 { 0.0 } else { r.answers_correct as f64 / r.questions as f64 };
    r
}

// ---- coding -----------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct CodingReport {
    pub runs: usize,
    pub passed: usize,
    pub pass_rate: f64,
    pub error: Option<String>,
}

/// The harness arm the report card runs: the product as shipped (`eval/coding/mcoding/arms.py`).
const CODING_ARM: &str = "marlowe-full";

/// The micro coding suite through the Python harness (`marlowe eval run`), one run per task.
pub fn coding(sp: &dyn Spawn, exe: &Path, cwd: &Path, model: &str, limit: Option<usize>, timeout: Duration, scratch: &Path) -> CodingReport {
    let summary = scratch.join("coding-summary.json");
    let _ = std::fs::remove_file(&summary);
    let mut argv: Vec<String> =
        ["eval", "run", "--suite", "micro", "--arm", CODING_ARM, "--model", model, "--summary-json"].iter().map(|s| s.to_string()).collect();
    argv.insert(0, exe.to_string_lossy().into_owned());
    argv.push(summary.to_string_lossy().into_owned());
    if let Some(n) = limit {
        argv.push("--limit".into());
        argv.push(n.to_string());
    }
    let out = match sp.run(&argv, &[], cwd, timeout) {
        Ok(o) => o,
        Err(e) => return CodingReport { error: Some(e), ..CodingReport::default() },
    };
    let parsed = std::fs::read_to_string(&summary).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok());
    match parsed {
        Some(v) => {
            let runs = v.get("runs").and_then(Value::as_u64).unwrap_or(0) as usize;
            let passed = v.get("passed").and_then(Value::as_u64).unwrap_or(0) as usize;
            CodingReport { runs, passed, pass_rate: if runs == 0 { 0.0 } else { passed as f64 / runs as f64 }, error: None }
        }
        None => CodingReport {
            error: Some(format!("the harness exited {} without a summary: {}", out.code, out.stderr.lines().last().unwrap_or("").trim())),
            ..CodingReport::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Stands in for `marlowe -p`: keeps what it is told across "processes" in a file under the
    /// home, as memory would, and answers questions from it.
    struct FakeMarlowe {
        forget: bool,
        calls: Mutex<Vec<Vec<String>>>,
        cite: fn(&str) -> String,
    }

    fn json_result(text: &str) -> String {
        serde_json::json!({"type": "result", "subtype": "success", "is_error": false, "result": text, "total_cost_usd": 0.0, "exit_code": 0}).to_string()
    }

    impl Spawn for FakeMarlowe {
        fn run(&self, argv: &[String], env: &[(String, String)], _cwd: &Path, _t: Duration) -> Result<Out, String> {
            self.calls.lock().unwrap().push(argv.to_vec());
            let home = PathBuf::from(&env.iter().find(|(k, _)| k == "MARLOWE_HOME").expect("a private home").1);
            let store = home.join("told.txt");
            let prompt = argv.last().unwrap();
            let text = if let Some(fact) = prompt.strip_prefix("Please remember this about me: ") {
                if !self.forget {
                    let mut told = std::fs::read_to_string(&store).unwrap_or_default();
                    told.push_str(fact);
                    told.push('\n');
                    std::fs::write(&store, told).unwrap();
                }
                "Noted.".to_string()
            } else if prompt.contains("notes/") {
                (self.cite)(prompt)
            } else {
                std::fs::read_to_string(&store).unwrap_or_default()
            };
            Ok(Out { code: 0, stdout: format!("{}\n", json_result(&text)), stderr: String::new(), timed_out: false })
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("marlowe-bench-e2e-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn setup<'a>(home: &'a Path, ws: &'a Path, exe: &'a Path) -> Setup<'a> {
        Setup { exe, home, workspace: ws, model: "ollama/test", timeout: Duration::from_secs(5) }
    }

    #[test]
    fn memory_is_told_in_one_process_and_asked_in_another_under_a_private_home() {
        let (home, ws) = (scratch("mem-home"), scratch("mem-ws"));
        let exe = PathBuf::from("marlowe.exe");
        let fake = FakeMarlowe { forget: false, calls: Mutex::new(Vec::new()), cite: |_| String::new() };
        let r = memory(&fake, &setup(&home, &ws, &exe), 4);
        assert_eq!((r.facts, r.stored, r.recalled), (4, 4, 4), "{:?}", r.errors);
        assert_eq!(r.recall_rate, 1.0);
        let calls = fake.calls.lock().unwrap();
        assert_eq!(calls.len(), 8, "one process per remember and per recall");
        assert!(calls.iter().all(|a| a.contains(&"--standalone".to_string()) && a.contains(&"-p".to_string())));
        let forgetful = FakeMarlowe { forget: true, calls: Mutex::new(Vec::new()), cite: |_| String::new() };
        let _ = std::fs::remove_file(home.join("told.txt"));
        let r = memory(&forgetful, &setup(&home, &ws, &exe), 4);
        assert_eq!((r.stored, r.recalled), (4, 0));
        assert_eq!(r.errors.len(), 4);
    }

    #[test]
    fn citations_are_read_from_prose_and_checked_against_the_files() {
        let files = notes();
        let good = "The peak was 3e-4 [notes/optimizers.md:3] \"The Lantern-350M run used a peak learning rate of 3e-4.\" and warmup 2000 [notes/optimizers.md:4] \"Warmup lasted 2000 steps before the cosine decay began.\"";
        let c = parse_citations(good);
        assert_eq!(c.len(), 2);
        assert!(c.iter().all(|c| verified(c, &files)));
        // Off by one line is tolerated, a made-up quote or file is not.
        assert!(verified(&Citation { file: "notes/evals.md".into(), line: 6, quote: "scored 55.7".into() }, &files));
        assert!(!verified(&Citation { file: "notes/evals.md".into(), line: 3, quote: "scored 99.9".into() }, &files));
        assert!(!verified(&Citation { file: "notes/ghost.md".into(), line: 1, quote: "x".into() }, &files));
        assert!(!verified(&Citation { file: "notes/evals.md".into(), line: 400, quote: "scored 55.7".into() }, &files));
        // Citations without a quote cannot be verified and are not counted as citations.
        assert!(parse_citations("see [notes/evals.md:4]").is_empty());
        assert_eq!(parse_citations("[notes\\evals.md:4] “Lantern-1.3B scored 55.7 on it.”").len(), 1);
    }

    #[test]
    fn research_scores_answers_and_the_share_of_citations_that_hold() {
        let (home, ws) = (scratch("res-home"), scratch("res-ws"));
        let exe = PathBuf::from("marlowe.exe");
        let fake = FakeMarlowe {
            forget: false,
            calls: Mutex::new(Vec::new()),
            cite: |_| "3e-4 [notes/optimizers.md:3] \"The Lantern-350M run used a peak learning rate of 3e-4.\" and 55.7 [notes/evals.md:4] \"Lantern-350M scored 99.9 on it.\"".to_string(),
        };
        let r = research(&fake, &setup(&home, &ws, &exe), 2);
        assert_eq!(r.questions, 2);
        assert_eq!((r.citations, r.citations_verified), (4, 2));
        assert_eq!(r.citation_accuracy, 0.5);
        assert_eq!(r.answers_correct, 1, "only the first question's answer, 3e-4, is in the text");
        assert!(ws.join("notes/evals.md").is_file(), "the notes are written into the workspace");
        assert!(r.errors.iter().any(|e| e.contains("unsupported citation")));
    }

    #[test]
    fn coding_reads_the_harness_summary_or_says_why_not() {
        struct Harness(bool);
        impl Spawn for Harness {
            fn run(&self, argv: &[String], _e: &[(String, String)], _c: &Path, _t: Duration) -> Result<Out, String> {
                assert_eq!(&argv[1..3], ["eval", "run"]);
                if self.0 {
                    let path = argv.iter().position(|a| a == "--summary-json").map(|i| argv[i + 1].clone()).unwrap();
                    std::fs::write(path, r#"{"runs": 8, "passed": 6}"#).unwrap();
                }
                Ok(Out { code: 0, ..Out::default() })
            }
        }
        let dir = scratch("coding");
        let r = coding(&Harness(true), Path::new("marlowe.exe"), &dir, "ollama/test", Some(8), Duration::from_secs(5), &dir);
        assert_eq!((r.runs, r.passed, r.pass_rate), (8, 6, 0.75));
        let r = coding(&Harness(false), Path::new("marlowe.exe"), &dir, "ollama/test", None, Duration::from_secs(5), &dir);
        assert!(r.error.unwrap().contains("without a summary"));
    }

    /// The fake harness above never looks at the arm, so this one asks the real harness: the argv
    /// `coding()` builds must be accepted by `run.py run --dry-run`, which names unknown arms.
    #[test]
    fn coding_names_an_arm_the_real_harness_knows() {
        struct Record(Mutex<Vec<String>>);
        impl Spawn for Record {
            fn run(&self, argv: &[String], _e: &[(String, String)], _c: &Path, _t: Duration) -> Result<Out, String> {
                *self.0.lock().unwrap() = argv.to_vec();
                Ok(Out { code: 0, ..Out::default() })
            }
        }
        let dir = scratch("coding-arm");
        let rec = Record(Mutex::new(Vec::new()));
        coding(&rec, Path::new("marlowe.exe"), &dir, "ollama/test", Some(1), Duration::from_secs(5), &dir);
        let argv = rec.0.lock().unwrap().clone();
        let run_py = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../eval/coding/run.py");
        let mut args: Vec<String> = vec!["-I".into(), run_py.to_string_lossy().into_owned()];
        args.extend(argv[2..].iter().cloned());
        // Drop the summary file (a dry run writes none) and keep everything else.
        if let Some(i) = args.iter().position(|a| a == "--summary-json") {
            args.drain(i..=i + 1);
        }
        args.extend(["--dry-run".into(), "--out".into(), dir.join("harness-out").to_string_lossy().into_owned()]);
        let ran = ["python", "py"].iter().find_map(|py| Command::new(py).args(&args).output().ok());
        let Some(out) = ran else { return }; // no Python on this machine: nothing to ask
        assert!(
            out.status.success(),
            "the harness refused the report card's argv {argv:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(String::from_utf8_lossy(&out.stdout).contains(CODING_ARM));
    }

    #[test]
    fn a_turn_without_a_result_is_an_error_with_the_last_stderr_line() {
        struct Broken;
        impl Spawn for Broken {
            fn run(&self, _a: &[String], _e: &[(String, String)], _c: &Path, _t: Duration) -> Result<Out, String> {
                Ok(Out { code: 1, stdout: "garbage".into(), stderr: "marlowe: model not found\n".into(), timed_out: false })
            }
        }
        let (home, ws) = (scratch("broken-home"), scratch("broken-ws"));
        let exe = PathBuf::from("marlowe.exe");
        let e = turn(&Broken, &setup(&home, &ws, &exe), "hi").unwrap_err();
        assert!(e.contains("model not found"), "{e}");
    }
}
