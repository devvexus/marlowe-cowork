//! What Ollama is doing with a model, for the context planner: the model's files (GGUF headers
//! of the main and DRAFT blobs, read from the store without loading anything), the runner log,
//! `/api/ps`, the runner's own `/slots`, and the server settings saved in the user environment.
//!
//! # Not ground truth: `/api/ps` sizes
//!
//! For llama-server runners Ollama 0.32 reports `size` = `size_vram` = the draft model plus the
//! recurrent state (1,631 MiB for a runner whose dedicated memory is 13,486 MiB). Only its model
//! list and `context_length` are used here; sizes come from the runner log and the GPU.
//!
//! # Changing the server
//!
//! `OLLAMA_KV_CACHE_TYPE`, `OLLAMA_NUM_PARALLEL` and `OLLAMA_FLASH_ATTENTION` are read once, when
//! the server starts, and apply to every model. [`OllamaServerControl::apply`] sets them in the
//! user environment and restarts `ollama serve`. The daemon calls it only after the user approved
//! that exact change; nothing in this crate's tests runs it.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use marlowe_hw::log::{self, LoadBlock};
use marlowe_hw::{
    gguf, ModelFiles, ModelHeader, ModelSource, RunnerObservation, ServerChange, ServerControl, ServerEnv, SlotObservation,
};
use serde_json::{json, Value};

use crate::http::{self, LocalEndpoint};
use crate::ollama_store::{self, manifest_relative_path};

/// The layer holding a speculative-decoding DRAFT model.
pub const DRAFT_MEDIA_TYPE: &str = "application/vnd.ollama.image.draft";
/// A separate vision projector.
pub const PROJECTOR_MEDIA_TYPE: &str = "application/vnd.ollama.image.projector";
/// How much of the end of `server.log` is read (it grows for the server's lifetime).
const LOG_TAIL_BYTES: u64 = 16 << 20;
const QUICK: Duration = Duration::from_millis(1500);

/// `%LOCALAPPDATA%\Ollama\server.log` (Windows), `~/.ollama/logs/server.log` elsewhere.
pub fn server_log_path() -> Option<PathBuf> {
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        let p = PathBuf::from(local).join("Ollama").join("server.log");
        if p.is_file() {
            return Some(p);
        }
    }
    let home = std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")).ok()?;
    let p = PathBuf::from(home).join(".ollama").join("logs").join("server.log");
    p.is_file().then_some(p)
}

/// The last `max` bytes of a file, from the first whole line.
pub fn read_tail(path: &Path, max: u64) -> std::io::Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    let start = len.saturating_sub(max);
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::with_capacity((len - start) as usize);
    f.read_to_end(&mut buf)?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    Ok(if start > 0 { text.split_once('\n').map(|(_, t)| t.to_string()).unwrap_or(text) } else { text })
}

/// One `/api/ps` entry. Its sizes are not used (see the module docs).
#[derive(Debug, Clone, PartialEq)]
pub struct PsModel {
    pub name: String,
    pub context_length: Option<u32>,
    pub size: u64,
    pub size_vram: u64,
}

pub fn parse_ps(v: &Value) -> Vec<PsModel> {
    v.get("models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|m| {
            Some(PsModel {
                name: m.get("name").or_else(|| m.get("model"))?.as_str()?.to_string(),
                context_length: m.get("context_length").and_then(Value::as_u64).map(|c| c as u32),
                size: m.get("size").and_then(Value::as_u64).unwrap_or(0),
                size_vram: m.get("size_vram").and_then(Value::as_u64).unwrap_or(0),
            })
        })
        .collect()
}

pub fn parse_slots(v: &Value) -> Vec<SlotObservation> {
    v.as_array()
        .into_iter()
        .flatten()
        .map(|s| SlotObservation {
            id: s.get("id").and_then(Value::as_u64).unwrap_or(0) as u32,
            n_ctx: s.get("n_ctx").and_then(Value::as_u64).unwrap_or(0) as u32,
            n_prompt_tokens: s.get("n_prompt_tokens").and_then(Value::as_u64).unwrap_or(0) as u32,
            is_processing: s.get("is_processing").and_then(Value::as_bool).unwrap_or(false),
        })
        .collect()
}

