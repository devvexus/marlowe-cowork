//! Track F2 end to end: per-call turn rows, checkpoints and `/rewind`, `/diff`, `/cost`, rename,
//! fork, export, and the A7 kill matrix. A daemon runs on an ephemeral port with a temp home, a
//! scripted provider and tools that really write files; the kill tests run the daemon in a child
//! process that exits hard (no destructors, no flush) at a chosen point, then start a new daemon
//! on the same home.
//!
//! Test names beginning `parity_s` are the S-rows of the M4.6 parity matrix; `a7_` is the
//! durability acceptance.

use std::collections::{BTreeMap, VecDeque};
use std::io::BufReader;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use marlowe_agent::{Consequence, Mutation, ToolCallReq, ToolCtx, ToolHost, ToolOutcome, UserInfo};
use marlowe_brains::{Library, MemoryStore, ProviderFactory};
use marlowe_daemon::{Daemon, DaemonConfig, DaemonDeps, DaemonError};
use marlowe_mind::{Mind, MindConfig, RecallQuery};
use marlowe_model::{
    CancelToken, ChatRequest, Completion, CompletionBuilder, ModelInfo, Part, Pricing, Provider, ProviderError,
    ProviderKind, Role, StopReason, StreamEvent, ToolSpec, Usage,
};
use marlowe_proto::{
    paths, write_line, DaemonInfo, Envelope, Event, ExportFormat, LineReader, Request, RewindScope, SessionId, StoredTurn,
};

// ---------------------------------------------------------------- scripted provider

enum Step {
    Ev(StreamEvent),
    /// The process dies here, as a killed daemon does.
    Exit,
    /// Waits until released.
    Gate(crossbeam_channel::Receiver<()>),
}

#[derive(Default)]
struct Scripted {
    scripts: Mutex<VecDeque<Vec<Step>>>,
    requests: Mutex<Vec<ChatRequest>>,
}

impl Scripted {
    fn push(&self, steps: Vec<Step>) {
        self.scripts.lock().unwrap().push_back(steps);
    }

    fn requests(&self) -> Vec<ChatRequest> {
        self.requests.lock().unwrap().clone()
    }
}

const KILLED: i32 = 86;

fn text(s: &str) -> Step {
    Step::Ev(StreamEvent::TextDelta(s.into()))
}

fn usage(input: u64, output: u64) -> Step {
    Step::Ev(StreamEvent::Usage(Usage { input, output, ..Default::default() }))
}

/// One model call: text, then `Done`.
fn say(s: &str) -> Vec<Step> {
    vec![text(s), usage(1_000, 200), Step::Ev(StreamEvent::Stop(StopReason::EndTurn))]
}

/// One model call that asks for tools: `(id, name, args)` each.
fn calls(list: &[(&str, &str, &str)]) -> Vec<Step> {
    let mut steps = vec![usage(1_000, 200)];
    for (i, (id, name, args)) in (0u32..).zip(list.iter()) {
        steps.push(Step::Ev(StreamEvent::ToolCallStart { index: i, id: (*id).into(), name: (*name).into() }));
        steps.push(Step::Ev(StreamEvent::ToolArgsDelta { index: i, json: (*args).into() }));
        steps.push(Step::Ev(StreamEvent::ToolCallEnd { index: i }));
    }
    steps.push(Step::Ev(StreamEvent::Stop(StopReason::ToolUse)));
    steps
}

impl Provider for Scripted {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Ollama
    }

    fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        let mut a = ModelInfo::bare(ProviderKind::Ollama, "scripted");
        a.supports_tools = true;
        a.context_window = Some(32_768);
        Ok(vec![a])
    }

    fn stream(&self, req: &ChatRequest, _cancel: &CancelToken, on: &mut dyn FnMut(StreamEvent)) -> Result<Completion, ProviderError> {
        self.requests.lock().unwrap().push(req.clone());
        let script = self.scripts.lock().unwrap().pop_front().unwrap_or_else(|| say("ok"));
        let mut b = CompletionBuilder::new();
        for step in script {
            match step {
                Step::Ev(e) => {
                    b.push(&e);
                    on(e);
                }
                Step::Exit => std::process::exit(KILLED),
                Step::Gate(rx) => {
                    let _ = rx.recv_timeout(Duration::from_secs(30));
                }
            }
        }
        Ok(b.finish())
    }
}

// ---------------------------------------------------------------- tools that touch the disk

struct WorkTools;

fn path_arg(call: &ToolCallReq) -> Option<PathBuf> {
    call.args.get("path").and_then(|p| p.as_str()).map(PathBuf::from)
}

fn resolve(ctx: &ToolCtx, p: PathBuf) -> PathBuf {
    if p.is_absolute() {
        p
    } else {
        ctx.workspace.clone().unwrap_or_default().join(p)
    }
}

impl ToolHost for WorkTools {
    fn specs(&self) -> Vec<ToolSpec> {
        ["write_file", "delete_file", "shell_touch", "die_later", "look"]
            .iter()
            .map(|n| ToolSpec { name: n.to_string(), description: n.to_string(), input_schema: serde_json::json!({"type": "object"}) })
            .collect()
    }

    fn call(&self, call: &ToolCallReq, ctx: &ToolCtx) -> ToolOutcome {
        match call.name.as_str() {
            "write_file" => {
                let path = resolve(ctx, path_arg(call).unwrap());
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                let content = call.args.get("content").and_then(|c| c.as_str()).unwrap_or("");
                std::fs::write(&path, content).unwrap();
                ToolOutcome::ok(format!("wrote {}", path.file_name().unwrap().to_string_lossy()), "wrote")
            }
            "delete_file" => {
                let path = resolve(ctx, path_arg(call).unwrap());
                let _ = std::fs::remove_file(&path);
                ToolOutcome::ok("deleted", "deleted")
            }
            // Stands for a shell command: changes something under the workspace.
            "shell_touch" => {
                let ws = ctx.workspace.clone().unwrap();
                std::fs::create_dir_all(ws.join("gen")).unwrap();
                std::fs::write(ws.join("gen").join("made.txt"), "by a shell\n").unwrap();
                ToolOutcome::ok("exit 0", "exit 0")
            }
            // Waits for the other call of its batch to finish (its file appears, its outcome is
            // stored a moment later), then the process dies with this call still running.
            "die_later" => {
                let fast = ctx.workspace.clone().unwrap().join("fast.txt");
                let deadline = Instant::now() + Duration::from_secs(30);
                while !fast.exists() && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(20));
                }
                std::thread::sleep(Duration::from_millis(1_000));
                std::process::exit(KILLED)
            }
            "look" => ToolOutcome::ok("looked", "looked"),
            other => ToolOutcome::error(format!("unknown tool {other}")),
        }
    }

    fn consequence(&self, _call: &ToolCallReq) -> Consequence {
        Consequence::Free
    }

    fn mutation(&self, call: &ToolCallReq, ctx: &ToolCtx) -> Mutation {
        match call.name.as_str() {
            "write_file" | "delete_file" => Mutation::Paths(vec![resolve(ctx, path_arg(call).unwrap())]),
            "shell_touch" => Mutation::Under(ctx.workspace.clone().unwrap()),
            _ => Mutation::None,
        }
    }
}

// ---------------------------------------------------------------- harness

