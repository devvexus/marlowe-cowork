//! End to end: a synthetic `cowork.db` in a temp home, exported through `run_with`.
//! A11 is `a11_zero_leaks_of_1000_planted_canaries`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use marlowe_crew::{Roles, Run, RunStore, StepRecord};
use marlowe_model::{Message, Part, Role, Thinking, ToolCall, ToolResult, Usage};
use marlowe_proto::{CrewBudget, CrewRunStatus};
use rusqlite::Connection;
use serde_json::{json, Value};

use super::config::ExportConfig;
use super::scrub::{Identity, ScrubInputs};
use super::source::{self, GoodMark};
use super::trajectories::{DPO_FILE, MANIFEST_FILE, SFT_FILE};

// ------------------------------------------------------------------ fixtures

struct TempHome(PathBuf);

impl TempHome {
    fn new(tag: &str) -> TempHome {
        static N: AtomicU32 = AtomicU32::new(0);
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().subsec_nanos();
        let p = std::env::temp_dir().join(format!(
            "marlowe-export-{tag}-{}-{}-{nanos}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&p).unwrap();
        TempHome(p)
    }
    fn path(&self) -> &Path {
        &self.0
    }
    fn dir(&self, name: &str) -> String {
        let d = self.0.join(name);
        std::fs::create_dir_all(&d).unwrap();
        super::config::canonical_dir(&d.to_string_lossy()).unwrap()
    }
}

impl Drop for TempHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The daemon's session tables, as `marlowe-daemon/src/store.rs` creates them.
const SESSION_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sessions (id TEXT PRIMARY KEY, title TEXT NOT NULL, workspace TEXT, created INTEGER NOT NULL,
    updated INTEGER NOT NULL, next_turn INTEGER NOT NULL DEFAULT 0, tainted INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS turns (session TEXT NOT NULL, idx INTEGER NOT NULL, role TEXT NOT NULL, content TEXT NOT NULL,
    at INTEGER NOT NULL, PRIMARY KEY (session, idx));
CREATE TABLE IF NOT EXISTS daemon_kv (key TEXT PRIMARY KEY, value TEXT NOT NULL);
";

fn db(home: &Path) -> Connection {
    let c = Connection::open(marlowe_proto::paths::cowork_db(home)).unwrap();
    c.pragma_update(None, "journal_mode", "WAL").unwrap();
    c.execute_batch(SESSION_SCHEMA).unwrap();
    c
}

fn role_str(r: Role) -> &'static str {
    match r {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

const T0: i64 = 1_760_000_000_000;

fn add_session(c: &Connection, id: &str, title: &str, ws: Option<&str>, tainted: bool, msgs: &[Message]) {
    c.execute(
        "INSERT INTO sessions (id, title, workspace, created, updated, next_turn, tainted) VALUES (?1, ?2, ?3, ?4, ?4, ?5, ?6)",
        rusqlite::params![id, title, ws, T0, msgs.len() as i64, tainted as i64],
    )
    .unwrap();
    for (i, m) in msgs.iter().enumerate() {
        c.execute(
            "INSERT INTO turns (session, idx, role, content, at) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![id, i as i64, role_str(m.role), serde_json::to_string(&m.content).unwrap(), T0],
        )
        .unwrap();
    }
}

fn good(c: &Connection, id: &str) {
    source::set_good(c, id, Some(&GoodMark { upto: None, at_ms: T0 })).unwrap();
}

fn add_run(home: &Path, ws: &str, goal: &str, merged: bool, tainted: bool, steps: Vec<(&str, Vec<Message>)>) -> String {
    let store = RunStore::open(&marlowe_proto::paths::cowork_db(home)).unwrap();
    let roles = Roles::builtin();
    let role = roles.get("engineer").expect("engineer role");
    let mut run = Run::new(None, role, goal, None, Some(PathBuf::from(ws)), CrewBudget::default());
    run.status = if merged { CrewRunStatus::Done } else { CrewRunStatus::Failed };
    run.note = Some(if merged { "merged: fast-forward to 1234567" } else { "failed: tests" }.into());
    run.tainted = tainted;
    store.save(&run).unwrap();
    for (n, (key, messages)) in steps.into_iter().enumerate() {
        store
            .append_step(&StepRecord {
                run: run.id.clone(),
                step: n as u32 + 1,
                key: key.into(),
                role: "engineer".into(),
                action: "worked".into(),
                messages,
                output: Some("done".into()),
                usage: Usage::default(),
                tainted,
                at_ms: T0,
            })
            .unwrap();
    }
    run.id.as_str().to_string()
}

fn user(t: &str) -> Message {
    Message::user(t)
}

fn say(t: &str) -> Message {
    Message::assistant(t)
}

fn call(think: &str, text: &str, calls: Vec<(String, &str, Value)>) -> Message {
    let mut content = Vec::new();
    if !think.is_empty() {
        content.push(Part::Thinking(Thinking { text: think.into(), signature: Some("opaque-signature".into()), details: None }));
    }
    if !text.is_empty() {
        content.push(Part::Text { text: text.into() });
    }
    for (id, name, args) in calls {
        content.push(Part::ToolCall(ToolCall { id, name: name.into(), args }));
    }
    Message { role: Role::Assistant, content }
}

fn results(rs: Vec<(String, String, bool)>) -> Message {
    Message::tool_results(rs.into_iter().map(|(id, c, e)| ToolResult::text(id, c, e)))
}

fn run(home: &Path, args: &[&str], inputs: &ScrubInputs) -> (Result<i32, String>, String) {
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let mut out = Vec::new();
    let r = super::run_with(home, &args, inputs, &mut out);
    (r, String::from_utf8(out).unwrap())
}

fn enable(home: &Path, dir: &str) {
    let (r, out) = run(home, &["enable", dir], &ScrubInputs::default());
    assert_eq!(r, Ok(0), "{out}");
}

/// Every string in a JSONL file, decoded (and tool-call arguments decoded once more).
fn decoded_strings(text: &str) -> String {
    fn walk(v: &Value, out: &mut String) {
        match v {
            Value::String(s) => {
                out.push_str(s);
                out.push('\n');
                if let Ok(inner) = serde_json::from_str::<Value>(s) {
                    if inner.is_object() || inner.is_array() {
                        walk(&inner, out);
                    }
                }
            }
            Value::Array(a) => a.iter().for_each(|x| walk(x, out)),
            Value::Object(o) => o.iter().for_each(|(k, x)| {
                out.push_str(k);
                out.push('\n');
                walk(x, out)
            }),
            _ => {}
        }
    }
    let mut out = String::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        walk(&serde_json::from_str(line).expect("valid JSONL"), &mut out);
    }
    out
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_default()
}

// ------------------------------------------------------------------ canaries

/// SplitMix64: deterministic, so a leak is reproducible.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn pick(&mut self, alphabet: &str, n: usize) -> String {
        let a = alphabet.as_bytes();
        (0..n).map(|_| a[self.below(a.len() as u64) as usize] as char).collect()
    }
    fn one<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.below(items.len() as u64) as usize]
    }
    /// Base62 with at least one upper, lower and digit.
    fn key(&mut self, n: usize) -> String {
        loop {
            let s = self.pick(B62, n);
            if s.chars().any(|c| c.is_ascii_uppercase()) && s.chars().any(|c| c.is_ascii_lowercase()) && s.chars().any(|c| c.is_ascii_digit()) {
                return s;
            }
        }
    }
    fn word(&mut self, n: usize) -> String {
        let mut w = self.pick(LOWER, n);
        w[..1].make_ascii_uppercase();
        w
    }
}

