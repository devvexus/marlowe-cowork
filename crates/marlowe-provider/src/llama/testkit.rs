//! Fakes for the engine's tests: a server process, a llama-server and an Ollama over real
//! loopback HTTP, so the engine's HTTP code runs as it does in production.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use super::engine::{EngineConfig, LlamaEngine};
use super::server::{Process, Spawner};
use crate::http::LocalEndpoint;
use crate::ollama_store::LaunchPlan;

static NEXT: AtomicUsize = AtomicUsize::new(0);

/// A directory removed on drop.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new() -> Self {
        let p = std::env::temp_dir().join(format!("marlowe-llama-test-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::SeqCst)));
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// What a fake server answers.
pub struct Reply {
    pub status: u16,
    pub content_type: &'static str,
    pub body: String,
}

impl Reply {
    pub fn json(v: Value) -> Self {
        Self { status: 200, content_type: "application/json", body: v.to_string() }
    }

    pub fn status(status: u16, v: Value) -> Self {
        Self { status, content_type: "application/json", body: v.to_string() }
    }

    pub fn sse(chunks: &[Value]) -> Self {
        let mut body = String::new();
        for c in chunks {
            body.push_str(&format!("data: {c}\n\n"));
        }
        body.push_str("data: [DONE]\n\n");
        Self { status: 200, content_type: "text/event-stream", body }
    }
}

pub type Handler = dyn Fn(&str, &str, &Value) -> Reply + Send + Sync;

/// A loopback HTTP server running `handler(method, path, body)`.
pub struct Http {
    pub port: u16,
    stop: Arc<AtomicBool>,
}

impl Http {
    pub fn start_on(port: u16, handler: Arc<Handler>) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", port)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        std::thread::spawn(move || {
            while !s.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((conn, _)) => {
                        let h = handler.clone();
                        std::thread::spawn(move || serve(conn, &*h));
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(5)),
                }
            }
        });
        Self { port, stop }
    }

    pub fn start(handler: Arc<Handler>) -> Self {
        let port = TcpListener::bind(("127.0.0.1", 0)).unwrap().local_addr().unwrap().port();
        Self::start_on(port, handler)
    }

    pub fn endpoint(&self) -> LocalEndpoint {
        LocalEndpoint::new("127.0.0.1", self.port).unwrap()
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

impl Drop for Http {
    fn drop(&mut self) {
        self.stop();
    }
}

fn serve(conn: TcpStream, handler: &Handler) {
    let _ = conn.set_nonblocking(false);
    let mut reader = BufReader::new(conn.try_clone().unwrap());
    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let mut parts = line.split_whitespace();
    let (method, path) = (parts.next().unwrap_or("").to_string(), parts.next().unwrap_or("").to_string());
    let mut len = 0usize;
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
            break;
        }
        if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
            len = v.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; len];
    let _ = reader.read_exact(&mut body);
    let value: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let r = handler(&method, &path, &value);
    let mut out = conn;
    let head = format!(
        "HTTP/1.1 {} X\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        r.status,
        r.content_type,
        r.body.len()
    );
    let _ = out.write_all(head.as_bytes());
    let _ = out.write_all(r.body.as_bytes());
    let _ = out.flush();
}

/// What the fake llama-server was asked.
#[derive(Default)]
pub struct Seen {
    pub chats: Mutex<Vec<Value>>,
    /// `save 0 file` / `restore 0 file`, in order.
    pub slot_calls: Mutex<Vec<String>>,
}

type Chat = dyn Fn(&Value) -> Reply + Send + Sync;

/// A fake `llama-server`: ready at once, answers `/health`, `/props`, `/slots`, and chat requests
/// through `chat`.
pub struct FakeSpawner {
    started: AtomicUsize,
    kills: Arc<AtomicUsize>,
    dying: Mutex<Option<String>>,
    chat: Mutex<Arc<Chat>>,
    pub seen: Arc<Seen>,
    pub plans: Mutex<Vec<LaunchPlan>>,
    /// How `/apply-template` renders a request (none: the server has no such route).
    apply: Mutex<Option<Arc<Apply>>>,
}

type Apply = dyn Fn(&Value) -> Reply + Send + Sync;

impl FakeSpawner {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            started: AtomicUsize::new(0),
            kills: Arc::new(AtomicUsize::new(0)),
            dying: Mutex::new(None),
            chat: Mutex::new(Arc::new(|_| Reply::sse(&[json!({"choices": [{"index": 0, "finish_reason": "stop", "delta": {"content": "ok"}}], "timings": {"prompt_n": 1, "cache_n": 0, "prompt_ms": 1.0, "predicted_n": 1, "predicted_ms": 1.0}})]))),
            seen: Arc::new(Seen::default()),
            plans: Mutex::new(Vec::new()),
            apply: Mutex::new(None),
        })
    }

    pub fn starts(&self) -> usize {
        self.started.load(Ordering::SeqCst)
    }

    pub fn killed(&self) -> usize {
        self.kills.load(Ordering::SeqCst)
    }

    pub fn die_at_start(&self, log_line: &str) {
        *self.dying.lock().unwrap() = Some(log_line.to_string());
    }

    pub fn apply_template(&self, f: impl Fn(&Value) -> Reply + Send + Sync + 'static) {
        *self.apply.lock().unwrap() = Some(Arc::new(f));
    }

    pub fn answer_chat(&self, f: impl Fn(&Value) -> Reply + Send + Sync + 'static) {
        *self.chat.lock().unwrap() = Arc::new(f);
    }
}