fn temp_dir(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!("marlowe-f2-{name}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn seed_model(home: &Path, pricing: Option<Pricing>) {
    let lib = Library::open(&paths::cowork_db(home)).unwrap();
    let mut info = ModelInfo::bare(ProviderKind::Ollama, "scripted");
    info.supports_tools = true;
    info.context_window = Some(32_768);
    info.pricing = pricing;
    lib.add(&info).unwrap();
}

fn open_mind(home: &Path) -> Mind {
    Mind::open_with(home, MindConfig { models_dir: Some(home.join("no-models-here")), ..MindConfig::default() }).expect("mind opens")
}

fn deps(provider: Arc<Scripted>, mind: Option<Mind>) -> DaemonDeps {
    let mut providers = ProviderFactory::new(Arc::new(MemoryStore::new()));
    providers.register(ProviderKind::Ollama, move |_| Ok(provider.clone() as Arc<dyn Provider>));
    DaemonDeps {
        providers,
        tools: Arc::new(|_| Arc::new(WorkTools) as Arc<dyn ToolHost>),
        user: UserInfo { name: Some("Tester".into()), address: None },
        mind,
        sentinel: None,
        voice: None,
        crew: None,
    }
}

struct Running {
    info: DaemonInfo,
    join: JoinHandle<Result<(), DaemonError>>,
}

fn start(home: &Path, provider: Arc<Scripted>, mind: Option<Mind>) -> Running {
    let d = Daemon::start(deps(provider, mind), home, DaemonConfig::default()).expect("daemon starts");
    let info = d.info();
    Running { info, join: std::thread::spawn(move || d.wait()) }
}

fn stop(r: Running) {
    let mut c = Client::connect(&r.info);
    c.send(&Request::Shutdown);
    c.until(|e| matches!(e, Event::ShuttingDown));
    r.join.join().unwrap().unwrap();
}

const WAIT: Duration = Duration::from_secs(60);

struct Client {
    w: TcpStream,
    r: LineReader<BufReader<TcpStream>>,
}

impl Client {
    fn connect(info: &DaemonInfo) -> Client {
        let w = TcpStream::connect(info.addr()).unwrap();
        w.set_read_timeout(Some(WAIT)).unwrap();
        let mut c = Client { r: LineReader::new(BufReader::new(w.try_clone().unwrap())), w };
        c.send(&Request::Hello { token: info.token.clone(), client: "test".into(), since_seq: None });
        let env = c.next();
        assert!(matches!(env.event, Event::Welcome { .. }), "{env:?}");
        c
    }

    fn send(&mut self, r: &Request) {
        write_line(&mut self.w, r).unwrap();
    }

    fn next(&mut self) -> Envelope {
        self.r.next::<Envelope>().expect("read an envelope").expect("the stream is open")
    }

    /// Every envelope up to and including the first whose event matches.
    fn until(&mut self, pred: impl Fn(&Event) -> bool) -> Vec<Envelope> {
        let deadline = Instant::now() + WAIT;
        let mut out = Vec::new();
        while Instant::now() < deadline {
            let env = self.next();
            let hit = pred(&env.event);
            out.push(env);
            if hit {
                return out;
            }
        }
        panic!("timed out; saw {out:#?}");
    }

    /// Sends `r` and returns the first event matching `pred`, or panics on an `Error` event.
    fn ask_for(&mut self, r: &Request, pred: impl Fn(&Event) -> bool) -> Event {
        self.send(r);
        let all = self.until(|e| pred(e) || matches!(e, Event::Error { .. }));
        match all.last().unwrap().event.clone() {
            Event::Error { message, .. } => panic!("the daemon answered with an error: {message}"),
            e => e,
        }
    }

    /// Sends `r` and returns the error message the daemon answers with.
    fn refused(&mut self, r: &Request) -> String {
        self.send(r);
        match self.until(|e| matches!(e, Event::Error { .. })).pop().unwrap().event {
            Event::Error { message, .. } => message,
            _ => unreachable!(),
        }
    }

    /// Asks and waits for the turn to finish; returns the session and every event of the turn.
    fn turn(&mut self, session: Option<&SessionId>, prompt: &str, workspace: Option<&Path>) -> (SessionId, Vec<Envelope>) {
        self.send(&Request::Ask {
            session: session.cloned(),
            text: prompt.into(),
            workspace: workspace.map(|w| w.display().to_string()),
            images: Vec::new(),
            options: None,
        });
        let envs = self.until(|e| matches!(e, Event::TurnFinished { .. }));
        let id = envs
            .iter()
            .find_map(|e| match &e.event {
                Event::TurnStarted { session, .. } => Some(session.clone()),
                _ => None,
            })
            .expect("a TurnStarted");
        (id, envs)
    }

    fn transcript(&mut self, session: &SessionId) -> Vec<StoredTurn> {
        match self.ask_for(&Request::OpenSession { session: session.clone() }, |e| matches!(e, Event::Transcript { .. })) {
            Event::Transcript { turns, .. } => turns,
            _ => unreachable!(),
        }
    }

    fn checkpoints(&mut self, session: &SessionId) -> Vec<marlowe_proto::CheckpointInfo> {
        match self.ask_for(&Request::ListCheckpoints { session: session.clone() }, |e| matches!(e, Event::Checkpoints { .. })) {
            Event::Checkpoints { list, .. } => list,
            _ => unreachable!(),
        }
    }

    fn rewind(&mut self, session: &SessionId, turn: u32, scope: RewindScope) {
        self.ask_for(&Request::Rewind { session: session.clone(), turn, scope }, |e| matches!(e, Event::Rewound { .. }));
    }
}

fn user_texts(t: &[StoredTurn]) -> Vec<String> {
    t.iter()
        .filter(|t| t.role == Role::User)
        .map(|t| t.content.iter().filter_map(|p| if let Part::Text { text } = p { Some(text.as_str()) } else { None }).collect())
        .collect()
}

fn put(root: &Path, rel: &str, bytes: &[u8]) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, bytes).unwrap();
}