const B62: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
const B64: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const B64URL: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
const HEX: &str = "0123456789abcdef";
const LOWER: &str = "abcdefghijklmnopqrstuvwxyz";
const UPPER36: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
const DIGITS: &str = "0123456789";
const PW: &str = "ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789!@#^*_-+=.~";

/// One planted canary: the text planted and the secret parts that must not come out.
struct Canary {
    kind: &'static str,
    text: String,
    cores: Vec<String>,
}

/// The machine identity and known secrets the canary corpus plants.
struct World {
    first: String,
    last: String,
    account: String,
    host: String,
    codename: String,
    known: Vec<String>,
}

impl World {
    fn new(rng: &mut Rng) -> World {
        let words = ["violet", "anchor", "mosaic", "harbor", "lantern", "quartz", "meadow", "falcon", "ember", "willow"];
        let known = (0..8)
            .map(|_| format!("{}-{}-{}-{}", rng.one(&words), rng.one(&words), rng.one(&words), rng.pick(DIGITS, 4)))
            .collect();
        World {
            first: rng.word(7),
            last: rng.word(9),
            account: format!("q{}", rng.pick(LOWER, 7)),
            host: format!("WS-{}", rng.pick("ABCDEFGHJKLMNPQRSTUVWXYZ", 6)),
            codename: rng.word(10),
            known,
        }
    }
    fn inputs(&self) -> ScrubInputs {
        ScrubInputs {
            known_secrets: self.known.clone(),
            identity: Identity {
                names: vec![format!("{} {}", self.first, self.last)],
                accounts: vec![self.account.clone()],
                hosts: vec![self.host.clone()],
                emails: vec![],
                homes: vec![format!(r"C:\Users\{}", self.account)],
            },
            ..ScrubInputs::default()
        }
    }
}

fn luhn_card(rng: &mut Rng) -> String {
    let mut d: Vec<u32> = vec![if rng.below(2) == 0 { 4 } else { 5 }];
    d.extend((0..14).map(|_| rng.below(10) as u32));
    let sum: u32 = d
        .iter()
        .rev()
        .enumerate()
        .map(|(i, &x)| if i % 2 == 0 { let y = x * 2; if y > 9 { y - 9 } else { y } } else { x })
        .sum();
    d.push((10 - sum % 10) % 10);
    d.iter().map(|x| char::from_digit(*x, 10).unwrap()).collect()
}

const KINDS: &[&str] = &[
    "openai", "anthropic", "openrouter", "github", "github_pat", "gitlab", "aws_id", "aws_secret", "huggingface",
    "slack", "stripe", "google", "jwt", "pem", "bearer", "basic", "url_creds", "assign", "json_assign", "flag",
    "known", "wandb", "env_hex", "email", "phone", "card", "ssn", "ip", "name", "account_path", "host", "other_user",
    "codename", "entropy", "base64", "azure", "npm", "sendgrid", "slack_hook", "secret_file",
];