/// A user environment variable as saved (what the next Ollama start reads), not this process's
/// copy, which is as old as the daemon.
pub fn saved_user_env(name: &str) -> Option<String> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let out = Command::new("reg")
            .args(["query", r"HKCU\Environment", "/v", name])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        parse_reg_value(&String::from_utf8_lossy(&out.stdout), name)
    }
    #[cfg(not(windows))]
    {
        std::env::var(name).ok()
    }
}

/// The value from `reg query` output: `    NAME    REG_SZ    value`.
pub fn parse_reg_value(out: &str, name: &str) -> Option<String> {
    out.lines().find_map(|l| {
        let mut it = l.split_whitespace();
        (it.next()? .eq_ignore_ascii_case(name)).then_some(())?;
        let _ty = it.next()?;
        let v: Vec<&str> = it.collect();
        (!v.is_empty()).then(|| v.join(" "))
    })
}

/// The planner's view of Ollama.
pub struct OllamaHw {
    endpoint: LocalEndpoint,
    store: PathBuf,
    log: Option<PathBuf>,
}

impl OllamaHw {
    pub fn new(endpoint: LocalEndpoint) -> Self {
        Self { endpoint, store: ollama_store::store_root(), log: server_log_path() }
    }

    /// With an explicit store and log (tests).
    pub fn with_paths(endpoint: LocalEndpoint, store: PathBuf, log: Option<PathBuf>) -> Self {
        Self { endpoint, store, log }
    }

    /// Every layer of `model`'s manifest as (media type, blob path, size).
    fn layers(&self, model: &str) -> Vec<(String, PathBuf, u64)> {
        let Ok(rel) = manifest_relative_path(model) else { return Vec::new() };
        let Ok(text) = std::fs::read_to_string(self.store.join(rel)) else { return Vec::new() };
        let Ok(v) = serde_json::from_str::<Value>(&text) else { return Vec::new() };
        v.get("layers")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|l| {
                let media = l.get("mediaType")?.as_str()?.to_string();
                let digest = l.get("digest")?.as_str()?;
                let size = l.get("size").and_then(Value::as_u64).unwrap_or(0);
                Some((media, self.store.join("blobs").join(digest.replace(':', "-")), size))
            })
            .collect()
    }

    fn show(&self, model: &str) -> Result<Value, String> {
        http::post_json(&self.endpoint, "/api/show", &json!({ "model": model }), Duration::from_secs(10))
            .map_err(|e| format!("/api/show {model}: {e}"))
    }

    /// The log's launches, and the architectures it said it serves one request at a time.
    fn logs(&self) -> (Vec<LoadBlock>, Vec<String>) {
        let text = self.log.as_deref().and_then(|p| read_tail(p, LOG_TAIL_BYTES).ok()).unwrap_or_default();
        (log::parse(&text), log::serial_architectures(&text))
    }
}

impl ModelSource for OllamaHw {
    fn files(&self, model_id: &str) -> Result<ModelFiles, String> {
        let quant = self
            .show(model_id)
            .ok()
            .and_then(|v| v.pointer("/details/quantization_level").and_then(Value::as_str).map(str::to_string))
            .filter(|q| !q.is_empty());
        match ollama_store::resolve_in(&self.store, model_id) {
            Ok(resolved) => {
                let main = gguf::read_header(&resolved.blob).map_err(|e| format!("{}: {e}", resolved.blob.display()))?;
                let layers = self.layers(model_id);
                let draft = layers
                    .iter()
                    .find(|(m, ..)| m == DRAFT_MEDIA_TYPE)
                    .map(|(_, p, _)| gguf::read_header(p).map_err(|e| format!("draft {}: {e}", p.display())))
                    .transpose()?;
                let mmproj_bytes = layers.iter().filter(|(m, ..)| m == PROJECTOR_MEDIA_TYPE).map(|(.., s)| s).sum();
                Ok(ModelFiles {
                    main,
                    draft,
                    mmproj_bytes,
                    blob: Some(resolved.blob.display().to_string()),
                    source: "GGUF header".into(),
                    quant,
                })
            }
            // A model the store cannot resolve (a remote Ollama, a renamed store): /api/show has
            // the same keys and tensor list, but arrays come back null.
            Err(e) => {
                let v = self.show(model_id).map_err(|s| format!("{} ({s})", e.remedy()))?;
                let main = ModelHeader::from_show(&v)?;
                Ok(ModelFiles { main, draft: None, mmproj_bytes: 0, blob: None, source: "/api/show".into(), quant })
            }
        }
    }

