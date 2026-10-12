//! `marlowe headless --probe-first-call <cases.jsonl>`: what a model does with the real tool
//! surface on its first call. Nothing runs.
//!
//! Each case builds the REAL host for the model and window (the binary's tool factory, the MCP
//! host over a case's stub servers, the memory and `dispatch` declarations, the persona blocks, the
//! prefix fit and the provider's wire), sends the case's request through the agent loop and
//! records every tool call three ways: as the provider parsed it (`raw`), as the loop made it
//! canonical (`prepared`) and as the forgiving layer left it (`layer`; none when the layer refused
//! the call, whose answer is then the `result`). Every call that gets through is answered with
//! `probe: not run` (or the case's canned answer), the turn stops after `max_calls` steps, and no
//! approval is ever shown. One JSON line per run goes to stdout; `eval/tool_probe/probe.py` grades.
//!
//! Hosts: `conductor` (built-ins, MCP, memory, dispatch) or `subagent:<type>` (the type's filtered
//! host with the subagent-only tools). Local models only: a probe is free or it does not run.
//!
//! The wire is the shipped one: `MARLOWE_OLLAMA_TYPED_TOOLS=1` sends the typed schemas (arm C).

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::unbounded;
use marlowe_agent::args::ArgRewrites;
use marlowe_agent::context::tool_spec_tokens;
use marlowe_agent::subagent::{builtin_types, find_type, FilteredTools, SUBAGENT_FRAME};
use marlowe_agent::{
    Agent, CallCtx, CallFlow, Conversation, Environment, Steer, SystemPrompt, ToolCallReq, ToolHost, ToolMiddleware, ToolOutcome, UserInfo,
};
use marlowe_brains::{CredentialStore, MemoryStore};
use marlowe_mcp::client::ConnectOpts;
use marlowe_mcp::config::{self, Paths};
use marlowe_mcp::hub::{Hub, HubOpts};
use marlowe_mcp::oauth::MemoryTokens;
use marlowe_model::{
    CancelToken, ChatRequest, Completion, ModelInfo, ModelRef, Part, Provider, ProviderError, ProviderKind, Role, StreamEvent, SystemBlock, ToolCall,
};
use marlowe_proto::SessionId;
use serde::Deserialize;
use serde_json::{json, Map, Value};