fn canary(kind: &'static str, rng: &mut Rng, w: &World) -> Canary {
    let c = |text: String, cores: Vec<String>| Canary { kind, text, cores };
    match kind {
        "openai" => {
            let b = rng.key(48);
            c(format!("OPENAI key sk-proj-{b}"), vec![b])
        }
        "anthropic" => {
            let b = rng.pick(B64URL, 93);
            c(format!("sk-ant-api03-{b}AA"), vec![b])
        }
        "openrouter" => {
            let b = rng.pick(HEX, 64);
            c(format!("router=sk-or-v1-{b}"), vec![b])
        }
        "github" => {
            let b = rng.key(36);
            c(format!("ghp_{b}"), vec![b])
        }
        "github_pat" => {
            let (a, b) = (rng.key(22), rng.key(59));
            c(format!("github_pat_{a}_{b}"), vec![a, b])
        }
        "gitlab" => {
            let b = rng.key(20);
            c(format!("glpat-{b}"), vec![b])
        }
        "aws_id" => {
            let b = rng.pick(UPPER36, 16);
            c(format!("AKIA{b}"), vec![b])
        }
        "aws_secret" => {
            let b = rng.pick(B64, 40);
            c(format!("aws_secret_access_key = {b}"), vec![b])
        }
        "huggingface" => {
            let b = rng.key(34);
            c(format!("HF hf_{b}"), vec![b])
        }
        "slack" => {
            let b = rng.key(24);
            c(format!("xoxb-{}-{}-{b}", rng.pick(DIGITS, 11), rng.pick(DIGITS, 13)), vec![b])
        }
        "stripe" => {
            let b = rng.key(24);
            c(format!("sk_live_{b}"), vec![b])
        }
        "google" => {
            let b = rng.key(35);
            c(format!("AIza{b}"), vec![b])
        }
        "jwt" => {
            let (a, b) = (rng.pick(B64URL, 40), rng.pick(B64URL, 43));
            c(format!("eyJhbGciOiJIUzI1NiJ9.eyJ{a}.{b}"), vec![a, b])
        }
        "pem" => {
            let lines: Vec<String> = (0..3).map(|_| rng.pick(B64, 64)).collect();
            c(format!("-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----", lines.join("\n")), lines)
        }
        "bearer" => {
            let b = rng.key(40);
            c(format!("curl -H \"Authorization: Bearer {b}\" https://api.service.io/v1"), vec![b])
        }
        "basic" => {
            let b = rng.pick(B64, 28);
            c(format!("Authorization: Basic {b}="), vec![b])
        }
        "url_creds" => {
            let (u, p) = (rng.pick(LOWER, 9), rng.key(18));
            c(format!("postgres://{u}:{p}@db.{}.internal:5432/app", rng.pick(LOWER, 6)), vec![u, p])
        }
        "assign" => {
            let p = format!("{}{}", rng.key(4), rng.pick(PW, 14));
            let key = rng.one(&["DB_PASSWORD", "password", "client_secret", "SECRET_KEY", "auth_token", "api-key"]);
            let sep = rng.one(&["=", ": ", " = ", " := ", " => "]);
            c(format!("{key}{sep}{p}"), vec![p])
        }
        "json_assign" => {
            let p = format!("{}{}", rng.key(4), rng.pick(PW, 14));
            let key = rng.one(&["password", "api_key", "clientSecret", "access_token", "privateKey"]);
            c(format!("{{\"{key}\": \"{p}\"}}"), vec![p])
        }
        "flag" => {
            let p = rng.key(24);
            let f = rng.one(&["--token", "--password", "--api-key", "--client-secret"]);
            let sep = rng.one(&[" ", "="]);
            c(format!("tool login {f}{sep}{p}"), vec![p])
        }
        "known" => {
            let k = w.known[rng.below(w.known.len() as u64) as usize].clone();
            c(format!("the vault phrase is {k} for now"), vec![k])
        }
        "wandb" => {
            let b = rng.pick(HEX, 40);
            c(format!("wandb login {b}"), vec![b])
        }
        "env_hex" => {
            let b = rng.pick(HEX, 40);
            c(format!("WANDB_API_KEY={b}"), vec![b])
        }
        "email" => {
            let e = format!("{}.{}@{}.{}", rng.pick(LOWER, 7), rng.pick(LOWER, 5), rng.pick(LOWER, 8), rng.one(&["com", "io", "dev", "net", "org"]));
            c(format!("ping {e} about it"), vec![e])
        }
        "phone" => {
            let (a, e, l) = (200 + rng.below(700), 200 + rng.below(800), rng.pick(DIGITS, 4));
            let text = match rng.below(5) {
                0 => format!("({a}) {e}-{l}"),
                1 => format!("{a}-{e}-{l}"),
                2 => format!("{a}.{e}.{l}"),
                3 => format!("+1 {a} {e} {l}"),
                _ => format!("+44 20 {e}{} {l}", rng.below(10)),
            };
            c(format!("call {text} after six"), vec![text.clone(), format!("{e}-{l}"), format!("{e} {l}")])
        }
        "card" => {
            let n = luhn_card(rng);
            let text = match rng.below(3) {
                0 => n.clone(),
                1 => format!("{} {} {} {}", &n[0..4], &n[4..8], &n[8..12], &n[12..16]),
                _ => format!("{}-{}-{}-{}", &n[0..4], &n[4..8], &n[8..12], &n[12..16]),
            };
            c(format!("card {text} exp 09/29"), vec![text.clone(), n])
        }
        "ssn" => {
            let s = format!("{:03}-{:02}-{:04}", 1 + rng.below(660), 1 + rng.below(98), 1 + rng.below(9998));
            c(format!("ssn {s}"), vec![s])
        }
        "ip" => {
            let first = rng.one(&["23", "34", "45", "52", "63", "74", "81", "93", "104", "151", "185", "212"]);
            let ip = format!("{first}.{}.{}.{}", 1 + rng.below(250), 1 + rng.below(250), 1 + rng.below(250));
            c(format!("ssh into {ip} now"), vec![ip])
        }
        "name" => {
            let text = match rng.below(3) {
                0 => format!("{} {} asked for this", w.first, w.last),
                1 => format!("thanks {}", w.first),
                _ => format!("signed, {}", w.last.to_uppercase()),
            };
            c(text, vec![w.first.clone(), w.last.clone()])
        }
        "account_path" => {
            let text = match rng.below(4) {
                0 => format!(r"C:\Users\{}\Projects\app\src\main.rs", w.account),
                1 => format!("c:/users/{}/AppData/Local/x", w.account),
                2 => format!(r"C:\\Users\\{}\\.cargo\\bin", w.account),
                _ => format!("logged in as {}", w.account),
            };
            c(text, vec![w.account.clone()])
        }
        "host" => c(format!("built on {} last night", w.host), vec![w.host.clone()]),
        "other_user" => {
            let u = format!("z{}", rng.pick(LOWER, 7));
            let text = if rng.below(2) == 0 { format!(r"C:\Users\{u}\Desktop\notes.txt") } else { format!(" /home/{u}/.bashrc") };
            c(text, vec![u])
        }
        "codename" => c(format!("ship Codename {} by Friday", w.codename), vec![w.codename.clone()]),
        "entropy" => {
            let b = rng.key(40);
            c(format!("use {b} as the seed"), vec![b])
        }
        "base64" => {
            let b = format!("{}+{}/{}", rng.key(30), rng.key(20), rng.key(14));
            c(format!("blob: {b}=="), vec![b])
        }
        "azure" => {
            let b = rng.pick(B64, 86);
            c(format!("DefaultEndpointsProtocol=https;AccountName=x;AccountKey={b}==;"), vec![b])
        }
        "npm" => {
            let b = rng.key(36);
            c(format!("//registry.npmjs.org/:_authToken=npm_{b}"), vec![b])
        }
        "sendgrid" => {
            let (a, b) = (rng.pick(B64URL, 22), rng.pick(B64URL, 43));
            c(format!("SG.{a}.{b}"), vec![a, b])
        }
        "slack_hook" => {
            let b = rng.key(24);
            c(format!("https://hooks.slack.com/services/T{}/B{}/{b}", rng.pick(UPPER36, 8), rng.pick(UPPER36, 8)), vec![b])
        }
        // Planted as a `.env` read whose value has no secret shape at all.
        "secret_file" => {
            let v = rng.pick(LOWER, 18);
            c(format!("FEATURE_SEED={v}\nMODE=dev"), vec![v])
        }
        other => unreachable!("{other}"),
    }
}