struct FakeProc {
    http: Option<Http>,
    dead: Option<i32>,
    kills: Arc<AtomicUsize>,
}

impl Process for FakeProc {
    fn id(&self) -> u32 {
        4242
    }

    fn exited(&mut self) -> Option<i32> {
        self.dead
    }

    fn kill(&mut self) {
        if self.dead.is_none() {
            self.dead = Some(-1);
            self.kills.fetch_add(1, Ordering::SeqCst);
        }
        if let Some(h) = self.http.take() {
            h.stop();
        }
    }
}

impl Spawner for FakeSpawner {
    fn start(&self, plan: &LaunchPlan, log: Option<&Path>) -> std::io::Result<Box<dyn Process>> {
        self.started.fetch_add(1, Ordering::SeqCst);
        self.plans.lock().unwrap().push(plan.clone());
        if let Some(line) = self.dying.lock().unwrap().clone() {
            if let Some(p) = log {
                let mut f = std::fs::OpenOptions::new().create(true).append(true).open(p)?;
                writeln!(f, "{line}")?;
            }
            return Ok(Box::new(FakeProc { http: None, dead: Some(1), kills: self.kills.clone() }));
        }
        let arg = |flag: &str| plan.args.iter().position(|a| a == flag).and_then(|i| plan.args.get(i + 1)).cloned().unwrap_or_default();
        let port: u16 = arg("--port").parse().unwrap();
        let slots: usize = arg("-np").parse().unwrap_or(1);
        let total: u64 = arg("-c").parse().unwrap_or(0);
        if let Some(p) = log {
            let mut f = std::fs::OpenOptions::new().create(true).append(true).open(p)?;
            writeln!(f, "load_tensors: offloaded 33/33 layers to GPU")?;
        }
        let chat = self.chat.lock().unwrap().clone();
        let apply = self.apply.lock().unwrap().clone();
        let seen = self.seen.clone();
        let slot_dir = PathBuf::from(arg("--slot-save-path"));
        let handler: Arc<Handler> = Arc::new(move |method, path, body| match (method, path) {
            ("GET", "/health") => Reply::json(json!({"status": "ok"})),
            ("GET", "/props") => Reply::json(json!({"total_slots": slots})),
            ("GET", "/slots") => Reply::json(Value::Array(
                (0..slots).map(|i| json!({"id": i, "n_ctx": total / slots.max(1) as u64, "is_processing": false})).collect(),
            )),
            ("POST", "/v1/chat/completions") => {
                seen.chats.lock().unwrap().push(body.clone());
                chat(body)
            }
            ("POST", "/apply-template") => match &apply {
                Some(f) => f(body),
                None => Reply::status(404, json!({"error": "not found"})),
            },
            ("POST", p) if p.starts_with("/slots/") => {
                // `/slots/{id}?action=save|restore` with a file name inside `--slot-save-path`.
                let (id, action) = p["/slots/".len()..].split_once("?action=").unwrap_or(("", ""));
                let file = body["filename"].as_str().unwrap_or("");
                if slot_dir.as_os_str().is_empty() {
                    return Reply::status(501, json!({"error": "slot save is not enabled"}));
                }
                seen.slot_calls.lock().unwrap().push(format!("{action} {id} {file}"));
                match action {
                    "save" => {
                        let _ = std::fs::write(slot_dir.join(file), b"kv");
                        Reply::json(json!({"id_slot": id, "filename": file}))
                    }
                    "restore" if slot_dir.join(file).is_file() => Reply::json(json!({"id_slot": id, "filename": file})),
                    _ => Reply::status(404, json!({"error": "no such file"})),
                }
            }
            _ => Reply::status(404, json!({"error": "not found"})),
        });
        Ok(Box::new(FakeProc { http: Some(Http::start_on(port, handler)), dead: None, kills: self.kills.clone() }))
    }

    fn free_port(&self) -> std::io::Result<u16> {
        Ok(TcpListener::bind(("127.0.0.1", 0))?.local_addr()?.port())
    }
}

/// A fake Ollama: `/api/ps` lists what is resident, `/api/generate` with `keep_alive: 0`
/// unloads, `/api/show` and `/api/tags` describe one chat model.
pub struct FakeOllama {
    http: Http,
    state: Arc<Mutex<(Vec<String>, Vec<String>)>>,
    chats: Arc<AtomicUsize>,
}