pub const USAGE: &str = "\
marlowe headless --probe-first-call <cases.jsonl> [flags]

  --model M        the model, ollama/<tag> or a bare Ollama tag (default marlowe-dawn:9b)
  --num-ctx N      the window (default 32768)
  --runs N         runs per case (default 3: temperature 0, then the model's default)
  --temps LIST     one entry per run, a number or `default` (overrides --runs)
  --only IDS       comma list of case ids
  --dry            build every case's host and print its tool surface; ask no model
  --calibrate      also send each host's first request without tools, to read the tools' real cost
  --timeout SECS   per run (default 300)
  --out FILE       write the JSON lines here instead of stdout

One JSON line per run. MARLOWE_OLLAMA_TYPED_TOOLS=1 sends typed schemas (the C arm).
";

const FIXTURE: &[(&str, &str)] = &[
    (".git/HEAD", "ref: refs/heads/main\n"),
    ("Cargo.toml", "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
    ("README.md", "# fixture\n\nMarlowe 0.3\n"),
    ("src/lib.rs", "pub fn fit_prefix() {}\n"),
    ("src/net.rs", "const MAX_RETRY: u32 = 3;\n"),
];

#[derive(Debug, Clone, Deserialize)]
pub struct Case {
    pub id: String,
    #[serde(default)]
    pub tool: String,
    #[serde(default = "conductor")]
    pub host: String,
    pub request: String,
    /// Harness-side state the model would have been told (a running job, a stored memory), put
    /// ahead of the request as a reminder.
    #[serde(default)]
    pub reminder: Option<String>,
    /// Extra files in the fixture repository.
    #[serde(default)]
    pub files: BTreeMap<String, String>,
    /// `--mcp-config` values; `{probe_dir}` is the cases file's folder.
    #[serde(default)]
    pub mcp: Vec<Value>,
    /// Model steps before the probe stops the turn.
    #[serde(default = "one")]
    pub max_calls: u32,
    /// Answers by tool name for calls that get through (default `probe: not run`).
    #[serde(default)]
    pub canned: BTreeMap<String, String>,
}

fn conductor() -> String {
    "conductor".into()
}

fn one() -> u32 {
    1
}

pub struct Probe {
    pub provider: Arc<dyn Provider>,
    pub model: ModelInfo,
    pub temps: Vec<Option<f32>>,
    pub calibrate: bool,
    /// Build every case's host and announce it, but ask no model.
    pub dry: bool,
    pub timeout: Duration,
    /// The built-in host for the conductor's model in a workspace (the binary's tool factory).
    pub tools: Arc<dyn Fn(&Path, &ModelInfo) -> Arc<dyn ToolHost> + Send + Sync>,
    pub root: PathBuf,
    pub probe_dir: PathBuf,
}

pub fn run(_home: &Path, raw: &[String]) -> Result<i32, String> {
    let mut it = raw.iter();
    let mut cases_path = None;
    let (mut model, mut num_ctx, mut runs, mut temps, mut only) = ("marlowe-dawn:9b".to_string(), 32_768u32, 3usize, None::<String>, None::<String>);
    let (mut calibrate, mut dry, mut timeout, mut out) = (false, false, 300u64, None::<PathBuf>);
    while let Some(a) = it.next() {
        let mut val = |f: &str| it.next().cloned().ok_or(format!("{f} needs a value"));
        match a.as_str() {
            "--probe-first-call" => cases_path = Some(PathBuf::from(val(a)?)),
            "--model" => model = val(a)?,
            "--num-ctx" => num_ctx = val(a)?.parse().map_err(|_| "--num-ctx is not a number".to_string())?,
            "--runs" => runs = val(a)?.parse().map_err(|_| "--runs is not a number".to_string())?,
            "--temps" => temps = Some(val(a)?),
            "--only" => only = Some(val(a)?),
            "--calibrate" => calibrate = true,
            "--dry" => dry = true,
            "--timeout" => timeout = val(a)?.parse().map_err(|_| "--timeout is not a number".to_string())?,
            "--out" => out = Some(PathBuf::from(val(a)?)),
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(0);
            }
            other => return Err(format!("unknown option {other:?}\n{USAGE}")),
        }
    }
    let cases_path = cases_path.ok_or_else(|| format!("--probe-first-call needs a cases file\n{USAGE}"))?;
    let cases_path = if cases_path.is_absolute() { cases_path } else { std::env::current_dir().map_err(|e| e.to_string())?.join(cases_path) };
    let mut cases = load_cases(&cases_path)?;
    if let Some(ids) = only {
        let want: Vec<&str> = ids.split(',').map(str::trim).collect();
        cases.retain(|c| want.contains(&c.id.as_str()));
    }
    let temps = match temps {
        Some(t) => t.split(',').map(|t| parse_temp(t.trim())).collect::<Result<Vec<_>, _>>()?,
        None => (0..runs).map(|i| (i == 0).then_some(0.0)).collect(),
    };
    let model_ref = match model.parse::<ModelRef>() {
        Ok(m) => m,
        Err(_) => ModelRef::new(ProviderKind::Ollama, model.clone()),
    };
    if model_ref.provider != ProviderKind::Ollama {
        return Err(format!("{model_ref}: the probe runs local models only"));
    }
    let scratch = std::env::temp_dir().join(format!("marlowe-probe-{}", std::process::id()));
    let home = scratch.join("home");
    std::fs::create_dir_all(&home).map_err(|e| e.to_string())?;
    let credentials: Arc<dyn CredentialStore> = Arc::new(MemoryStore::new());
    let factory = crate::wire::provider_factory(credentials.clone(), &home);
    factory.allocations().set(&model_ref.model_id, num_ctx);
    let provider = factory.provider(ProviderKind::Ollama).map_err(|e| format!("ollama: {e}"))?;
    let info = ModelInfo { context_window: Some(num_ctx), supports_tools: true, local: true, ..ModelInfo::bare(ProviderKind::Ollama, model_ref.model_id.clone()) };
    let tools_factory = crate::wire::tool_factory(credentials, &home);
    let tools = Arc::new(move |root: &Path, model: &ModelInfo| {
        tools_factory(&marlowe_daemon::HostRequest { workspace: Some(root.to_path_buf()), model: Some(model.clone()), session: None })
    });
    let probe = Probe {
        provider,
        model: info,
        temps,
        calibrate,
        dry,
        timeout: Duration::from_secs(timeout),
        tools,
        root: scratch.join("ws"),
        probe_dir: cases_path.parent().map(Path::to_path_buf).unwrap_or_default(),
    };
    let mut sink: Box<dyn Write> = match &out {
        Some(p) => Box::new(std::fs::File::create(p).map_err(|e| format!("{}: {e}", p.display()))?),
        None => Box::new(std::io::stdout()),
    };
    let result = probe.run_cases(&cases, &mut *sink);
    let _ = std::fs::remove_dir_all(&scratch);
    result.map(|()| 0)
}

/// Names a case's host: its kind plus a digest of its MCP servers.
fn surface_key(case: &Case) -> String {
    use sha2::{Digest, Sha256};
    if case.mcp.is_empty() {
        return case.host.clone();
    }
    let digest = Sha256::digest(Value::Array(case.mcp.clone()).to_string().as_bytes());
    format!("{}+mcp-{}", case.host, digest.iter().take(3).map(|b| format!("{b:02x}")).collect::<String>())
}

fn parse_temp(t: &str) -> Result<Option<f32>, String> {
    if t.eq_ignore_ascii_case("default") {
        return Ok(None);
    }
    t.parse().map(Some).map_err(|_| format!("--temps: {t:?} is not a number or `default`"))
}

pub fn load_cases(path: &Path) -> Result<Vec<Case>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    text.lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty() && !l.trim_start().starts_with("//"))
        .map(|(i, l)| serde_json::from_str::<Case>(l).map_err(|e| format!("{}:{}: {e}", path.display(), i + 1)))
        .collect()
}