/// Builds one exchange (user turn through final answer) carrying `cn` at placement `place`.
fn exchange(cn: &Canary, place: u64, ids: &mut u32, rng: &mut Rng) -> Vec<Message> {
    let mut id = || {
        *ids += 1;
        format!("toolu_{:08}", *ids)
    };
    let benign = "fn parse_config(path: &Path) -> Result<Config>";
    let sha = "6c0c6759d0617d7670826b61aca0a931bbb6db1f";
    if cn.kind == "secret_file" {
        let (a, b) = (id(), id());
        // grep/rg output from the secret file inside an ordinary search.
        let rg_lines: String = cn.text.lines().enumerate().map(|(n, l)| format!("web/.env.development.local:{}:{l}\n", n + 1)).collect();
        return match place % 6 {
            3 => vec![
                user("check the next.js env"),
                call("", "", vec![(a.clone(), "read", json!({"path": "web/.env.production.local"}))]),
                results(vec![(a, cn.text.clone(), false)]),
                say("The seed is set."),
            ],
            4 => vec![
                user("what does the prod env say"),
                call("", "", vec![(a.clone(), "bash", json!({"command": "cat prod.env"}))]),
                results(vec![(a, cn.text.clone(), false)]),
                say("Shown above."),
            ],
            5 => vec![
                user("where is the seed set"),
                call("", "", vec![(a.clone(), "grep", json!({"pattern": "SEED|fn main", "path": "."}))]),
                results(vec![(a, format!("{rg_lines}src/main.rs:3:fn main() {{}}"), false)]),
                say("In the development env."),
            ],
            0 => vec![
                user("check the feature seed"),
                call("", "", vec![(a.clone(), "read", json!({"path": ".env"}))]),
                results(vec![(a, cn.text.clone(), false)]),
                say("The seed is set."),
            ],
            1 => vec![
                user("what is in the local env"),
                call("", "", vec![(a.clone(), "bash", json!({"command": "cat config/.env.local"}))]),
                results(vec![(a, cn.text.clone(), false)]),
                say("Shown above."),
            ],
            _ => vec![
                user("write the production env"),
                call("", "", vec![(a.clone(), "write", json!({"path": "deploy/.env.production", "content": cn.text}))]),
                results(vec![(a, "wrote 2 lines".into(), false)]),
                call("", "", vec![(b.clone(), "bash", json!({"command": "cargo test --workspace"}))]),
                results(vec![(b, format!("test result: ok. {sha}"), false)]),
                say("Written."),
            ],
        };
    }
    let t = &cn.text;
    let wrap = |s: &str, rng: &mut Rng| -> String {
        match rng.below(6) {
            0 => format!("`{s}`"),
            1 => format!("\"{s}\""),
            2 => format!("({s})"),
            3 => format!("```\n{s}\n```"),
            4 => format!("{s}."),
            _ => s.to_string(),
        }
    };
    let w = wrap(t, rng);
    let (a, b) = (id(), id());
    match place % 8 {
        0 => vec![
            user(&format!("please look at {w} and fix the build")),
            call("", "", vec![(a.clone(), "bash", json!({"command": "cargo build"}))]),
            results(vec![(a, format!("Finished. {benign}"), false)]),
            say("Fixed."),
        ],
        1 => vec![user("summarise the config"), say(&format!("The config has {w}; also {benign}"))],
        2 => vec![
            user("go on"),
            call(&format!("I notice {w} in the output, careful."), "Checking.", vec![(a.clone(), "grep", json!({"pattern": "fn main", "path": "src"}))]),
            results(vec![(a, format!("src/main.rs:1:fn main() {{}} {sha}"), false)]),
            say("Found it."),
        ],
        3 => vec![
            user("run it"),
            call("", "", vec![(a.clone(), "bash", json!({"command": format!("echo {t} | tool --stdin"), "timeout": 30}))]),
            results(vec![(a, "ok".into(), false)]),
            say("Ran."),
        ],
        4 => vec![
            user("edit the file"),
            call(
                "",
                "",
                vec![(
                    a.clone(),
                    "edit",
                    json!({"path": "src/lib.rs", "edits": [{"old": "let v = 0;", "new": format!("let v = \"{t}\";")}], "nested": {"deep": [t]}}),
                )],
            ),
            results(vec![(a, "applied 1 edit".into(), false)]),
            say("Edited."),
        ],
        5 => vec![
            user("show the log"),
            call("", "", vec![(a.clone(), "read", json!({"path": "logs/run.log"}))]),
            results(vec![(a, format!("[info] start\n{w}\n[info] done {benign}"), false)]),
            say("Log read."),
        ],
        6 => vec![
            user("dump the settings"),
            call("", "", vec![(a.clone(), "bash", json!({"command": "marlowe settings --json"}))]),
            results(vec![(a, serde_json::to_string(&json!({"log": format!("[info] {t}"), "n": 3})).unwrap(), false)]),
            say("Dumped."),
        ],
        // A failed call then a corrected retry of the same tool: a DPO pair with the canary in it.
        _ => vec![
            user("run the tests"),
            call("", "", vec![(a.clone(), "bash", json!({"command": format!("pytest -k {t}")}))]),
            results(vec![(a, format!("error: bad selector {w}"), true)]),
            call("", "", vec![(b.clone(), "bash", json!({"command": "pytest -q tests"}))]),
            results(vec![(b, "3 passed".into(), false)]),
            say("Tests pass."),
        ],
    }
}