    fn runtime(&self, model_id: &str, blob: Option<&str>) -> RunnerObservation {
        let ps = http::get_json(&self.endpoint, "/api/ps", QUICK).map(|v| parse_ps(&v)).unwrap_or_default();
        let entry = ps.iter().find(|p| p.name == model_id);
        let (blocks, serial_archs) = self.logs();
        let live = live_launches();
        let mine = blob.and_then(|b| live.iter().find(|l| l.model.as_deref().is_some_and(|m| same_file(m, b)))).cloned();
        let load = match (blob.and_then(|b| log::last_for_blob(&blocks, b)).cloned(), mine) {
            // The log describes the runner that is running: it has the buffers too.
            (Some(logged), Some(l)) if logged.launch.port == l.port => Some(logged),
            // A server started from a console logs there, not to server.log: the log's launch is
            // an older one, and the runner's own argv is the truth.
            (_, Some(l)) => Some(LoadBlock { launch: l, ..LoadBlock::default() }),
            (logged, None) => logged,
        };
        let slots = match (entry, load.as_ref().and_then(|l| l.launch.port)) {
            (Some(_), Some(port)) => LocalEndpoint::new("127.0.0.1", port)
                .and_then(|ep| http::get_json(&ep, "/slots", QUICK).ok())
                .map(|v| parse_slots(&v))
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        RunnerObservation {
            // The server-wide settings of the last launch of any model stand for the running
            // server when this model has not loaded since it started.
            load: load
                .or_else(|| live.first().map(|l| LoadBlock { launch: l.clone(), ..LoadBlock::default() }))
                .or_else(|| blocks.last().map(|b| LoadBlock { launch: b.launch.clone(), ..LoadBlock::default() })),
            loaded: entry.is_some(),
            ps_context: entry.and_then(|e| e.context_length),
            slots,
            saved: ServerEnv {
                kv_cache_type: saved_user_env("OLLAMA_KV_CACHE_TYPE"),
                num_parallel: saved_user_env("OLLAMA_NUM_PARALLEL"),
                flash_attention: saved_user_env("OLLAMA_FLASH_ATTENTION"),
            },
            serial_archs,
        }
    }

    fn loaded_models(&self) -> Vec<String> {
        self.resident().unwrap_or_default()
    }

    fn resident(&self) -> Option<Vec<String>> {
        let v = http::get_json(&self.endpoint, "/api/ps", QUICK).ok()?;
        Some(parse_ps(&v).into_iter().map(|p| p.name).collect())
    }

    fn unload(&self, model_id: &str) -> Result<(), String> {
        // `keep_alive: 0` on a request with no prompt unloads the model at once.
        http::post_json(&self.endpoint, "/api/generate", &unload_body(model_id), Duration::from_secs(8))
            .map(|_| ())
            .map_err(|e| format!("could not unload {model_id}: {e}"))
    }
}

/// The launches of the runners running now, read from their own command lines: `llama-server.exe`,
/// and `ollama.exe runner` from older Ollama.
fn live_launches() -> Vec<log::Launch> {
    let servers = marlowe_os::process::command_lines("llama-server.exe").into_iter();
    let runners = marlowe_os::process::command_lines("ollama.exe").into_iter().filter(|(_, c)| c.split_whitespace().any(|w| w == "runner"));
    servers.chain(runners).map(|(_, c)| log::launch_from_command_line(&c)).collect()
}

/// Whether two paths name the same blob file (`...\blobs\sha256-17c3...`).
fn same_file(a: &str, b: &str) -> bool {
    let name = |p: &str| p.rsplit(['\\', '/']).next().unwrap_or(p).to_ascii_lowercase();
    name(a) == name(b)
}

/// The request that unloads `model` from Ollama.
pub fn unload_body(model: &str) -> Value {
    json!({ "model": model, "keep_alive": 0 })
}

/// Sets Ollama's user environment variables and restarts `ollama serve`. See the module docs.
pub struct OllamaServerControl {
    endpoint: LocalEndpoint,
    /// Variables removed from every process this starts (API keys never reach Ollama).
    scrub: Vec<String>,
    /// Whether `ensure_running` may start Ollama (off for a remote server or a scripted run).
    auto_start: bool,
    /// Where a server this starts writes its output, kept for reading the prompt read from it.
    own_log: Option<PathBuf>,
}

impl OllamaServerControl {
    pub fn new(endpoint: LocalEndpoint) -> Self {
        Self { endpoint, scrub: Vec::new(), auto_start: true, own_log: None }
    }