/// The provider as the loop sees it: records what it parsed, sets the temperature and, once, asks
/// the same question without tools.
struct Tap {
    inner: Arc<dyn Provider>,
    temperature: Option<f32>,
    calibrate: bool,
    log: Mutex<TapLog>,
}

#[derive(Default)]
struct TapLog {
    /// Each model call's parsed tool calls, in order.
    raw: Vec<Vec<ToolCall>>,
    input_tokens: Vec<u64>,
    text: Vec<String>,
    /// (without tools, with tools) prompt tokens, from the calibration pair.
    calibration: Option<(u64, u64)>,
    asked_without_tools: bool,
}

impl Provider for Tap {
    fn kind(&self) -> ProviderKind {
        self.inner.kind()
    }

    fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        self.inner.list_models()
    }

    fn stream(&self, req: &ChatRequest, cancel: &CancelToken, on: &mut dyn FnMut(StreamEvent)) -> Result<Completion, ProviderError> {
        let mut req = req.clone();
        if self.temperature.is_some() {
            req.temperature = self.temperature;
        }
        let first = !std::mem::replace(&mut self.lock().asked_without_tools, true);
        if self.calibrate && first {
            // Two one-token requests behind a nonce, so the server's prompt cache cannot shorten
            // either count: the difference is what the tool schemas cost on this model's template.
            let ask = |tools: bool, tag: &str| {
                let mut r = req.clone();
                if !tools {
                    r.tools.clear();
                }
                r.max_output_tokens = Some(1);
                r.system.insert(0, SystemBlock { text: format!("[calibration {tag}-{}]", std::process::id()), cache: false });
                self.inner.stream(&r, cancel, &mut |_| {}).ok().map(|c| c.usage.input)
            };
            let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or_default();
            let tag = format!("{nanos:x}");
            let (without, with) = (ask(false, &format!("a{tag}")), ask(true, &format!("b{tag}")));
            self.lock().calibration = without.zip(with);
        }
        let done = self.inner.stream(&req, cancel, on)?;
        let mut log = self.lock();
        log.raw.push(done.tool_calls.clone());
        log.input_tokens.push(done.usage.input);
        log.text.push(done.text.clone());
        Ok(done)
    }
}

impl Tap {
    fn lock(&self) -> std::sync::MutexGuard<'_, TapLog> {
        self.log.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Last in the chain, after the forgiving layer: records the call as the layer left it and
/// answers it. A call the layer refused never gets here. A tool the host does not offer (one a
/// subagent type lacks, which the layer lets on to the host) gets the host's refusal, unrecorded.
struct Answer {
    host: Arc<dyn ToolHost>,
    canned: BTreeMap<String, String>,
    seen: Mutex<Vec<(ToolCall, Option<Value>)>>,
}

impl ToolMiddleware for Answer {
    fn name(&self) -> &str {
        "probe"
    }