#[test]
fn a11_zero_leaks_of_1000_planted_canaries() {
    let home = TempHome::new("a11");
    let proj = home.dir("proj");
    let other = home.dir("other");
    // A fixed corpus; `MARLOWE_A11_SEED=<n>` plants a different one (to hunt for misses).
    let seed = std::env::var("MARLOWE_A11_SEED").ok().and_then(|s| s.parse().ok()).unwrap_or(0x00A1_1C4A_4A21);
    let mut rng = Rng(seed);
    let world = World::new(&mut rng);
    let cfg = ExportConfig { projects: vec![proj.clone()], redact_terms: vec![format!("Codename {}", world.codename)], allow_terms: vec![] };
    cfg.save(home.path()).unwrap();

    // 1,000 canaries, every kind in every placement, spread over 40 sessions and 5 crew runs.
    let canaries: Vec<Canary> = (0..1000).map(|i| canary(KINDS[i % KINDS.len()], &mut rng, &world)).collect();
    let c = db(home.path());
    let mut ids = 0u32;
    let mut sessions: Vec<Vec<Message>> = vec![Vec::new(); 40];
    let mut crew: Vec<Vec<Message>> = vec![Vec::new(); 10];
    for (i, cn) in canaries.iter().enumerate() {
        let place = (i / KINDS.len()) as u64 + i as u64;
        let ex = exchange(cn, place, &mut ids, &mut rng);
        if i % 9 == 4 {
            crew[i % 10].extend(ex);
        } else {
            sessions[i % 40].extend(ex);
        }
    }
    for (n, msgs) in sessions.iter().enumerate() {
        let id = format!("sess{n:04}");
        // Titles are never exported; one carries a canary to prove it.
        add_session(&c, &id, &format!("title {}", canaries[n].text), Some(&proj), false, msgs);
        good(&c, &id);
    }
    for k in 0..5 {
        add_run(
            home.path(),
            &format!(r"{proj}\sub"),
            &format!("goal {}", canaries[k].text),
            true,
            false,
            vec![("implement", crew[2 * k].clone()), ("test:1", crew[2 * k + 1].clone())],
        );
    }
    // Sources that must not be exported at all.
    let excluded: Vec<String> = (0..4).map(|_| format!("EXCLUDED{}", rng.key(20))).collect();
    let ex = |s: &str| vec![user(&format!("note {s}")), say("ok")];
    add_session(&c, "unmarked", "t", Some(&proj), false, &ex(&excluded[0]));
    add_session(&c, "otherproj", "t", Some(&other), false, &ex(&excluded[1]));
    good(&c, "otherproj");
    add_session(&c, "tainted", "t", Some(&proj), true, &ex(&excluded[2]));
    good(&c, "tainted");
    add_run(home.path(), &proj, "failed run", false, false, vec![("implement", ex(&excluded[3]))]);
    drop(c);

    let out_dir = home.path().join("out");
    let out_s = out_dir.to_string_lossy().to_string();
    let (r, report) = run(home.path(), &["trajectories", "--out", &out_s], &world.inputs());
    assert_eq!(r, Ok(0), "{report}");

    let sft = read(&out_dir.join(SFT_FILE));
    let dpo = read(&out_dir.join(DPO_FILE));
    let manifest = read(&out_dir.join(MANIFEST_FILE));
    let card = read(&out_dir.join("README.md"));
    let raw = format!("{sft}\n{dpo}\n{manifest}\n{card}").to_lowercase();
    let decoded = format!("{}\n{}", decoded_strings(&sft), decoded_strings(&dpo)).to_lowercase();

    // A11: no canary, and no 12-character piece of a long one, in any written byte.
    // One multi-pattern scan (the windows of every core) instead of 30,000 substring searches.
    let mut owner: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (i, cn) in canaries.iter().enumerate() {
        for core in &cn.cores {
            let core = core.to_lowercase();
            if core.len() >= 16 {
                for s in 0..=core.len() - 12 {
                    owner.entry(core[s..s + 12].to_string()).or_insert(i);
                }
            } else {
                owner.entry(core).or_insert(i);
            }
        }
    }
    let mut pats: Vec<&String> = owner.keys().collect();
    pats.sort_by_key(|p| std::cmp::Reverse(p.len()));
    let finder = regex::RegexBuilder::new(&pats.iter().map(|p| regex::escape(p)).collect::<Vec<_>>().join("|"))
        .size_limit(1 << 30)
        .dfa_size_limit(1 << 30)
        .build()
        .unwrap();
    let mut leaks = Vec::new();
    for text in [&raw, &decoded] {
        for m in finder.find_iter(text) {
            let i = owner[m.as_str()];
            leaks.push(format!("#{i} {}: {:?} (planted {:?})", canaries[i].kind, m.as_str(), canaries[i].text));
        }
    }
    // The scan itself must be able to see a leak.
    assert!(finder.is_match(&canaries[0].cores[0].to_lowercase()));
    assert!(leaks.is_empty(), "{} leak(s) of 1000 canaries:\n{}", leaks.len(), leaks.iter().take(25).cloned().collect::<Vec<_>>().join("\n"));
    for e in &excluded {
        assert!(!raw.contains(&e.to_lowercase()), "an excluded source was exported: {e}");
    }

    // Utility: the scrubber removed the canaries, not the data.
    let m: Value = serde_json::from_str(&manifest).unwrap();
    assert_eq!(m["records"]["dropped_by_gate"], 0, "gate dropped records: {}", m["records"]["dropped_kinds"]);
    assert_eq!(m["records"]["sft"].as_u64().unwrap(), 40 + 10, "{report}");
    assert!(m["records"]["dpo"].as_u64().unwrap() >= 100, "{}", m["records"]);
    let redactions: u64 = m["scrub"]["redactions"].as_object().unwrap().values().map(|v| v.as_u64().unwrap()).sum();
    assert!(redactions >= 1000, "{redactions}");
    for keep in ["fn parse_config(path: &path) -> result<config>", "6c0c6759d0617d7670826b61aca0a931bbb6db1f", "cargo test --workspace", "tests pass."] {
        assert!(decoded.contains(keep), "benign text lost: {keep}");
    }
    assert!(!raw.contains("opaque-signature"), "thinking signatures must not be exported");
}