impl FakeOllama {
    pub fn new(model: &str) -> Self {
        let state: Arc<Mutex<(Vec<String>, Vec<String>)>> = Arc::new(Mutex::new((Vec::new(), Vec::new())));
        let s = state.clone();
        let chats = Arc::new(AtomicUsize::new(0));
        let c = chats.clone();
        let model = model.to_string();
        let handler: Arc<Handler> = Arc::new(move |method, path, body| match (method, path) {
            ("GET", "/api/ps") => {
                let resident = s.lock().unwrap().0.clone();
                Reply::json(json!({"models": resident.iter().map(|m| json!({"name": m, "context_length": 32768})).collect::<Vec<_>>()}))
            }
            ("POST", "/api/generate") => {
                let m = body.get("model").and_then(Value::as_str).unwrap_or_default().to_string();
                let mut g = s.lock().unwrap();
                g.0.retain(|x| x != &m);
                g.1.push(m);
                Reply::json(json!({"done": true}))
            }
            ("POST", "/api/show") => Reply::json(json!({
                "capabilities": ["completion", "tools", "thinking"],
                "model_info": {"general.architecture": "qwen35", "qwen35.context_length": 262144},
                "details": {"family": "qwen35"},
            })),
            ("GET", "/api/tags") => Reply::json(json!({"models": [{"name": model, "model": model}]})),
            ("POST", "/api/chat") => {
                c.fetch_add(1, Ordering::SeqCst);
                Reply {
                    status: 200,
                    content_type: "application/x-ndjson",
                    body: format!("{}\n", json!({"message": {"content": "from ollama"}, "done": true, "prompt_eval_count": 1, "eval_count": 1})),
                }
            }
            _ => Reply::status(404, json!({"error": "not found"})),
        });
        Self { http: Http::start(handler), state, chats }
    }

    /// How many `/api/chat` requests Ollama itself served.
    pub fn chats(&self) -> usize {
        self.chats.load(Ordering::SeqCst)
    }

    pub fn endpoint(&self) -> LocalEndpoint {
        self.http.endpoint()
    }

    pub fn resident(&self, model: &str) {
        self.state.lock().unwrap().0.push(model.to_string());
    }

    pub fn unloaded(&self) -> Vec<String> {
        self.state.lock().unwrap().1.clone()
    }
}

/// A store holding `model`, a fake Ollama, a fake spawner and a log, in one temp dir.
pub struct Fixture {
    pub dir: TempDir,
    pub ollama: FakeOllama,
    pub spawner: Arc<FakeSpawner>,
}

impl Fixture {
    pub fn new(model: &str) -> Self {
        let dir = TempDir::new();
        super::eligible::tests::store(
            &dir.path().join("store"),
            model,
            json!({"model_format": "gguf", "model_family": "qwen35"}),
            &[
                (crate::ollama_store::MODEL_MEDIA_TYPE, "gguf"),
                (crate::ollama_store::PARAMS_MEDIA_TYPE, r#"{"temperature":1,"top_k":20,"top_p":0.95,"presence_penalty":0,"repeat_penalty":1,"min_p":0}"#),
            ],
        );
        Self { ollama: FakeOllama::new(model), spawner: FakeSpawner::new(), dir }
    }

    pub fn config(&self) -> EngineConfig {
        let mut cfg = EngineConfig::new(self.ollama.endpoint(), self.dir.path().join("store"));
        cfg.log = Some(self.dir.path().join("llama.log"));
        cfg.binary = Some(self.dir.path().join("llama-server.exe"));
        cfg.poll = Duration::from_millis(10);
        cfg.idle_tick = Duration::from_millis(50);
        cfg.keep_alive = None;
        cfg.start_timeout = Duration::from_secs(10);
        cfg
    }

    pub fn engine(&self) -> LlamaEngine {
        LlamaEngine::new(self.config(), self.spawner.clone())
    }
}

/// A ChatML-ish template, as llama-server's `/apply-template` would answer: an assistant
/// turn's reasoning is printed only after the newest user message, unless `keep` is set.
pub fn chatml(body: &Value, keep: bool) -> String {
    let messages = body["messages"].as_array().cloned().unwrap_or_default();
    let kept = body["chat_template_kwargs"]["preserve_thinking"] == true && keep;
    let last_user = messages.iter().rposition(|m| m["role"] == "user").unwrap_or(0);
    let mut out = String::new();
    for (i, m) in messages.iter().enumerate() {
        let role = m["role"].as_str().unwrap_or("user");
        let text = m["content"].as_str().unwrap_or("");
        match role {
            "assistant" => {
                out.push_str("<|im_start|>assistant\n");
                if let Some(r) = m["reasoning_content"].as_str().filter(|_| kept || i > last_user) {
                    out.push_str(&format!("<think>\n{r}\n</think>\n\n"));
                }
                out.push_str(text);
                for c in m["tool_calls"].as_array().into_iter().flatten() {
                    out.push_str(&format!("<tool_call>{}</tool_call>", c["function"]["arguments"].as_str().unwrap_or("")));
                }
                out.push_str("<|im_end|>\n");
            }
            "tool" => out.push_str(&format!("<|im_start|>user\n<tool_response>\n{text}\n</tool_response><|im_end|>\n")),
            other => out.push_str(&format!("<|im_start|>{other}\n{text}<|im_end|>\n")),
        }
    }
    if body.get("add_generation_prompt") != Some(&json!(false)) {
        out.push_str("<|im_start|>assistant\n<think>\n");
    }
    out
}