    fn before_approval(&self, call: &mut ToolCall, cx: &CallCtx) -> CallFlow {
        if self.host.tool_spec(&call.name).is_none() {
            return CallFlow::Done(ToolOutcome::error(format!("`{}` is not available to this agent", call.name)));
        }
        // A workflow script is checked the way `workflow{validate_only}` checks it (nothing runs).
        let checked = (call.name == "workflow" && call.args.get("script").is_some_and(Value::is_string)).then(|| {
            let mut args = call.args.clone();
            args["validate_only"] = json!(true);
            let out = self.host.call_scoped(&ToolCallReq { id: call.id.clone(), name: call.name.clone(), args }, cx);
            json!({"ok": !out.is_error, "text": clip(&out.content, 400)})
        });
        self.seen.lock().unwrap_or_else(|e| e.into_inner()).push((call.clone(), checked));
        let text = self.canned.get(&call.name).map(String::as_str).unwrap_or(marlowe_daemon::probe::NOT_RUN);
        CallFlow::Done(ToolOutcome::ok(text, "probe"))
    }
}

impl Probe {
    pub fn run_cases(&self, cases: &[Case], out: &mut dyn Write) -> Result<(), String> {
        let _ = std::fs::remove_dir_all(&self.root);
        let mut calibrated: Vec<String> = Vec::new();
        let mut announced: Vec<String> = Vec::new();
        for case in cases {
            let surface = match self.surface(case) {
                Ok(s) => s,
                Err(e) => {
                    emit(out, json!({"kind": "run", "case": case.id, "tool": case.tool, "host": case.host, "run": 0, "error": e, "calls": []}))?;
                    continue;
                }
            };
            let specs = surface.host.specs();
            let key = surface_key(case);
            if !announced.contains(&key) {
                let chars: usize = specs.iter().map(|s| s.name.len() + s.description.len() + s.input_schema.to_string().len()).sum();
                let system = surface.system.persona_text();
                emit(out, json!({"kind": "surface", "host": case.host, "key": key, "specs": specs, "tools_chars": chars, "persona_chars": system.len(), "tools_estimate": tool_spec_tokens(&specs)}))?;
                announced.push(key.clone());
            }
            if self.dry {
                emit(out, json!({"kind": "case", "case": case.id, "tool": case.tool, "surface": key}))?;
                continue;
            }
            for (run, temp) in self.temps.iter().enumerate() {
                let calibrate = self.calibrate && !calibrated.contains(&key);
                if calibrate {
                    calibrated.push(key.clone());
                }
                let line = self.one_run(case, &surface, run, *temp, calibrate);
                let mut line = line;
                line["surface"] = json!(key);
                emit(out, line)?;
            }
        }
        Ok(())
    }