// ------------------------------------------------------------------ behaviour

fn small_session() -> Vec<Message> {
    vec![
        user("rename the function"),
        call("think first", "", vec![("c1".into(), "edit", json!({"path": "src/a.rs", "old": "fn a", "new": "fn b"}))]),
        results(vec![("c1".into(), "error: old string not found".into(), true)]),
        call("", "", vec![("c2".into(), "edit", json!({"path": "src/a.rs", "old": "fn a()", "new": "fn b()"}))]),
        results(vec![("c2".into(), "applied".into(), false)]),
        call("", "", vec![("c3".into(), "bash", json!({"command": "rm -rf target"}))]),
        results(vec![("c3".into(), "The user denied this call. Do not retry it; continue without it or ask the user how to proceed.".into(), true)]),
        say("Renamed; I left target alone."),
        user("thanks"),
        call("", "", vec![("c4".into(), "bash", json!({"command": "cargo check"}))]),
    ]
}

#[test]
fn export_is_off_by_default() {
    let home = TempHome::new("off");
    let proj = home.dir("proj");
    let c = db(home.path());
    add_session(&c, "s1", "t", Some(&proj), false, &small_session());
    good(&c, "s1");
    drop(c);
    let (r, out) = run(home.path(), &["trajectories"], &ScrubInputs::default());
    assert_eq!(r, Ok(1));
    assert!(out.contains("export is off"), "{out}");
    assert!(!home.path().join("exports").exists());
    let (r, out) = run(home.path(), &["status"], &ScrubInputs::default());
    assert_eq!(r, Ok(0));
    assert!(out.contains("export is off") && out.contains("/good marks: 1"), "{out}");
}

#[test]
fn dry_run_reports_and_writes_nothing() {
    let home = TempHome::new("dry");
    let proj = home.dir("proj");
    enable(home.path(), &proj);
    let c = db(home.path());
    add_session(&c, "s1", "t", Some(&proj), false, &small_session());
    good(&c, "s1");
    add_session(&c, "s2", "t", Some(&proj), false, &small_session());
    add_session(&c, "s3", "t", None, false, &small_session());
    drop(c);
    let out_dir = home.path().join("dry-out");
    let (r, out) = run(home.path(), &["trajectories", "--dry-run", "--out", &out_dir.to_string_lossy()], &ScrubInputs::default());
    assert_eq!(r, Ok(0), "{out}");
    assert!(out.starts_with("dry run: nothing written"), "{out}");
    assert!(out.contains("3 session(s) seen, 1 kept"), "{out}");
    assert!(out.contains("no_project 1") && out.contains("not_kept 1"), "{out}");
    assert!(out.contains("sft 1, dpo 2 (denied 1, error_retry 1)"), "{out}");
    assert!(!out_dir.exists() && !home.path().join("exports").exists());
}

#[test]
fn records_are_hf_chat_rows_and_step_dpo_pairs() {
    let home = TempHome::new("hf");
    let proj = home.dir("proj");
    enable(home.path(), &proj);
    let c = db(home.path());
    add_session(&c, "s1", "t", Some(&format!(r"{proj}\crates\x")), false, &small_session());
    good(&c, "s1");
    drop(c);
    let out_dir = home.path().join("o");
    let (r, out) = run(home.path(), &["trajectories", "--out", &out_dir.to_string_lossy()], &ScrubInputs::default());
    assert_eq!(r, Ok(0), "{out}");

    let sft: Vec<Value> = read(&out_dir.join(SFT_FILE)).lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(sft.len(), 1);
    let rec = &sft[0];
    assert_eq!(rec["source"], "conductor");
    assert_eq!(rec["keep"], "good");
    assert_eq!(rec["project"], "proj");
    let msgs = rec["messages"].as_array().unwrap();
    // The dangling `cargo check` call after the last answer is trimmed.
    assert_eq!(msgs.last().unwrap()["content"], "Renamed; I left target alone.");
    assert_eq!(msgs[1]["reasoning_content"], "think first");
    let mut seen_calls = std::collections::HashSet::new();
    for m in msgs {
        match m["role"].as_str().unwrap() {
            "assistant" => {
                for c in m["tool_calls"].as_array().into_iter().flatten() {
                    assert_eq!(c["type"], "function");
                    let args: Value = serde_json::from_str(c["function"]["arguments"].as_str().unwrap()).unwrap();
                    assert!(args.is_object());
                    assert!(c["id"].as_str().unwrap().starts_with("call_"));
                    seen_calls.insert(c["id"].as_str().unwrap().to_string());
                }
            }
            "tool" => {
                assert!(seen_calls.contains(m["tool_call_id"].as_str().unwrap()), "result before its call");
                assert!(!m["name"].as_str().unwrap().is_empty());
            }
            "user" => {}
            other => panic!("role {other}"),
        }
    }

    let dpo: Vec<Value> = read(&out_dir.join(DPO_FILE)).lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(dpo.len(), 2);
    let retry = dpo.iter().find(|d| d["kind"] == "error_retry").unwrap();
    assert_eq!(retry["prompt"].as_array().unwrap().len(), 1);
    assert_eq!(retry["prompt"][0]["role"], "user");
    let args = |v: &Value| v[0]["tool_calls"][0]["function"]["arguments"].as_str().unwrap().to_string();
    assert!(args(&retry["rejected"]).contains("fn a\"") && args(&retry["chosen"]).contains("fn a()"));
    let denied = dpo.iter().find(|d| d["kind"] == "denied").unwrap();
    assert!(args(&denied["rejected"]).contains("rm -rf target"));
    assert_eq!(denied["chosen"][0]["content"], "Renamed; I left target alone.");

    // The manifest's hashes and counts are those of the files.
    let m: Value = serde_json::from_str(&read(&out_dir.join(MANIFEST_FILE))).unwrap();
    assert_eq!(m["format"], "marlowe-trajectories/1");
    assert_eq!(m["local_only"], true);
    for f in m["files"].as_array().unwrap() {
        let bytes = std::fs::read(out_dir.join(f["path"].as_str().unwrap())).unwrap();
        use sha2::Digest;
        let sha: String = sha2::Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(f["sha256"], sha.as_str());
        assert_eq!(f["bytes"], bytes.len() as u64);
        assert_eq!(f["records"].as_u64().unwrap() as usize, bytes.split(|b| *b == b'\n').filter(|l| !l.is_empty()).count());
    }
    // A second export into the same directory is refused.
    let (r, _) = run(home.path(), &["trajectories", "--out", &out_dir.to_string_lossy()], &ScrubInputs::default());
    assert!(r.is_err());
}