    /// Keeps the output of an `ollama serve` this starts in `path` (size-capped).
    pub fn with_own_log(mut self, path: PathBuf) -> Self {
        self.own_log = Some(path);
        self
    }

    /// Whether a stopped Ollama is started when something needs it (default on).
    pub fn auto_start(mut self, on: bool) -> Self {
        self.auto_start = on;
        self
    }

    /// Removes these variables (the credential list) from every child's environment.
    pub fn with_scrub<I: IntoIterator<Item = S>, S: Into<String>>(mut self, vars: I) -> Self {
        self.scrub = vars.into_iter().map(Into::into).collect();
        self
    }
}

impl ServerControl for OllamaServerControl {
    fn ensure_running(&self, progress: &mut dyn FnMut(&str)) -> marlowe_hw::EngineStart {
        if !self.auto_start {
            return marlowe_hw::EngineStart::NotApplicable;
        }
        let io = crate::ollama_start::SystemIo { endpoint: self.endpoint.clone(), scrub: self.scrub.clone(), log: self.own_log.clone() };
        crate::ollama_start::ensure_running(&io, &self.endpoint, crate::ollama_start::START_TIMEOUT, progress)
    }

    fn log_feed(&self) -> Option<Box<dyn marlowe_hw::LogFeed>> {
        // A remote Ollama's files are not on this machine.
        if !self.auto_start {
            return None;
        }
        let feed = crate::ollama_log::OllamaLogFeed::new(self.own_log.as_deref(), server_log_path().as_deref());
        (!feed.is_empty()).then(|| Box::new(feed) as Box<dyn marlowe_hw::LogFeed>)
    }