/// Every file under `root` except `.git`, by relative path.
fn files(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.file_name().is_some_and(|n| n == ".git") {
                continue;
            }
            if p.is_dir() {
                walk(root, &p, out);
            } else {
                out.insert(p.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/"), std::fs::read(&p).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A workspace that is the user's own git repository: tracked files, an untracked one, an ignored
/// build directory.
fn project(base: &Path) -> PathBuf {
    let ws = base.join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    git(&ws, &["init", "-q"]);
    put(&ws, ".gitignore", b"out/\n");
    put(&ws, "src/main.rs", b"fn main() {}\r\n");
    put(&ws, "notes.txt", b"first\n");
    git(&ws, &["add", "-A"]);
    git(&ws, &["commit", "-q", "-m", "init"]);
    put(&ws, "scratch/todo.txt", b"untracked\n");
    put(&ws, "out/build.log", b"build v0\n");
    ws
}

/// What the user's repository holds: object count and size, refs, the index file's bytes.
fn user_repo_census(ws: &Path) -> (String, String, Vec<u8>) {
    (git(ws, &["count-objects", "-v"]), git(ws, &["for-each-ref"]), std::fs::read(ws.join(".git").join("index")).unwrap())
}

fn write(id: &str, path: &str, content: &str) -> (String, String, String) {
    (id.to_string(), "write_file".to_string(), format!("{{\"path\":\"{path}\",\"content\":\"{content}\"}}"))
}

fn tool_script(list: &[(String, String, String)]) -> Vec<Step> {
    let borrowed: Vec<(&str, &str, &str)> = list.iter().map(|(a, b, c)| (a.as_str(), b.as_str(), c.as_str())).collect();
    calls(&borrowed)
}

// ---------------------------------------------------------------- S5: per-call rows

#[test]
fn parity_s5_the_transcript_is_written_per_model_call_and_per_tool_batch() {
    let home = temp_dir("s5");
    let ws = temp_dir("s5-ws");
    seed_model(&home, None);
    let p = Arc::new(Scripted::default());
    p.push(tool_script(&[write("c1", "a.txt", "one"), ("c2".into(), "look".into(), "{}".into())]));
    p.push(say("both done"));
    let d = start(&home, p.clone(), None);
    let mut c = Client::connect(&d.info);
    let (id, _) = c.turn(None, "make a.txt and look around", Some(&ws));

    // Read the rows the way a crashed daemon's successor would: straight from the database.
    let conn = rusqlite::Connection::open(paths::cowork_db(&home)).unwrap();
    type Row = (String, Option<u32>, Option<String>, Option<String>, Option<String>, Option<i64>, u32);
    let mut st = conn
        .prepare("SELECT role, step, provider, model, usage, latency_ms, turn FROM turns WHERE session = ?1 ORDER BY idx")
        .unwrap();
    let rows: Vec<Row> = st
        .query_map([id.as_str()], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(rows.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(), ["user", "assistant", "tool", "assistant"]);
    assert!(rows.iter().all(|r| r.6 == 0), "all four rows belong to turn 0");
    // Assistant rows: one per model call, each with provider, model, usage and latency.
    for (row, step) in [&rows[1], &rows[3]].into_iter().zip([1u32, 2]) {
        assert_eq!((row.1, row.2.as_deref(), row.3.as_deref()), (Some(step), Some("ollama"), Some("scripted")));
        let u: Usage = serde_json::from_str(row.4.as_deref().unwrap()).unwrap();
        assert_eq!((u.input, u.output), (1_000, 200));
        assert!(row.5.is_some());
    }
    // The tool batch is one row carrying both results.
    let tool_content: String = conn
        .query_row("SELECT content FROM turns WHERE session = ?1 AND role = 'tool'", [id.as_str()], |r| r.get(0))
        .unwrap();
    assert!(tool_content.contains("wrote a.txt") && tool_content.contains("looked"), "{tool_content}");
    // A turn is not written twice: the transcript a client sees has exactly those four.
    assert_eq!(c.transcript(&id).len(), 4);
    stop(d);
}

// ---------------------------------------------------------------- S4: cost

#[test]
fn parity_s4_cost_is_usage_times_the_price_table() {
    let home = temp_dir("s4");
    let ws = temp_dir("s4-ws");
    let price = Pricing { input_per_mtok: 2.0, output_per_mtok: 10.0 };
    seed_model(&home, Some(price));
    let p = Arc::new(Scripted::default());
    p.push(tool_script(&[write("c1", "a.txt", "x")]));
    p.push(say("done"));
    p.push(say("second turn"));
    let d = start(&home, p, None);
    let mut c = Client::connect(&d.info);
    let (id, _) = c.turn(None, "first", Some(&ws));
    c.turn(Some(&id), "second", None);
    match c.ask_for(&Request::SessionCost { session: id.clone() }, |e| matches!(e, Event::SessionCost { .. })) {
        Event::SessionCost { report, .. } => {
            assert_eq!(report.calls, 3);
            assert_eq!((report.usage.input, report.usage.output), (3_000, 600));
            assert_eq!(report.by_model.len(), 1);
            assert_eq!(report.by_model[0].model.to_string(), "ollama/scripted");
            let want = price.cost(3_000, 600);
            assert!((report.usage.cost_usd.unwrap() - want).abs() < 1e-12, "{:?} vs {want}", report.usage.cost_usd);
        }
        _ => unreachable!(),
    }
    // A session that never spoke costs nothing.
    let empty = match c.ask_for(&Request::NewSession { workspace: None }, |e| matches!(e, Event::SessionOpened { .. })) {
        Event::SessionOpened { session, .. } => session,
        _ => unreachable!(),
    };
    match c.ask_for(&Request::SessionCost { session: empty }, |e| matches!(e, Event::SessionCost { .. })) {
        Event::SessionCost { report, .. } => assert_eq!((report.calls, report.usage.cost_usd), (0, None)),
        _ => unreachable!(),
    }
    stop(d);
}

// ---------------------------------------------------------------- S2, S3: checkpoints, rewind, diff

/// Three turns over a user's repository; returns the file states after each turn (and before).
struct Three {
    home: PathBuf,
    ws: PathBuf,
    id: SessionId,
    states: Vec<BTreeMap<String, Vec<u8>>>,
}

fn three_turns(d: &Running, home: &Path, ws: &Path, p: &Scripted, c: &mut Client) -> Three {
    let mut states = vec![files(ws)];
    // Turn 0: edit a tracked file (CRLF kept), create one, write the ignored build log.
    p.push(tool_script(&[
        write("t0a", "notes.txt", "first\\nsecond\\n"),
        write("t0b", "src/new.rs", "pub fn new() {}\\n"),
        write("t0c", "out/build.log", "build v1\\n"),
    ]));
    p.push(say("turn 0 done"));
    let (id, _) = c.turn(None, "add a new module", Some(ws));
    states.push(files(ws));
    // Turn 1: a shell-like command under the workspace, and one more edit.
    p.push(tool_script(&[("t1a".into(), "shell_touch".into(), "{}".into()), write("t1b", "src/main.rs", "fn main() { new(); }\\n")]));
    p.push(say("turn 1 done"));
    c.turn(Some(&id), "wire it up", None);
    states.push(files(ws));
    // Turn 2: delete a file, rewrite the log, and a read-only turn after it.
    p.push(tool_script(&[
        ("t2a".into(), "delete_file".into(), "{\"path\":\"src/new.rs\"}".into()),
        write("t2b", "out/build.log", "build v2\\n"),
    ]));
    p.push(say("turn 2 done"));
    c.turn(Some(&id), "drop the module again", None);
    states.push(files(ws));
    let _ = d;
    Three { home: home.to_path_buf(), ws: ws.to_path_buf(), id, states }
}

#[test]
fn parity_s2_rewind_restores_code_conversation_or_both() {
    let base = temp_dir("s2");
    let home = base.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let ws = project(&base);
    seed_model(&home, None);
    let census = user_repo_census(&ws);
    let p = Arc::new(Scripted::default());
    let d = start(&home, p.clone(), None);
    let mut c = Client::connect(&d.info);
    let t = three_turns(&d, &home, &ws, &p, &mut c);
    assert_eq!(t.states.len(), 4);
    assert_ne!(t.states[0], t.states[1]);

    // One checkpoint per turn that changed files, with what each asked and how many files moved.
    let cps = c.checkpoints(&t.id);
    assert_eq!(cps.iter().map(|c| c.turn).collect::<Vec<_>>(), vec![0, 1, 2]);
    assert_eq!(cps[0].summary, "add a new module");
    assert_eq!(cps[0].files, 2, "the ignored log is not in the tree: notes.txt and src/new.rs");
    assert!(cps[1].files >= 2 && cps[2].files == 1, "{cps:?}");

    // Code only: the conversation keeps all three turns.
    c.rewind(&t.id, 2, RewindScope::Code);
    assert_eq!(files(&t.ws), t.states[2], "back to the start of turn 2, ignored log included");
    assert_eq!(user_texts(&c.transcript(&t.id)).len(), 3);

    // Conversation only: turns from 1 on are gone; the files stay where they are.
    c.rewind(&t.id, 1, RewindScope::Conversation);
    assert_eq!(files(&t.ws), t.states[2]);
    assert_eq!(user_texts(&c.transcript(&t.id)), vec!["add a new module".to_string()]);
    // The session carries on from turn 1: the model sees one turn of history and the new prompt.
    p.push(say("fresh start"));
    let before = p.requests().len();
    let (_, envs) = c.turn(Some(&t.id), "do it differently", None);
    assert!(envs.iter().any(|e| matches!(&e.event, Event::TurnStarted { turn: 1, .. })), "numbering resumes at the rewound turn");
    let req = &p.requests()[before];
    let said: Vec<String> = req.messages.iter().filter(|m| m.role == Role::User).map(|m| m.joined_text()).collect();
    assert_eq!(said.iter().filter(|s| s.contains("do it differently")).count(), 1);
    assert!(said.iter().any(|s| s.contains("add a new module")) && !said.iter().any(|s| s.contains("wire it up")));

    // Both: the files and the conversation go back to before turn 0.
    c.rewind(&t.id, 0, RewindScope::Both);
    assert_eq!(files(&t.ws), t.states[0], "byte for byte, ignored file and CRLF included");
    assert!(c.transcript(&t.id).is_empty());

    // A rewind to a turn that has not begun is refused; a busy session refuses too (below).
    assert!(c.refused(&Request::Rewind { session: t.id.clone(), turn: 5, scope: RewindScope::Both }).contains("has not begun"));
    // The user's own repository was never touched, by any of it.
    assert_eq!(user_repo_census(&t.ws), census);
    stop(d);
}

#[test]
fn parity_s3_the_diff_is_the_shadow_trees_diff() {
    let base = temp_dir("s3");
    let home = base.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let ws = project(&base);
    seed_model(&home, None);
    let p = Arc::new(Scripted::default());
    let d = start(&home, p.clone(), None);
    let mut c = Client::connect(&d.info);
    let t = three_turns(&d, &home, &ws, &p, &mut c);

    let diff_of = |c: &mut Client, turn: Option<u32>| match c
        .ask_for(&Request::SessionDiff { session: t.id.clone(), turn }, |e| matches!(e, Event::SessionDiff { .. }))
    {
        Event::SessionDiff { diff, .. } => diff,
        _ => unreachable!(),
    };

    // Turn 0 changed notes.txt and added src/new.rs.
    let d0 = diff_of(&mut c, Some(0));
    let lines: Vec<&str> = d0.lines().filter(|l| !l.starts_with("index ")).collect();
    assert_eq!(
        lines,
        vec![
            "diff --git a/notes.txt b/notes.txt",
            "--- a/notes.txt",
            "+++ b/notes.txt",
            "@@ -1 +1,2 @@",
            " first",
            "+second",
            "diff --git a/src/new.rs b/src/new.rs",
            "new file mode 100644",
            "--- /dev/null",
            "+++ b/src/new.rs",
            "@@ -0,0 +1 @@",
            "+pub fn new() {}",
        ]
    );
    // It is exactly the diff between the shadow repository's own trees.
    let store = marlowe_agent::checkpoint::CheckpointStore::new(t.home.join("checkpoints"));
    let repo = store.existing(&t.ws).unwrap().expect("checkpoints exist");
    let cps = repo.checkpoints(&t.id).unwrap();
    assert_eq!(d0, repo.diff(&cps[0].tree, &cps[1].tree).unwrap());
    // The last turn's diff runs to the present; the whole session runs from the first checkpoint.
    let d2 = diff_of(&mut c, Some(2));
    assert_eq!(d2, repo.diff(&cps[2].tree, &repo.current_tree().unwrap()).unwrap());
    assert!(d2.contains("deleted file mode") && d2.contains("src/new.rs"));
    let whole = diff_of(&mut c, None);
    assert_eq!(whole, repo.diff(&cps[0].tree, &repo.current_tree().unwrap()).unwrap());
    assert!(whole.contains("gen/made.txt") && whole.contains("src/main.rs"));
    assert!(!whole.contains("build.log"), "ignored files are not in the tree diff");
    // Edits made after the last turn ended, by hand, are not the turn's: not in its diff, not in
    // the whole session's, not in its file count.
    let listed_before = c.checkpoints(&t.id).iter().map(|c| c.files).collect::<Vec<_>>();
    std::fs::write(t.ws.join("notes.txt"), b"first\nsecond\nedited by hand afterwards\n").unwrap();
    std::fs::write(t.ws.join("by-hand.txt"), b"mine\n").unwrap();
    for diff in [diff_of(&mut c, Some(2)), diff_of(&mut c, None)] {
        assert!(!diff.contains("by-hand.txt") && !diff.contains("edited by hand afterwards"), "{diff}");
    }
    assert_eq!(diff_of(&mut c, Some(2)), d2);
    assert_eq!(diff_of(&mut c, None), whole);
    assert_eq!(c.checkpoints(&t.id).iter().map(|c| c.files).collect::<Vec<_>>(), listed_before);
    // A turn that changed nothing, and a session that changed nothing, have empty diffs.
    assert!(diff_of(&mut c, Some(9)).is_empty());
    let quiet = match c.ask_for(&Request::NewSession { workspace: None }, |e| matches!(e, Event::SessionOpened { .. })) {
        Event::SessionOpened { session, .. } => session,
        _ => unreachable!(),
    };
    match c.ask_for(&Request::SessionDiff { session: quiet, turn: None }, |e| matches!(e, Event::SessionDiff { .. })) {
        Event::SessionDiff { diff, .. } => assert!(diff.is_empty()),
        _ => unreachable!(),
    }
    stop(d);
}

#[test]
fn a_running_turn_refuses_a_rewind() {
    let home = temp_dir("busy");
    let ws = temp_dir("busy-ws");
    seed_model(&home, None);
    let p = Arc::new(Scripted::default());
    let d = start(&home, p.clone(), None);
    let mut c = Client::connect(&d.info);
    let (id, _) = c.turn(None, "first", Some(&ws));
    // The second turn's model call waits at a gate, so the session is busy.
    let (gate_tx, gate_rx) = crossbeam_channel::bounded::<()>(1);
    p.push(vec![Step::Gate(gate_rx), text("late answer"), Step::Ev(StreamEvent::Stop(StopReason::EndTurn))]);
    c.send(&Request::Ask { session: Some(id.clone()), text: "second".into(), workspace: None, images: Vec::new(), options: None });
    c.until(|e| matches!(e, Event::TurnStarted { .. }));
    let msg = c.refused(&Request::Rewind { session: id.clone(), turn: 0, scope: RewindScope::Conversation });
    assert!(msg.contains("a turn is running"), "{msg}");
    gate_tx.send(()).unwrap();
    c.until(|e| matches!(e, Event::TurnFinished { .. }));
    // Idle again: the same rewind goes through.
    c.rewind(&id, 1, RewindScope::Conversation);
    assert_eq!(user_texts(&c.transcript(&id)), vec!["first".to_string()]);
    stop(d);
}

// ---------------------------------------------------------------- S6, S7: rename, fork, export

#[test]
fn parity_s6_rename_and_a_fork_that_is_independent() {
    let base = temp_dir("s6");
    let home = base.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let ws = project(&base);
    seed_model(&home, None);
    let p = Arc::new(Scripted::default());
    let d = start(&home, p.clone(), None);
    let mut c = Client::connect(&d.info);
    p.push(tool_script(&[write("a", "notes.txt", "turn zero\\n")]));
    p.push(say("zero"));
    let (id, _) = c.turn(None, "turn zero prompt", Some(&ws));
    p.push(tool_script(&[write("b", "notes.txt", "turn one\\n")]));
    p.push(say("one"));
    c.turn(Some(&id), "turn one prompt", None);

    // Rename: broadcast, persisted, and a live session keeps the name.
    match c.ask_for(&Request::RenameSession { session: id.clone(), title: "  parser fix ".into() }, |e| {
        matches!(e, Event::SessionOpened { .. })
    }) {
        Event::SessionOpened { title, .. } => assert_eq!(title, "parser fix"),
        _ => unreachable!(),
    }
    match c.ask_for(&Request::ListSessions { limit: 10 }, |e| matches!(e, Event::SessionList { .. })) {
        Event::SessionList { sessions } => assert_eq!(sessions.iter().find(|s| s.id == id).unwrap().title, "parser fix"),
        _ => unreachable!(),
    }
    assert!(c.refused(&Request::RenameSession { session: id.clone(), title: "   ".into() }).contains("needs a name"));

    // Fork before turn 1: a new session holding turn 0 only.
    let fork = match c.ask_for(&Request::ForkSession { session: id.clone(), at_turn: Some(1) }, |e| matches!(e, Event::SessionOpened { .. })) {
        Event::SessionOpened { session, title, workspace } => {
            assert_eq!(title, "parser fix (fork)");
            assert_eq!(workspace.as_deref(), Some(ws.display().to_string().as_str()));
            session
        }
        _ => unreachable!(),
    };
    assert_ne!(fork, id);
    assert_eq!(user_texts(&c.transcript(&fork)), vec!["turn zero prompt".to_string()]);
    assert_eq!(c.checkpoints(&fork).iter().map(|c| c.turn).collect::<Vec<_>>(), vec![0], "the fork has turn 0's checkpoint");

    // Continue the fork in a different direction: the original does not move.
    p.push(say("a different turn one"));
    let before = p.requests().len();
    c.turn(Some(&fork), "forked prompt", None);
    let req = &p.requests()[before];
    let said: Vec<String> = req.messages.iter().filter(|m| m.role == Role::User).map(|m| m.joined_text()).collect();
    assert!(said.iter().any(|s| s.contains("turn zero prompt")) && said.iter().any(|s| s.contains("forked prompt")));
    assert!(!said.iter().any(|s| s.contains("turn one prompt")));
    assert_eq!(user_texts(&c.transcript(&id)), vec!["turn zero prompt".to_string(), "turn one prompt".to_string()]);
    assert_eq!(user_texts(&c.transcript(&fork)), vec!["turn zero prompt".to_string(), "forked prompt".to_string()]);
    // Rewinding the fork's code never touches the original session's checkpoints.
    assert_eq!(c.checkpoints(&id).iter().map(|c| c.turn).collect::<Vec<_>>(), vec![0, 1]);

    // A whole-session fork, and a cut past the end is refused.
    match c.ask_for(&Request::ForkSession { session: id.clone(), at_turn: None }, |e| matches!(e, Event::SessionOpened { .. })) {
        Event::SessionOpened { session, .. } => assert_eq!(c.transcript(&session).len(), c.transcript(&id).len()),
        _ => unreachable!(),
    }
    assert!(c.refused(&Request::ForkSession { session: id.clone(), at_turn: Some(9) }).contains("has not begun"));
    stop(d);
}

#[test]
fn parity_s7_export_is_markdown_or_json_of_the_session() {
    let home = temp_dir("s7");
    let ws = temp_dir("s7-ws");
    seed_model(&home, None);
    let p = Arc::new(Scripted::default());
    p.push(tool_script(&[write("c1", "a.txt", "one")]));
    p.push(say("wrote it"));
    let d = start(&home, p, None);
    let mut c = Client::connect(&d.info);
    let (id, _) = c.turn(None, "write a.txt please", Some(&ws));
    let md = match c.ask_for(&Request::ExportSession { session: id.clone(), format: ExportFormat::Markdown }, |e| {
        matches!(e, Event::SessionExported { .. })
    }) {
        Event::SessionExported { text, format, .. } => {
            assert_eq!(format, ExportFormat::Markdown);
            text
        }
        _ => unreachable!(),
    };
    assert!(md.starts_with("# write a.txt please\n"), "{md}");
    for want in ["**You**", "write a.txt please", "**Marlowe** (ollama/scripted", "`write_file`", "wrote a.txt", "wrote it", "2 model calls"] {
        assert!(md.contains(want), "missing {want:?} in\n{md}");
    }
    let json = match c.ask_for(&Request::ExportSession { session: id.clone(), format: ExportFormat::Json }, |e| {
        matches!(e, Event::SessionExported { .. })
    }) {
        Event::SessionExported { text, .. } => text,
        _ => unreachable!(),
    };
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["format"], "marlowe-session/1");
    assert_eq!(v["session"]["id"], id.as_str());
    assert_eq!(v["messages"].as_array().unwrap().len(), c.transcript(&id).len());
    assert_eq!(v["cost"]["calls"], 2);
    assert!(c.refused(&Request::ExportSession { session: SessionId("nope".into()), format: ExportFormat::Json }).contains("unknown session"));
    stop(d);
}

// ---------------------------------------------------------------- A7: the kill matrix

/// What a killed child leaves behind, and how it was killed.
#[derive(Clone, Copy, Debug)]
enum Kill {
    /// Hard stop in the middle of the second turn's first model call (some text streamed).
    MidCall,
    /// Hard stop at the start of the model call that follows a finished tool batch.
    AfterTools,
    /// Hard stop while one call of a parallel batch is still running and another has finished.
    MidBatch,
}

impl Kill {
    fn name(self) -> &'static str {
        match self {
            Kill::MidCall => "mid_call",
            Kill::AfterTools => "after_tools",
            Kill::MidBatch => "mid_batch",
        }
    }
}

/// The child process: runs a daemon, plays two turns (the second is killed), never returns.
#[test]
fn child_entry() {
    let Ok(scenario) = std::env::var("F2_CHILD") else { return };
    let home = PathBuf::from(std::env::var("F2_HOME").unwrap());
    let ws = PathBuf::from(std::env::var("F2_WS").unwrap());
    let mind = open_mind(&home);
    let p = Arc::new(Scripted::default());
    // Turn 0 completes: one write, then an answer.
    p.push(tool_script(&[write("t0", "app.conf", "v1")]));
    p.push(say("configured"));
    // Turn 1 is the one that dies.
    match scenario.as_str() {
        "mid_call" => p.push(vec![text("partial answer"), Step::Exit]),
        "after_tools" => {
            p.push(tool_script(&[write("t1a", "app.conf", "v2"), write("t1b", "extra/new.txt", "made")]));
            p.push(vec![Step::Exit]);
        }
        "mid_batch" => {
            p.push(tool_script(&[write("t1a", "fast.txt", "done quickly"), ("t1b".into(), "die_later".into(), "{}".into())]));
        }
        other => panic!("unknown scenario {other}"),
    }
    let d = start(&home, p, Some(mind));
    let mut c = Client::connect(&d.info);
    let (id, _) = c.turn(None, "My sister lives in Porto. Write the config.", Some(&ws));
    // The id is how the parent finds the session again.
    std::fs::write(home.join("child-session.txt"), id.as_str()).unwrap();
    c.send(&Request::Ask {
        session: Some(id),
        text: "My favourite colour is teal. Change the config.".into(),
        workspace: None,
        images: Vec::new(),
        options: None,
    });
    // The daemon dies before this wait ends.
    let _ = c.until(|e| matches!(e, Event::ShuttingDown));
}

fn kill_child(kill: Kill, home: &Path, ws: &Path) -> SessionId {
    let exe = std::env::current_exe().unwrap();
    let status = Command::new(exe)
        .args(["--exact", "child_entry", "--nocapture", "--test-threads=1"])
        .env("F2_CHILD", kill.name())
        .env("F2_HOME", home)
        .env("F2_WS", ws)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(KILLED), "{kill:?}: the child died where the scenario put the kill, not elsewhere");
    SessionId(std::fs::read_to_string(home.join("child-session.txt")).unwrap())
}

/// The state a killed child leaves for each kill point.
struct Killed {
    base: PathBuf,
    home: PathBuf,
    ws: PathBuf,
    id: SessionId,
}

fn killed(kill: Kill) -> Killed {
    let base = temp_dir(&format!("a7-{}", kill.name()));
    let home = base.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let ws = project(&base);
    seed_model(&home, None);
    let id = kill_child(kill, &home, &ws);
    Killed { base, home, ws, id }
}

fn tool_result_texts(t: &[StoredTurn]) -> Vec<(String, bool)> {
    t.iter()
        .filter(|t| t.role == Role::Tool)
        .flat_map(|t| t.content.iter())
        .filter_map(|p| if let Part::ToolResult(r) = p { Some((r.content.clone(), r.is_error)) } else { None })
        .collect()
}

#[test]
fn a7_killed_mid_model_call_resumes_with_the_prompt_and_loses_only_that_call() {
    let k = killed(Kill::MidCall);
    let p = Arc::new(Scripted::default());
    let d = start(&k.home, p.clone(), None);
    let mut c = Client::connect(&d.info);

    // The session is listed, with the interrupted turn's prompt stored and no answer to it.
    match c.ask_for(&Request::ListSessions { limit: 10 }, |e| matches!(e, Event::SessionList { .. })) {
        Event::SessionList { sessions } => {
            let s = sessions.iter().find(|s| s.id == k.id).expect("the session survived the kill");
            assert_eq!(s.turns, 2, "the interrupted turn counts");
        }
        _ => unreachable!(),
    }
    let t = c.transcript(&k.id);
    assert_eq!(user_texts(&t).len(), 2);
    assert_eq!(t.last().unwrap().role, Role::User, "no half answer was kept");
    assert_eq!(tool_result_texts(&t), vec![("wrote app.conf".to_string(), false)], "turn 0's tool call is intact");
    assert_eq!(std::fs::read_to_string(k.ws.join("app.conf")).unwrap(), "v1");

    // parity S1: resuming continues the conversation from what was kept.
    let before = p.requests().len();
    let (_, _) = c.turn(Some(&k.id), "carry on", None);
    let req = &p.requests()[before];
    let history: Vec<&marlowe_model::Message> = req.messages.iter().collect();
    assert!(history.iter().any(|m| m.joined_text().contains("My favourite colour is teal")), "the interrupted prompt is in the history");
    assert!(history.iter().any(|m| m.tool_results_iter().any(|r| r.content == "wrote app.conf")));
    assert!(history.last().unwrap().joined_text().contains("carry on"));
    stop(d);
    let _ = std::fs::remove_dir_all(&k.base);
}

#[test]
fn parity_s1_continue_and_resume_find_the_session_after_the_daemon_died() {
    let k = killed(Kill::AfterTools);
    let other_ws = temp_dir("s1-other");
    let p = Arc::new(Scripted::default());
    let d = start(&k.home, p.clone(), None);
    let mut c = Client::connect(&d.info);
    // A newer session in another directory, so "the most recent session" is not the killed one.
    c.turn(None, "something elsewhere", Some(&other_ws));

    let sessions = match c.ask_for(&Request::ListSessions { limit: 50 }, |e| matches!(e, Event::SessionList { .. })) {
        Event::SessionList { sessions } => sessions,
        _ => unreachable!(),
    };
    assert_eq!(sessions[0].workspace.as_deref(), Some(other_ws.display().to_string().as_str()), "most recent first");
    // `marlowe -c`: the most recent session whose workspace is this directory.
    let here = k.ws.display().to_string();
    let latest = sessions.iter().find(|s| s.workspace.as_deref() == Some(here.as_str())).expect("-c finds the killed session");
    assert_eq!(latest.id, k.id);
    assert_eq!(latest.title, "My sister lives in Porto. Write the config.");
    assert_eq!(latest.turns, 2);

    // `marlowe -r <id>` / the picker: the whole transcript, then the conversation goes on.
    let t = c.transcript(&k.id);
    assert_eq!(user_texts(&t).len(), 2);
    let before = p.requests().len();
    c.turn(Some(&k.id), "continue", None);
    let req = &p.requests()[before];
    assert!(req.messages.iter().any(|m| m.joined_text().contains("My favourite colour is teal")));
    assert!(req.messages.last().unwrap().joined_text().contains("continue"));
    stop(d);
    let _ = std::fs::remove_dir_all(&k.base);
}

#[test]
fn a7_killed_after_tools_before_the_next_call_loses_no_completed_tool_call() {
    let k = killed(Kill::AfterTools);
    let p = Arc::new(Scripted::default());
    let d = start(&k.home, p.clone(), None);
    let mut c = Client::connect(&d.info);
    let t = c.transcript(&k.id);
    // Both calls of the interrupted batch, and their results, are in the history.
    assert_eq!(
        tool_result_texts(&t),
        vec![
            ("wrote app.conf".to_string(), false),
            ("wrote app.conf".to_string(), false),
            ("wrote new.txt".to_string(), false)
        ]
    );
    assert_eq!(t.last().unwrap().role, Role::Tool, "history ends on the finished batch");
    assert_eq!(std::fs::read_to_string(k.ws.join("app.conf")).unwrap(), "v2");
    assert_eq!(std::fs::read_to_string(k.ws.join("extra/new.txt")).unwrap(), "made");

    // Resuming: the model is shown the finished batch and the new prompt; nothing is re-run.
    let before = p.requests().len();
    c.turn(Some(&k.id), "what happened?", None);
    let req = &p.requests()[before];
    assert_eq!(req.messages.iter().flat_map(|m| m.tool_results_iter()).count(), 3);
    assert_eq!(std::fs::read_to_string(k.ws.join("app.conf")).unwrap(), "v2", "no tool ran again");
    stop(d);
    let _ = std::fs::remove_dir_all(&k.base);
}

#[test]
fn a7_killed_mid_batch_keeps_the_finished_call_and_marks_the_running_one() {
    let k = killed(Kill::MidBatch);
    let p = Arc::new(Scripted::default());
    let d = start(&k.home, p.clone(), None);
    let mut c = Client::connect(&d.info);
    // Opening the session is not enough; the first ask starts its runner, which repairs it.
    let before = p.requests().len();
    c.turn(Some(&k.id), "pick up where it stopped", None);
    let req = &p.requests()[before];
    let results: Vec<(String, bool)> =
        req.messages.iter().flat_map(|m| m.tool_results_iter()).map(|r| (r.content.clone(), r.is_error)).collect();
    assert_eq!(results.len(), 3, "turn 0's call and both calls of the killed batch each have a result: {results:?}");
    assert_eq!(results[1], ("wrote fast.txt".to_string(), false), "the call that finished keeps its real result");
    assert!(results[2].1 && results[2].0.contains("did not finish"), "the running call is marked: {results:?}");
    // The repair is stored: a later reload sees the same well-formed history.
    let t = c.transcript(&k.id);
    assert_eq!(tool_result_texts(&t).len(), 3);
    assert_eq!(std::fs::read_to_string(k.ws.join("fast.txt")).unwrap(), "done quickly");
    stop(d);
    let _ = std::fs::remove_dir_all(&k.base);
}

#[test]
fn a7_rewind_after_a_kill_is_byte_identical_and_the_rewound_turn_is_not_recalled() {
    let k = killed(Kill::AfterTools);
    let census = user_repo_census(&k.ws);
    let mind = open_mind(&k.home);
    let p = Arc::new(Scripted::default());
    let d = start(&k.home, p.clone(), Some(mind.clone()));
    let mut c = Client::connect(&d.info);

    let recall = |q: &str| -> Vec<String> {
        mind.recall(RecallQuery { text: q.into(), scope: None, limit: 10 }).unwrap().into_iter().map(|h| h.text).collect()
    };
    // Both statements were remembered as they were said.
    assert!(recall("Porto sister").iter().any(|t| t.contains("Porto")));
    assert!(recall("favourite colour teal").iter().any(|t| t.contains("teal")));

    // Before the kill's turn: put the files and the conversation back to the end of turn 0.
    let at_turn_0_end = {
        // The workspace as turn 1 found it: app.conf was written once, v1.
        let mut s = files(&k.ws);
        s.insert("app.conf".into(), b"v1".to_vec());
        s.remove("extra/new.txt");
        s
    };
    c.rewind(&k.id, 1, RewindScope::Both);
    assert_eq!(files(&k.ws), at_turn_0_end, "byte for byte: v1 back, the file turn 1 created gone");
    assert_eq!(user_texts(&c.transcript(&k.id)), vec!["My sister lives in Porto. Write the config.".to_string()]);
    assert_eq!(user_repo_census(&k.ws), census, "the user's repository: same objects, same refs, same index");

    // A fact said only in the rewound turn is never recalled again; the earlier one is.
    assert!(recall("favourite colour teal").is_empty(), "retracted: {:?}", recall("favourite colour teal"));
    assert!(recall("Porto sister").iter().any(|t| t.contains("Porto")));
    stop(d);
    // And it stays retracted after a restart of the memory service (snapshot, then full replay).
    drop(mind);
    for replay in [false, true] {
        if replay {
            let _ = std::fs::remove_file(k.home.join("profile").join("beliefs.snapshot"));
        }
        let again = open_mind(&k.home);
        let hits = again.recall(RecallQuery { text: "favourite colour teal".into(), scope: None, limit: 10 }).unwrap();
        assert!(hits.is_empty(), "replay={replay}: {hits:?}");
        assert!(!again.recall(RecallQuery { text: "Porto sister".into(), scope: None, limit: 10 }).unwrap().is_empty());
    }
    let _ = std::fs::remove_dir_all(&k.base);
}

#[test]
fn a7_a_conversation_rewind_keeps_the_older_checkpoints_reachable() {
    let base = temp_dir("a7-keep");
    let home = base.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let ws = project(&base);
    seed_model(&home, None);
    let p = Arc::new(Scripted::default());
    let d = start(&home, p.clone(), None);
    let mut c = Client::connect(&d.info);
    let t = three_turns(&d, &home, &ws, &p, &mut c);
    // Drop the conversation from turn 1 but keep the files, then rewind to turn 0 later: the
    // pre-turn-0 state is still what comes back, whatever turn numbers were reused.
    c.rewind(&t.id, 1, RewindScope::Conversation);
    p.push(tool_script(&[write("n1", "later.txt", "x")]));
    p.push(say("ok"));
    c.turn(Some(&t.id), "something new", None);
    assert!(c.checkpoints(&t.id).iter().any(|c| c.turn == 1), "the reused turn number has a fresh checkpoint");
    c.rewind(&t.id, 0, RewindScope::Both);
    assert_eq!(files(&ws), t.states[0]);
    stop(d);
}

// ---------------------------------------------------------------- review fixes

impl Client {
    /// Rewinds and returns every event up to `Rewound`, the errors the rewind reported included.
    fn rewind_events(&mut self, session: &SessionId, turn: u32, scope: RewindScope) -> Vec<Event> {
        self.send(&Request::Rewind { session: session.clone(), turn, scope });
        self.until(|e| matches!(e, Event::Rewound { .. })).into_iter().map(|e| e.event).collect()
    }

    fn cost(&mut self, session: &SessionId) -> marlowe_proto::CostReport {
        match self.ask_for(&Request::SessionCost { session: session.clone() }, |e| matches!(e, Event::SessionCost { .. })) {
            Event::SessionCost { report, .. } => report,
            _ => unreachable!(),
        }
    }
}

#[test]
fn parity_s4_a_rewind_does_not_give_the_money_back_and_an_empty_answer_is_billed() {
    let home = temp_dir("s4-rewind");
    let ws = temp_dir("s4-rewind-ws");
    seed_model(&home, Some(Pricing { input_per_mtok: 2.0, output_per_mtok: 10.0 }));
    let p = Arc::new(Scripted::default());
    p.push(tool_script(&[write("c1", "a.txt", "x")]));
    p.push(say("done"));
    p.push(say("second"));
    // The third answer has no text and no tool calls: no row to store, but it was billed. The
    // guard asks once more for a reply, which comes back empty too.
    p.push(vec![usage(500, 0), Step::Ev(StreamEvent::Stop(StopReason::EndTurn))]);
    p.push(vec![usage(500, 0), Step::Ev(StreamEvent::Stop(StopReason::EndTurn))]);
    let d = start(&home, p, None);
    let mut c = Client::connect(&d.info);
    let (id, _) = c.turn(None, "first", Some(&ws));
    c.turn(Some(&id), "second", None);
    c.turn(Some(&id), "third", None);

    let before = c.cost(&id);
    assert_eq!(before.calls, 5);
    assert_eq!((before.usage.input, before.usage.output), (4_000, 600));
    c.rewind(&id, 1, RewindScope::Both);
    assert_eq!(user_texts(&c.transcript(&id)), vec!["first".to_string()], "the later turns are gone");
    let after = c.cost(&id);
    assert_eq!((after.calls, after.usage.input, after.usage.output), (5, 4_000, 600), "{after:?}");
    assert_eq!(after.usage.cost_usd, before.usage.cost_usd);
    assert!(before.usage.cost_usd.unwrap() > 0.0);
    stop(d);
}

#[test]
fn a_reused_turn_number_restores_ignored_files_to_what_the_new_turn_found() {
    let base = temp_dir("reuse");
    let home = base.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let ws = project(&base);
    seed_model(&home, None);
    let before = files(&ws);
    let p = Arc::new(Scripted::default());
    let d = start(&home, p.clone(), None);
    let mut c = Client::connect(&d.info);
    p.push(tool_script(&[write("a", "notes.txt", "turn zero\\n")]));
    p.push(say("zero"));
    let (id, _) = c.turn(None, "turn zero", Some(&ws));
    // Turn 1 writes the ignored build log: v0 -> v1.
    p.push(tool_script(&[write("b", "out/build.log", "build v1\\n")]));
    p.push(say("one"));
    c.turn(Some(&id), "turn one", None);
    // The conversation goes back to turn 1; the files stay (v1). The reused turn 1 writes v2.
    c.rewind(&id, 1, RewindScope::Conversation);
    p.push(tool_script(&[write("c", "out/build.log", "build v2\\n")]));
    p.push(say("one again"));
    c.turn(Some(&id), "turn one, differently", None);
    assert_eq!(std::fs::read(ws.join("out/build.log")).unwrap(), b"build v2\n");

    c.rewind(&id, 1, RewindScope::Code);
    assert_eq!(
        std::fs::read(ws.join("out/build.log")).unwrap(),
        b"build v1\n",
        "the state the new turn 1 started with, not the older state of the turn it replaced"
    );
    // And all the way back, through the dropped turn too.
    c.rewind(&id, 0, RewindScope::Code);
    assert_eq!(files(&ws), before);
    stop(d);
}

#[test]
fn a_code_rewind_leaves_the_users_own_edits_alone_and_says_so() {
    let base = temp_dir("foreign");
    let home = base.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let ws = project(&base);
    seed_model(&home, None);
    let p = Arc::new(Scripted::default());
    let d = start(&home, p.clone(), None);
    let mut c = Client::connect(&d.info);
    p.push(tool_script(&[write("a", "notes.txt", "turn zero\\n"), write("b", "src/new.rs", "pub fn new() {}\\n")]));
    p.push(say("zero"));
    let (id, _) = c.turn(None, "turn zero", Some(&ws));
    // Between turns, in an editor: a new file, and an edit of a file the session never touched.
    put(&ws, "scratch/by-hand.txt", b"mine\n");
    put(&ws, "src/main.rs", b"fn main() { println!(\"mine\"); }\n");

    let events = c.rewind_events(&id, 0, RewindScope::Code);
    let said: Vec<String> = events.iter().filter_map(|e| if let Event::Error { message, .. } = e { Some(message.clone()) } else { None }).collect();
    assert_eq!(said.len(), 1, "{said:?}");
    assert!(said[0].contains("scratch/by-hand.txt") && said[0].contains("src/main.rs"), "{}", said[0]);
    assert!(said[0].contains("refs/marlowe/"), "the way back to the previous state is named: {}", said[0]);
    assert_eq!(std::fs::read(ws.join("notes.txt")).unwrap(), b"first\n", "the session's own edit is undone");
    assert!(!ws.join("src/new.rs").exists());
    assert_eq!(std::fs::read(ws.join("scratch/by-hand.txt")).unwrap(), b"mine\n");
    assert_eq!(std::fs::read(ws.join("src/main.rs")).unwrap(), b"fn main() { println!(\"mine\"); }\n");
    stop(d);
}

#[test]
fn a_code_rewind_refuses_while_another_session_is_running_in_the_workspace() {
    let home = temp_dir("sibling");
    let ws = temp_dir("sibling-ws");
    seed_model(&home, None);
    let p = Arc::new(Scripted::default());
    let d = start(&home, p.clone(), None);
    let mut c = Client::connect(&d.info);
    p.push(tool_script(&[write("a", "a.txt", "one")]));
    p.push(say("done"));
    let (a, _) = c.turn(None, "session a", Some(&ws));
    p.push(say("hello"));
    let (b, _) = c.turn(None, "session b", Some(&ws));
    assert_ne!(a, b);
    // Session b is mid-turn (its model call waits at a gate) in the same workspace.
    let (gate_tx, gate_rx) = crossbeam_channel::bounded::<()>(1);
    p.push(vec![Step::Gate(gate_rx), text("late"), Step::Ev(StreamEvent::Stop(StopReason::EndTurn))]);
    c.send(&Request::Ask { session: Some(b.clone()), text: "b again".into(), workspace: None, images: Vec::new(), options: None });
    c.until(|e| matches!(e, Event::TurnStarted { .. }));
    let msg = c.refused(&Request::Rewind { session: a.clone(), turn: 0, scope: RewindScope::Code });
    assert!(msg.contains("running a turn in this workspace"), "{msg}");
    assert!(ws.join("a.txt").exists(), "nothing was touched");
    gate_tx.send(()).unwrap();
    c.until(|e| matches!(e, Event::TurnFinished { .. }));
    c.rewind(&a, 0, RewindScope::Code);
    assert!(!ws.join("a.txt").exists());
    stop(d);
}

#[test]
fn a_session_asked_again_after_an_idle_eviction_records_each_turn_once() {
    let home = temp_dir("evict");
    seed_model(&home, None);
    let p = Arc::new(Scripted::default());
    p.push(say("first answer"));
    p.push(say("second answer"));
    let cfg = DaemonConfig { session_idle: Duration::from_millis(100), ..DaemonConfig::default() };
    let d = Daemon::start(deps(p.clone(), None), &home, cfg).expect("daemon starts");
    let info = d.info();
    let join = std::thread::spawn(move || d.wait());
    let mut c = Client::connect(&info);
    let (id, _) = c.turn(None, "one", None);
    // Long enough for the runner to be evicted and a new one to be built by the next ask.
    std::thread::sleep(Duration::from_millis(800));
    let (again, _) = c.turn(Some(&id), "two", None);
    assert_eq!(again, id);
    let conn = rusqlite::Connection::open(paths::cowork_db(&home)).unwrap();
    let mut st = conn.prepare("SELECT role, turn FROM turns WHERE session = ?1 ORDER BY idx").unwrap();
    let rows: Vec<(String, u32)> =
        st.query_map([id.as_str()], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(Result::unwrap).collect();
    assert_eq!(
        rows,
        [("user".to_string(), 0), ("assistant".to_string(), 0), ("user".to_string(), 1), ("assistant".to_string(), 1)],
        "each turn once, numbered from where the stored history ends"
    );
    let mut c2 = Client::connect(&info);
    c2.send(&Request::Shutdown);
    c2.until(|e| matches!(e, Event::ShuttingDown));
    join.join().unwrap().unwrap();
}