#[test]
fn keep_rule_tainted_and_crew_merge() {
    let home = TempHome::new("keep");
    let proj = home.dir("proj");
    enable(home.path(), &proj);
    let c = db(home.path());
    add_session(&c, "clean", "t", Some(&proj), false, &small_session());
    add_session(&c, "web", "t", Some(&proj), true, &small_session());
    good(&c, "clean");
    good(&c, "web");
    drop(c);
    add_run(home.path(), &proj, "merged", true, false, vec![("implement", small_session()), ("review", vec![user("review"), say("lgtm")])]);
    add_run(home.path(), &proj, "failed", false, false, vec![("implement", small_session())]);
    add_run(home.path(), &proj, "tainted", true, true, vec![("implement", small_session())]);

    let (r, out) = run(home.path(), &["trajectories", "--dry-run"], &ScrubInputs::default());
    assert_eq!(r, Ok(0));
    assert!(out.contains("2 session(s) seen, 1 kept; 3 crew run(s) seen, 1 kept"), "{out}");
    assert!(out.contains("tainted 2") && out.contains("not_kept 1"), "{out}");
    assert!(out.contains("sft 3,"), "{out}");
    let (_, out) = run(home.path(), &["trajectories", "--dry-run", "--include-tainted"], &ScrubInputs::default());
    assert!(out.contains("2 session(s) seen, 2 kept; 3 crew run(s) seen, 2 kept"), "{out}");
}

#[test]
fn good_marks_from_the_cli_and_upto() {
    let home = TempHome::new("good");
    let proj = home.dir("proj");
    enable(home.path(), &proj);
    let c = db(home.path());
    add_session(&c, "a1b2c3", "t", Some(&proj), false, &small_session());
    drop(c);
    let i = ScrubInputs::default();
    let (r, out) = run(home.path(), &["good", "a1b2", "--upto", "2"], &i);
    assert_eq!(r, Ok(0), "{out}");
    assert!(out.contains("kept a1b2c3 (2 message(s))"), "{out}");
    let marks = source::good_marks(&source::open(&marlowe_proto::paths::cowork_db(home.path())).unwrap()).unwrap();
    assert_eq!(marks["a1b2c3"].upto, Some(2));
    // Two messages end on a tool call: nothing complete to keep.
    let (_, out) = run(home.path(), &["trajectories", "--dry-run"], &i);
    assert!(out.contains("empty 1"), "{out}");
    let (r, _) = run(home.path(), &["good", "a1b2c3"], &i);
    assert_eq!(r, Ok(0));
    let (_, out) = run(home.path(), &["trajectories", "--dry-run"], &i);
    assert!(out.contains("1 kept"), "{out}");
    let (r, _) = run(home.path(), &["ungood", "a1b2c3"], &i);
    assert_eq!(r, Ok(0));
    let (_, out) = run(home.path(), &["trajectories", "--dry-run"], &i);
    assert!(out.contains("0 kept") && out.contains("not_kept 1"), "{out}");
    assert!(run(home.path(), &["good", "zzz"], &i).0.is_err());
}

#[test]
fn opt_in_is_per_project_and_reversible() {
    let home = TempHome::new("optin");
    let proj = home.dir("proj");
    let other = home.dir("other");
    enable(home.path(), &proj);
    let (r, out) = run(home.path(), &["enable", &proj], &ScrubInputs::default());
    assert_eq!(r, Ok(0));
    assert!(out.contains("already"), "{out}");
    let cfg = ExportConfig::load(home.path()).unwrap();
    assert_eq!(cfg.projects.len(), 1);
    assert!(cfg.project_of(&format!(r"{proj}\src")).is_some());
    assert!(cfg.project_of(&format!("{proj}x")).is_none());
    assert!(cfg.project_of(&other).is_none());
    // --project must name an opted-in project.
    let c = db(home.path());
    drop(c);
    let (r, _) = run(home.path(), &["trajectories", "--dry-run", "--project", &other], &ScrubInputs::default());
    assert!(r.is_err());
    let (r, out) = run(home.path(), &["disable", &proj], &ScrubInputs::default());
    assert_eq!(r, Ok(0), "{out}");
    assert!(ExportConfig::load(home.path()).unwrap().projects.is_empty());
}

#[test]
fn secret_file_calls_lose_results_and_contents() {
    let home = TempHome::new("sf");
    let proj = home.dir("proj");
    enable(home.path(), &proj);
    let c = db(home.path());
    let msgs = vec![
        user("set it up"),
        call("", "", vec![("r".into(), "read", json!({"path": "C:/x/.env"})), ("w".into(), "write", json!({"path": "id_ed25519", "content": "plainvalueabc"}))]),
        results(vec![("r".into(), "SEED=plainvalueabc".into(), false), ("w".into(), "ok".into(), false)]),
        call("", "", vec![("t".into(), "read", json!({"path": ".env.example"}))]),
        results(vec![("t".into(), "SEED=changeme".into(), false)]),
        say("Done."),
    ];
    add_session(&c, "s", "t", Some(&proj), false, &msgs);
    good(&c, "s");
    drop(c);
    let out_dir = home.path().join("o");
    let (r, _) = run(home.path(), &["trajectories", "--out", &out_dir.to_string_lossy()], &ScrubInputs::default());
    assert_eq!(r, Ok(0));
    let text = read(&out_dir.join(SFT_FILE));
    assert!(!text.contains("plainvalueabc"), "{text}");
    assert!(text.contains("[REDACTED:secret_file]"));
    assert!(text.contains(".env") && text.contains("id_ed25519"), "paths stay: {text}");
    assert!(text.contains("SEED=changeme"), "templates are not secret: {text}");
}