    fn one_run(&self, case: &Case, surface: &Surface, run: usize, temp: Option<f32>, calibrate: bool) -> Value {
        let tap = Arc::new(Tap { inner: self.provider.clone(), temperature: temp, calibrate, log: Mutex::new(TapLog::default()) });
        let answer = Arc::new(Answer { host: surface.host.clone(), canned: case.canned.clone(), seen: Mutex::new(Vec::new()) });
        let mut agent = Agent::new(tap.clone(), self.model.clone(), surface.host.clone(), surface.system.clone());
        agent.budget.max_steps = case.max_calls.max(1);
        agent.max_wall = Some(self.timeout);
        agent.middleware.push(answer.clone());
        let mut convo = Conversation::new(SessionId(format!("probe-{}-{run}", case.id)), Some(self.root.clone()));
        let text = match &case.reminder {
            Some(r) => format!("<system-reminder>\n{r}\n</system-reminder>\n\n{}", case.request),
            None => case.request.clone(),
        };
        let (_keep, inbox) = unbounded::<Steer>();
        let started = Instant::now();
        let outcome = agent.run_turn(&mut convo, &text, &CancelToken::new(), &inbox, &mut |_| {});
        let ms = started.elapsed().as_millis() as u64;

        let log = tap.lock();
        let laid: Vec<(ToolCall, Option<Value>)> = answer.seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
        // The transcript holds each call as the loop made it canonical, with its result.
        let mut prepared: Vec<(ToolCall, Option<(String, bool)>)> = Vec::new();
        for m in &convo.messages {
            for p in &m.content {
                match (m.role, p) {
                    (Role::Assistant, Part::ToolCall(c)) => prepared.push((c.clone(), None)),
                    (_, Part::ToolResult(r)) => {
                        if let Some(slot) = prepared.iter_mut().find(|(c, res)| c.id == r.call_id && res.is_none()) {
                            slot.1 = Some((r.content.clone(), r.is_error));
                        }
                    }
                    _ => {}
                }
            }
        }
        let raw: Vec<&ToolCall> = log.raw.iter().flatten().collect();
        // The calls that got through, in order; ids repeat across steps on some wires, so a call
        // matches the next one in line by id and name.
        let mut next = 0;
        let calls: Vec<Value> = prepared
            .iter()
            .enumerate()
            .map(|(i, (c, res))| {
                let through = laid.get(next).filter(|(l, _)| l.id == c.id && l.name == c.name);
                next += usize::from(through.is_some());
                json!({
                    "raw": raw.get(i).map(|r| json!({"name": r.name, "args": r.args})),
                    "prepared": {"name": c.name, "args": c.args},
                    "layer": through.map(|(l, _)| json!({"name": l.name, "args": l.args})),
                    "validated": through.and_then(|(_, v)| v.clone()),
                    "refused": through.is_none(),
                    "result": res.as_ref().map(|r| r.0.clone()),
                })
            })
            .collect();
        let rewrites: Vec<String> = convo
            .ext
            .get::<ArgRewrites>()
            .map(|r| r.0.iter().map(|w| if w.coerced { format!("{}: {} coerced", w.tool, w.from) } else { format!("{}: {} -> {}", w.tool, w.from, w.to) }).collect())
            .unwrap_or_default();
        let mut line = json!({
            "kind": "run", "case": case.id, "tool": case.tool, "host": case.host, "run": run, "temperature": temp,
            "model": self.model.id, "calls": calls, "steps": log.raw.len(), "ms": ms,
            "text": log.text.first().map(|t| clip(t, 600)).unwrap_or_default(),
            "stop": format!("{:?}", outcome.stop), "error": outcome.error, "rewrites": rewrites,
            "prompt_tokens": log.input_tokens,
        });
        if let Some((without, with)) = log.calibration {
            line["calibration"] = json!({"without_tools": without, "with_tools": with});
        }
        line
    }

    /// The host and prompt a case runs under, with its fixture written.
    fn surface(&self, case: &Case) -> Result<Surface, String> {
        std::fs::create_dir_all(&self.root).map_err(|e| e.to_string())?;
        for (rel, body) in FIXTURE.iter().map(|(p, b)| (p.to_string(), b.to_string())).chain(case.files.clone()) {
            let path = self.root.join(&rel);
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            }
            if !path.exists() || case.files.contains_key(&rel) {
                std::fs::write(&path, body).map_err(|e| format!("{}: {e}", path.display()))?;
            }
        }
        let (kind, name) = case.host.split_once(':').unwrap_or((case.host.as_str(), ""));
        let builtin = (self.tools)(&self.root, &self.model);
        let mut env = Environment::current(Some(self.root.display().to_string()), self.model.model_ref().to_string());
        let user = UserInfo { name: Some("Matthew".into()), address: None };
        match kind {
            "conductor" => {
                let hub = self.hub(case)?;
                let host = marlowe_daemon::probe::conductor_host(builtin, hub.map(|h| (h, self.model.context_window)));
                env.hints = host.prompt_hints();
                let system = marlowe_daemon::probe::conductor_system(host.as_ref()).user(user).environment(env);
                Ok(Surface { host, system })
            }
            "subagent" => {
                let types = builtin_types();
                let ty = find_type(&types, name).ok_or_else(|| format!("no subagent type {name:?}"))?;
                let home = self.root.join("home");
                let extras = marlowe_agent::subagent_tools::extras_for(Some(&home), Some(&self.root));
                let host: Arc<dyn ToolHost> = Arc::new(FilteredTools::new(builtin, ty).with_extras(extras));
                let core = format!("{}\n\n{SUBAGENT_FRAME}", ty.prompt.trim());
                Ok(Surface { host, system: SystemPrompt::with_core(core).environment(env) })
            }
            other => Err(format!("unknown host {other:?} (conductor, subagent:<type>)")),
        }
    }