    fn apply(&self, change: &ServerChange) -> Result<String, String> {
        if change.is_empty() {
            return Ok("nothing to change".into());
        }
        let vars = change.variables();
        for (k, v) in &vars {
            let ok = hidden("setx", &[k, v], &self.scrub).map(|s| s.success()).unwrap_or(false);
            if !ok {
                return Err(format!("could not set {k} in your user environment (setx failed)"));
            }
        }
        // Quit the tray app and the server (its runners go with it), then start `ollama serve` again so
        // the server reads the new environment.
        let _ = hidden("taskkill", &["/IM", "ollama app.exe", "/F"], &self.scrub);
        let _ = hidden("taskkill", &["/IM", "ollama.exe", "/F", "/T"], &self.scrub);
        std::thread::sleep(Duration::from_millis(800));
        // Always `ollama serve`, hidden and detached, never the tray app. The environment is the
        // saved one (the values just set included, which this process predates), credentials
        // scrubbed, and the server's output goes to Marlowe's capped log.
        use crate::ollama_start::StartIo as _;
        let io = crate::ollama_start::SystemIo { endpoint: self.endpoint.clone(), scrub: self.scrub.clone(), log: self.own_log.clone() };
        let Some(cli) = io.cli() else {
            return Err(format!("set {}; start Ollama yourself (`ollama` is not installed where Marlowe looks)", names(&vars)));
        };
        let mut env = io.environment();
        for (k, v) in &vars {
            env.retain(|(name, _)| name != k);
            env.push((k.to_string(), v.clone()));
        }
        io.spawn(&cli, &["serve"], &env).map_err(|e| format!("starting Ollama: {e}"))?;
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(30) {
            if http::get_json(&self.endpoint, "/api/tags", QUICK).is_ok() {
                return Ok(format!("set {} and restarted Ollama", names(&vars)));
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        Err(format!("set {}; Ollama did not answer within 30 s of restarting", names(&vars)))
    }
}

fn names(vars: &[(&str, String)]) -> String {
    vars.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(", ")
}

fn hidden(program: &str, args: &[&str], scrub: &[String]) -> std::io::Result<std::process::ExitStatus> {
    let mut c = Command::new(program);
    c.args(args);
    for k in scrub {
        c.env_remove(k);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000);
    }
    c.output().map(|o| o.status)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ps_and_slots_parse_and_reg_output_is_read() {
        // Verbatim shape of /api/ps on 2026-10-09 (sizes there are wrong for these runners).
        let ps = parse_ps(&json!({"models":[{"name":"marlowe-dusk:27b-super","size":1710468627u64,"size_vram":1710468627u64,"context_length":32768}]}));
        assert_eq!(ps[0].context_length, Some(32_768));
        let slots = parse_slots(&json!([{"id":0,"n_ctx":32768,"is_processing":true,"n_prompt_tokens":1187}]));
        assert_eq!(slots[0], SlotObservation { id: 0, n_ctx: 32_768, n_prompt_tokens: 1187, is_processing: true });
        let out = "\r\nHKEY_CURRENT_USER\\Environment\r\n    OLLAMA_KV_CACHE_TYPE    REG_SZ    q8_0\r\n\r\n";
        assert_eq!(parse_reg_value(out, "OLLAMA_KV_CACHE_TYPE").as_deref(), Some("q8_0"));
        assert_eq!(parse_reg_value(out, "OLLAMA_NUM_PARALLEL"), None);
    }

    #[test]
    fn a_store_model_reads_its_main_and_draft_headers() {
        let dir = std::env::temp_dir().join(format!("marlowe-ollama-hw-{}", std::process::id()));
        let blobs = dir.join("blobs");
        let man = dir.join("manifests/registry.ollama.ai/library/m");
        std::fs::create_dir_all(&blobs).unwrap();
        std::fs::create_dir_all(&man).unwrap();
        let main = gguf::write_header(
            &[("general.architecture", json!("llama")), ("llama.block_count", json!(1)), ("llama.embedding_length", json!(64))],
            &[("blk.0.attn_k.weight", 1, vec![64, 64])],
        );
        let draft = gguf::write_header(&[("general.architecture", json!("llama")), ("llama.block_count", json!(1))], &[]);
        std::fs::write(blobs.join("sha256-aa"), &main).unwrap();
        std::fs::write(blobs.join("sha256-bb"), &draft).unwrap();
        std::fs::write(
            man.join("t"),
            json!({"layers":[
                {"mediaType":"application/vnd.ollama.image.model","digest":"sha256:aa","size":main.len()},
                {"mediaType":DRAFT_MEDIA_TYPE,"digest":"sha256:bb","size":draft.len()}]})
            .to_string(),
        )
        .unwrap();
        // A dead endpoint: /api/show and /api/ps fail, the files are still read.
        let hw = OllamaHw::with_paths(LocalEndpoint::new("127.0.0.1", 9).unwrap(), dir.clone(), None);
        let f = hw.files("m:t").unwrap();
        assert_eq!(f.main.arch(), Some("llama"));
        assert!(f.draft.is_some());
        assert_eq!(f.source, "GGUF header");
        let rt = hw.runtime("m:t", f.blob.as_deref());
        assert!(!rt.loaded && rt.load.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