#[test]
fn sft_rows_carry_the_schema_of_every_tool_they_call() {
    let home = TempHome::new("tools");
    let proj = home.dir("proj");
    enable(home.path(), &proj);
    let c = db(home.path());
    let msgs = vec![
        user("look around"),
        call("", "", vec![("a".into(), "read", json!({"path": "src/a.rs"})), ("b".into(), "mcp__fs__list", json!({"dir": "."}))]),
        results(vec![("a".into(), "fn a() {}".into(), false), ("b".into(), "a.rs".into(), false)]),
        call("", "", vec![("c".into(), "bash", json!({"command": "cargo test"})), ("d".into(), "read", json!({"path": "src/b.rs"}))]),
        results(vec![("c".into(), "ok".into(), false), ("d".into(), "fn b() {}".into(), false)]),
        say("All good."),
    ];
    add_session(&c, "s", "t", Some(&proj), false, &msgs);
    good(&c, "s");
    drop(c);
    let out_dir = home.path().join("o");
    let (r, out) = run(home.path(), &["trajectories", "--out", &out_dir.to_string_lossy()], &ScrubInputs::default());
    assert_eq!(r, Ok(0), "{out}");
    assert!(out.contains("tools     1 call name(s) outside the registry"), "{out}");
    let rec: Value = serde_json::from_str(read(&out_dir.join(SFT_FILE)).lines().next().unwrap()).unwrap();
    let tools = rec["tools"].as_array().unwrap();
    let listed: Vec<&str> = tools.iter().map(|t| t["function"]["name"].as_str().unwrap()).collect();
    // Every called name is listed once, in first-call order.
    let mut called: Vec<&str> = Vec::new();
    for m in rec["messages"].as_array().unwrap() {
        for c in m["tool_calls"].as_array().into_iter().flatten() {
            let n = c["function"]["name"].as_str().unwrap();
            if !called.contains(&n) {
                called.push(n);
            }
        }
    }
    assert_eq!(listed, called);
    assert_eq!(listed, ["read", "mcp__fs__list", "bash"]);
    for t in tools {
        assert_eq!(t["type"], "function");
        let f = &t["function"];
        if f["name"] == "mcp__fs__list" {
            assert!(f.get("parameters").is_none(), "an unknown tool has no schema: {f}");
        } else {
            assert_eq!(f["parameters"]["type"], "object", "{f}");
            assert!(!f["description"].as_str().unwrap().is_empty());
        }
    }
    let read_spec = &marlowe_exec::specs::table().get("read").unwrap().spec;
    assert_eq!(tools[0]["function"]["parameters"], read_spec.input_schema);
    let m: Value = serde_json::from_str(&read(&out_dir.join(MANIFEST_FILE))).unwrap();
    assert_eq!(m["tools_source"], format!("registry@{}", env!("CARGO_PKG_VERSION")));
    assert_eq!(m["tools_unknown"], 1);
    assert!(read(&out_dir.join("README.md")).contains("tools=row[\"tools\"]"));
}

#[test]
fn dpo_rows_carry_tools_too() {
    let home = TempHome::new("dpotools");
    let proj = home.dir("proj");
    enable(home.path(), &proj);
    let c = db(home.path());
    add_session(&c, "s", "t", Some(&proj), false, &small_session());
    good(&c, "s");
    drop(c);
    let out_dir = home.path().join("o");
    let (r, out) = run(home.path(), &["trajectories", "--out", &out_dir.to_string_lossy()], &ScrubInputs::default());
    assert_eq!(r, Ok(0), "{out}");
    for line in read(&out_dir.join(DPO_FILE)).lines() {
        let row: Value = serde_json::from_str(line).unwrap();
        let names: Vec<&str> = row["tools"].as_array().unwrap().iter().map(|t| t["function"]["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["edit", "bash"]);
    }
}

#[test]
fn secret_file_variants_and_grep_lines_are_redacted() {
    let home = TempHome::new("sf2");
    let proj = home.dir("proj");
    enable(home.path(), &proj);
    let c = db(home.path());
    let msgs = vec![
        user("check the envs"),
        call("", "", vec![
            ("a".into(), "read", json!({"path": "web/.env.production.local"})),
            ("b".into(), "bash", json!({"command": "cat prod.env"})),
            ("c".into(), "read", json!({"path": ".env.local.bak"})),
            ("d".into(), "grep", json!({"pattern": "SEED", "path": "."})),
        ]),
        results(vec![
            ("a".into(), "SEED=alphavalueqq".into(), false),
            ("b".into(), "SEED=betavalueqq".into(), false),
            ("c".into(), "SEED=gammavalueqq".into(), false),
            ("d".into(), "web/.env.development.local:3:SEED=deltavalueqq\r\nC:\\w\\.env-4-OTHER=epsilonvalue\nsrc/seed.rs:1:const SEED: u32 = 7;".into(), false),
        ]),
        say("Done."),
    ];
    add_session(&c, "s", "t", Some(&proj), false, &msgs);
    good(&c, "s");
    drop(c);
    let out_dir = home.path().join("o");
    let (r, _) = run(home.path(), &["trajectories", "--out", &out_dir.to_string_lossy()], &ScrubInputs::default());
    assert_eq!(r, Ok(0));
    let text = decoded_strings(&read(&out_dir.join(SFT_FILE)));
    for v in ["alphavalueqq", "betavalueqq", "gammavalueqq", "deltavalueqq", "epsilonvalue"] {
        assert!(!text.contains(v), "{v} leaked: {text}");
    }
    assert!(text.contains("web/.env.development.local:3:[REDACTED:secret_file]"), "{text}");
    assert!(text.contains("src/seed.rs:1:const SEED: u32 = 7;"), "other lines stay: {text}");
}