    fn hub(&self, case: &Case) -> Result<Option<Arc<Hub>>, String> {
        if case.mcp.is_empty() {
            return Ok(None);
        }
        let dir = self.probe_dir.display().to_string().replace('\\', "/");
        let configs: Vec<String> = case.mcp.iter().map(|v| v.to_string().replace("{probe_dir}", &dir)).collect();
        let home = self.root.join("home");
        let mut opts = HubOpts::new(Paths::new(&home, Some(&self.root)), ConnectOpts::new(Arc::new(MemoryTokens::default())));
        opts.session = config::parse_session(&configs)?;
        opts.strict = true;
        let hub = Hub::start(opts);
        if !hub.wait_settled(Duration::from_secs(30)) {
            return Err("the case's MCP servers did not connect in time".into());
        }
        // A server that did not come up would leave the case measuring nothing about MCP.
        let down: Vec<String> = hub.statuses().into_iter().filter(|s| s.tools == 0).map(|s| format!("{}: {}", s.name, s.status.label())).collect();
        if !down.is_empty() {
            return Err(format!("MCP server not usable: {}", down.join("; ")));
        }
        Ok(Some(hub))
    }
}

struct Surface {
    host: Arc<dyn ToolHost>,
    system: SystemPrompt,
}

fn clip(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn emit(out: &mut dyn Write, line: Value) -> Result<(), String> {
    let mut obj: Map<String, Value> = line.as_object().cloned().unwrap_or_default();
    obj.retain(|_, v| !v.is_null());
    writeln!(out, "{}", Value::Object(obj)).and_then(|()| out.flush()).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use marlowe_model::StopReason;
    use std::collections::VecDeque;

    /// Answers each model call with the next scripted list of tool calls (no calls: a text reply).
    struct Seq {
        replies: Mutex<VecDeque<Vec<(&'static str, Value)>>>,
        temps: Mutex<Vec<Option<f32>>>,
        tools: Mutex<Vec<usize>>,
    }

    impl Provider for Seq {
        fn kind(&self) -> ProviderKind {
            ProviderKind::Ollama
        }

        fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
            Ok(Vec::new())
        }

        fn stream(&self, req: &ChatRequest, _: &CancelToken, _: &mut dyn FnMut(StreamEvent)) -> Result<Completion, ProviderError> {
            self.temps.lock().unwrap().push(req.temperature);
            self.tools.lock().unwrap().push(req.tools.len());
            // The calibration requests (one token out) take no scripted reply.
            let calls = if req.max_output_tokens == Some(1) { Vec::new() } else { self.replies.lock().unwrap().pop_front().unwrap_or_default() };
            let stop = if calls.is_empty() { StopReason::EndTurn } else { StopReason::ToolUse };
            let tool_calls = calls.into_iter().enumerate().map(|(i, (name, args))| ToolCall { id: format!("c{i}"), name: name.into(), args }).collect();
            Ok(Completion { text: "ok".into(), thinking: None, tool_calls, usage: Default::default(), stop })
        }
    }

    fn probe(replies: Vec<Vec<(&'static str, Value)>>, temps: Vec<Option<f32>>) -> (Probe, Arc<Seq>, PathBuf) {
        let dir = std::env::temp_dir().join(format!("marlowe-probe-test-{}-{}", std::process::id(), marlowe_mcp::util::random_token(5)));
        let home = dir.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let seq = Arc::new(Seq { replies: Mutex::new(replies.into()), temps: Mutex::new(Vec::new()), tools: Mutex::new(Vec::new()) });
        let factory = crate::wire::tool_factory(Arc::new(MemoryStore::new()), &home);
        let tools = Arc::new(move |root: &Path, model: &ModelInfo| {
            factory(&marlowe_daemon::HostRequest { workspace: Some(root.to_path_buf()), model: Some(model.clone()), session: None })
        });
        let model = ModelInfo { context_window: Some(32_768), supports_tools: true, local: true, ..ModelInfo::bare(ProviderKind::Ollama, "scripted") };
        let p = Probe { provider: seq.clone(), model, temps, calibrate: true, dry: false, timeout: Duration::from_secs(60), tools, root: dir.join("ws"), probe_dir: dir.clone() };
        (p, seq, dir)
    }

    fn case(v: Value) -> Case {
        serde_json::from_value(v).unwrap()
    }

    fn lines(buf: Vec<u8>) -> Vec<Value> {
        let all: Vec<Value> = String::from_utf8(buf).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(all[0]["kind"], "surface", "a host's tool surface is announced before its first run");
        all.into_iter().filter(|l| l["kind"] == "run").collect()
    }

    #[test]
    fn a_call_is_captured_raw_and_after_the_forgiving_layer_and_nothing_runs() {
        let (p, seq, dir) = probe(vec![vec![("read", json!({"file_path": "src/lib.rs", "line": 460}))]], vec![Some(0.0)]);
        let mut out = Vec::new();
        p.run_cases(&[case(json!({"id": "read", "tool": "read", "request": "Show src/lib.rs from line 460"}))], &mut out).unwrap();
        let run = &lines(out)[0];
        let call = &run["calls"][0];
        assert_eq!(call["raw"]["args"], json!({"file_path": "src/lib.rs", "line": 460}), "raw is what the provider parsed: {run}");
        assert_eq!(call["layer"]["args"]["path"], "src/lib.rs", "{run}");
        assert_eq!(call["layer"]["args"]["offset"], 460, "{run}");
        assert_eq!(call["refused"], false);
        assert_eq!(call["result"], "probe: not run");
        assert!(run["rewrites"].as_array().unwrap().len() >= 2, "{run}");
        assert_eq!(run["stop"], "StepLimit", "the turn stops after max_calls steps: {run}");
        assert_eq!(*seq.temps.lock().unwrap().last().unwrap(), Some(0.0));
        assert_eq!(run["calibration"], json!({"without_tools": 0, "with_tools": 0}), "{run}");
        assert_eq!(run["surface"], "conductor");
        // The calibration request (first) carries no tools, the real one carries them.
        let tools = seq.tools.lock().unwrap().clone();
        assert_eq!((tools[0], tools[1] > 20, tools[2] > 20), (0, true, true), "{tools:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn two_steps_use_the_canned_answer_and_an_invalid_call_gets_the_signature() {
        let replies = vec![vec![("task_list", json!({}))], vec![("task_stop", json!({"id": "wf_7"}))], vec![("web_fetch", json!({}))]];
        let (p, _seq, dir) = probe(replies, vec![None]);
        let mut out = Vec::new();
        let cancel = case(json!({
            "id": "cancel", "request": "cancel the workflow", "max_calls": 2,
            "canned": {"task_list": "wf_7  workflow  running (started in another conversation)"}
        }));
        let fetch = case(json!({"id": "fetch", "request": "fetch it"}));
        p.run_cases(&[cancel, fetch], &mut out).unwrap();
        let runs = lines(out);
        let calls = runs[0]["calls"].as_array().unwrap();
        assert_eq!(calls.iter().map(|c| c["prepared"]["name"].as_str().unwrap()).collect::<Vec<_>>(), ["task_list", "task_stop"], "{}", runs[0]);
        assert!(calls[0]["result"].as_str().unwrap().contains("wf_7"));
        assert_eq!(calls[1]["layer"]["args"]["id"], "wf_7");
        let bad = &runs[1]["calls"][0];
        assert_eq!(bad["refused"], true, "{}", runs[1]);
        assert!(bad["layer"].is_null() && bad["result"].as_str().unwrap().contains("web_fetch("), "{bad}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_subagent_host_has_its_own_tools_only() {
        let (p, _seq, dir) = probe(vec![vec![("handback", json!({"text": "Found 3 reads"}))], vec![("write", json!({"path": "a", "content": "b"}))]], vec![None]);
        let mut out = Vec::new();
        let hand = case(json!({"id": "handback", "host": "subagent:explore", "request": "find reads"}));
        let write = case(json!({"id": "nowrite", "host": "subagent:explore", "request": "write a file"}));
        let missing = case(json!({"id": "missing", "host": "subagent:nope", "request": "x"}));
        p.run_cases(&[hand, write, missing], &mut out).unwrap();
        let runs = lines(out);
        assert_eq!(runs[0]["calls"][0]["layer"]["name"], "handback", "{}", runs[0]);
        assert_eq!(runs[1]["calls"][0]["refused"], true, "an explore agent has no write: {}", runs[1]);
        assert!(runs[2]["error"].as_str().unwrap().contains("no subagent type"), "{}", runs[2]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_dry_run_announces_each_surface_once_and_asks_no_model() {
        let (mut p, seq, dir) = probe(Vec::new(), vec![None]);
        p.dry = true;
        let mut out = Vec::new();
        let cases = [
            case(json!({"id": "a", "tool": "read", "request": "x"})),
            case(json!({"id": "b", "tool": "grep", "request": "y"})),
            case(json!({"id": "c", "tool": "handback", "host": "subagent:explore", "request": "z"})),
        ];
        p.run_cases(&cases, &mut out).unwrap();
        let all: Vec<Value> = String::from_utf8(out).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        let surfaces: Vec<&Value> = all.iter().filter(|l| l["kind"] == "surface").collect();
        assert_eq!(surfaces.iter().map(|s| s["key"].as_str().unwrap()).collect::<Vec<_>>(), ["conductor", "subagent:explore"]);
        let names: Vec<&str> = surfaces[0]["specs"].as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap()).collect();
        for want in ["read", "git", "workflow", "remember", "recall", "dispatch"] {
            assert!(names.contains(&want), "{want} missing: {names:?}");
        }
        assert!(surfaces[0]["tools_chars"].as_u64().unwrap() > 10_000 && surfaces[0]["persona_chars"].as_u64().unwrap() > 3_000);
        assert_eq!(all.iter().filter(|l| l["kind"] == "case").count(), 3);
        assert!(seq.temps.lock().unwrap().is_empty(), "no model call in a dry run");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_case_can_bring_an_mcp_stub_server_and_its_tools_are_called_by_name() {
        let stubs = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("eval").join("tool_probe");
        let has_python = std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join("python.exe").is_file() || d.join("python").is_file()));
        if !has_python || !stubs.join("mcp_stub.py").is_file() {
            return;
        }
        let (mut p, _seq, dir) = probe(vec![vec![("mcp__notes__search_notes", json!({"query": "tokenizer"}))], vec![("mcp__notes__search_notes", json!({"q": "x"}))]], vec![None]);
        p.probe_dir = stubs;
        let mcp = json!([{"mcpServers": {"notes": {"command": "python", "args": ["-I", "{probe_dir}/mcp_stub.py"]}}}]);
        let mut out = Vec::new();
        let ok = case(json!({"id": "mcp", "request": "use the notes server to find the tokenizer notes", "mcp": mcp}));
        p.run_cases(&[ok], &mut out).unwrap();
        let all: Vec<Value> = String::from_utf8(out).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        let surface = all.iter().find(|l| l["kind"] == "surface").unwrap();
        let names: Vec<&str> = surface["specs"].as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"mcp__notes__search_notes") && names.contains(&"list_mcp_resources"), "{names:?}");
        assert!(surface["key"].as_str().unwrap().contains("+mcp-"), "{surface}");
        let run = all.iter().find(|l| l["kind"] == "run").unwrap();
        assert_eq!(run["calls"][0]["layer"]["name"], "mcp__notes__search_notes", "{run}");
        assert_eq!(run["calls"][0]["refused"], false);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_workflow_script_is_validated_not_run() {
        let good = "META = {\"name\": \"probe\", \"description\": \"x\"}
def main(args):
    return {\"ok\": True}
";
        let (p, _seq, dir) = probe(vec![vec![("workflow", json!({"script": good}))], vec![("workflow", json!({"script": "this is not starlark ("}))]], vec![None]);
        let mut out = Vec::new();
        let a = case(json!({"id": "good", "request": "spawn a workflow"}));
        let b = case(json!({"id": "bad", "request": "spawn a workflow"}));
        p.run_cases(&[a, b], &mut out).unwrap();
        let runs = lines(out);
        assert_eq!(runs[0]["calls"][0]["validated"]["ok"], true, "{}", runs[0]);
        assert_eq!(runs[1]["calls"][0]["validated"]["ok"], false, "{}", runs[1]);
        assert_eq!(runs[0]["calls"][0]["result"], "probe: not run");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_cases_file_skips_comments_and_names_the_bad_line() {
        let dir = std::env::temp_dir().join(format!("marlowe-probe-cases-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("c.jsonl");
        std::fs::write(&f, "// comment\n{\"id\":\"a\",\"request\":\"x\"}\n\n{\"id\":\"b\"}\n").unwrap();
        let e = load_cases(&f).unwrap_err();
        assert!(e.contains(":4:") && e.contains("request"), "{e}");
        std::fs::write(&f, "// comment\n{\"id\":\"a\",\"request\":\"x\",\"accept\":[{\"name\":\"read\"}]}\n").unwrap();
        let cases = load_cases(&f).unwrap();
        assert_eq!((cases.len(), cases[0].host.as_str(), cases[0].max_calls), (1, "conductor", 1), "grading fields the harness ignores are fine");
        let _ = std::fs::remove_dir_all(dir);
    }
}
