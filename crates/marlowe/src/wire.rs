//! The only place concrete provider and tool crates are named.
//!
//! Owned by track K from M4.6 (with `doctor.rs`). Tracks that configure tool hosts do it in their
//! own `marlowe-exec/src/specs/<group>.rs` (`configure`, run here through
//! `marlowe_exec::specs::configure`); the MCP host is composed in the daemon (`stack.rs`).

use std::path::Path;
use std::sync::Arc;

use marlowe_agent::{ToolHost, UserInfo};
use marlowe_brains::{CredentialStore, KeyringStore, MemoryStore, ProviderConfig, ProviderFactory};
use marlowe_crew::{CrewConfig, CrewSetup, CrewToolFactory, Sandbox, SandboxKind};
use marlowe_daemon::{DaemonDeps, HostRequest, SentinelLink, ToolHostFactory, VoiceLink};
use marlowe_exec::{Leftovers, ResearchConfig, SearchBackend, SearchResponse};
use marlowe_model::image::{BlobStore, FsBlobStore};
use marlowe_model::{
    CancelToken, Provider, ProviderError, ProviderKind,
};
use marlowe_provider::ollama::repair::RepairStats;

/// Sentinel events queued for the daemon's forwarder; a full queue drops, never blocks a watcher.
const SENTINEL_QUEUE: usize = 1024;
/// Voice events (envelope chunks at 30 Hz) queued for the forwarder.
const VOICE_QUEUE: usize = 256;

/// Tool-call repair counts across every provider this process builds (A4; `marlowe bench`).
pub fn repair_stats() -> Arc<RepairStats> {
    static STATS: std::sync::OnceLock<Arc<RepairStats>> = std::sync::OnceLock::new();
    STATS.get_or_init(|| Arc::new(RepairStats::new())).clone()
}

fn make_ollama(
    cfg: &ProviderConfig,
    blobs: Arc<dyn BlobStore>,
    engine: Option<marlowe_provider::llama::LlamaEngine>,
) -> Result<Arc<dyn Provider>, ProviderError> {
    // Every request for a model sends that model's context policy window (one allocation per
    // model), so Ollama never reloads a runner because two agents asked for different sizes.
    let mut p = marlowe_provider::OllamaProvider::from_base_url(cfg.base_url.as_deref())?
        .with_allocations(cfg.context.clone())
        .with_blobs(blobs)
        .with_repair_stats(repair_stats());
    // The models the context policy gives Marlowe's own llama-server run there, not on Ollama.
    if let Some(e) = engine {
        p = p.with_llama_engine(e);
    }
    Ok(Arc::new(p))
}

/// Marlowe's parallel local engine: Ollama's bundled llama-server, started by Marlowe for the
/// models Ollama hands to llama-server whole. Off for a scripted run, a remote Ollama and
/// `MARLOWE_LLAMA_ENGINE=0`.
fn llama_engine(home: &Path, endpoint: &marlowe_provider::LocalEndpoint) -> Option<marlowe_provider::llama::LlamaEngine> {
    use marlowe_provider::llama::{keep_alive_from, EngineConfig, LlamaEngine, SystemSpawner};
    if scripted() || std::env::var("MARLOWE_LLAMA_ENGINE").is_ok_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "off" | "false" | "no")) {
        return None;
    }
    let mut cfg = EngineConfig::new(endpoint.clone(), marlowe_provider::ollama_store::store_root());
    cfg.log = Some(home.join("logs").join("llama-server.log"));
    // Each conversation's cache is written here when the server stops and read back on the next load.
    cfg.slots_dir = Some(home.join("llama-slots"));
    // Ollama's own keep-alive: the setting it was started with, else five minutes.
    let keep = marlowe_provider::ollama_runtime::saved_user_env("OLLAMA_KEEP_ALIVE").or_else(|| std::env::var("OLLAMA_KEEP_ALIVE").ok());
    cfg.keep_alive = keep_alive_from(keep.as_deref());
    let scrub = marlowe_brains::scrubbed_env().iter().map(|s| s.to_string()).collect();
    Some(LlamaEngine::new(cfg, Arc::new(SystemSpawner::new(scrub))))
}

/// Never fails: the catalog works without a key; `stream` then returns an Auth error.
fn make_openrouter(cfg: &ProviderConfig, blobs: Arc<dyn BlobStore>) -> Result<Arc<dyn Provider>, ProviderError> {
    Ok(Arc::new(
        marlowe_openrouter::OpenRouterProvider::from_key(cfg.api_key.as_deref())
            .with_blobs(blobs)
            .with_repair_stats(repair_stats()),
    ))
}

/// Provider factory with Ollama and OpenRouter registered, keys from Credential Manager. Images
/// are read from `<home>\blobs`, the store `read`, paste (`PutBlob`) and the browser write.
pub fn provider_factory(credentials: Arc<dyn CredentialStore>, home: &Path) -> ProviderFactory {
    let mut f = ProviderFactory::new(credentials);
    let blobs: Arc<dyn BlobStore> = Arc::new(FsBlobStore::new(home.join("blobs")));
    let b = blobs.clone();
    let ollama_url = std::env::var("MARLOWE_OLLAMA_ADDR").ok();
    let endpoint = ollama_url
        .as_deref()
        .and_then(marlowe_provider::LocalEndpoint::parse)
        .unwrap_or_else(marlowe_provider::LocalEndpoint::default_ollama);
    let engine = llama_engine(home, &endpoint);
    if let Some(e) = &engine {
        // Loads and idle unloads show as background work once the daemon is listening.
        e.follow(f.allocations().clone());
    }
    let for_provider = engine.clone();
    f.register(ProviderKind::Ollama, move |cfg| make_ollama(cfg, b.clone(), for_provider.clone()));
    f.register(ProviderKind::OpenRouter, move |cfg| make_openrouter(cfg, blobs.clone()));
    if let Some(url) = &ollama_url {
        f.set_base_url(ProviderKind::Ollama, url.clone());
    }
    // The context planner reads Ollama's store, its runner log and the GPU; it changes nothing
    // but the server settings, and only after an approval. A scripted run has no engine to read:
    // nothing loads, so nothing is gated.
    if !scripted() {
        let ollama_hw = marlowe_provider::OllamaHw::new(endpoint.clone());
        // With the engine in the build the planner also sees what it runs and which models it may.
        let models: Arc<dyn marlowe_hw::ModelSource> = match engine {
            Some(e) => Arc::new(marlowe_provider::llama::LlamaModels::new(ollama_hw, e, marlowe_provider::ollama_store::store_root())),
            None => Arc::new(ollama_hw),
        };
        f.set_hardware(marlowe_brains::Hardware {
            models,
            gpu: Arc::new(marlowe_sentinel::vram::SentinelGpu::open()),
            server: Arc::new(
                marlowe_provider::OllamaServerControl::new(endpoint)
                    // A remote Ollama is not ours to start.
                    .auto_start(ollama_url.as_deref().is_none_or(|u| marlowe_provider::LocalEndpoint::parse(u).is_some()))
                    .with_own_log(home.join("logs").join("ollama-serve.log"))
                    .with_scrub(marlowe_brains::scrubbed_env().iter().copied()),
            ),
        });
    }
    #[cfg(debug_assertions)]
    crate::scripted::install(&mut f);
    f
}

/// Debug builds with `MARLOWE_SCRIPTED_PROVIDER` set: no real model or search call is possible.
#[cfg(debug_assertions)]
fn scripted() -> bool {
    crate::scripted::active()
}

#[cfg(not(debug_assertions))]
fn scripted() -> bool {
    false
}

/// `web_search` over the keyless web channel deep research uses (SearXNG, else DuckDuckGo lite),
/// configured by `<home>\research.json`. One registry per factory, so a resting backend stays rested.
fn search_backend(home: &Path) -> Arc<dyn SearchBackend> {
    if scripted() {
        return Arc::new(|_: &str, _: &CancelToken| Err::<SearchResponse, String>("web_search is off under the scripted provider".into()));
    }
    let settings = crate::research_wire::ResearchSettings::load(home);
    Arc::new(crate::research_wire::KeylessSearch::new(home, &settings))
}

/// arXiv and paper settings: the on-disk cache under `<home>\cache`, the global rate limit.
fn research(home: &Path) -> ResearchConfig {
    ResearchConfig::default().with_cache_dir(home.join("cache"))
}

/// Tool host factory: builtin tools rooted at the session workspace, exposing the groups the
/// session's model gets, configured by every group's `configure`. With no workspace every write
/// asks.
pub fn tool_factory(home: &Path) -> ToolHostFactory {
    let home = home.to_path_buf();
    let search = search_backend(&home);
    Arc::new(move |req: &HostRequest| {
        let mut tools = marlowe_exec::BuiltinTools::new(req.workspace.clone())
            .with_search(search.clone())
            .with_research(research(&home));
        if let Some(model) = &req.model {
            tools = tools.with_model_exposure(model);
        }
        let setup = marlowe_exec::specs::HostSetup { home: &home, workspace: req.workspace.as_deref() };
        let tools = marlowe_exec::specs::configure(tools, &setup);
        if let Some(session) = &req.session {
            marlowe_exec::specs::shell::note_session(&tools, &session.0);
        }
        Arc::new(tools) as Arc<dyn ToolHost>
    })
}

/// A crew run's inner tool host: rooted in its sandbox (reads stay free anywhere, so the original
/// workspace is readable), shells started in the sandbox with the no-push git environment, and
/// every process the run leaves behind killed with the host. `CrewTools` cuts it to the role's
/// allowlist and applies the sandbox's shell policy on top.
///
/// The sandbox lives under the profile in app data, which the broad-root rule refuses, so it is
/// trusted explicitly: writes inside are free and reads inside (or in the original workspace) do
/// not taint the run.
pub fn crew_tool_factory(home: &Path) -> CrewToolFactory {
    let home = home.to_path_buf();
    let search = search_backend(&home);
    Arc::new(move |sandbox: &Sandbox, workspace: Option<&Path>| {
        let tools = crew_host(sandbox, workspace)
            .with_search(search.clone())
            .with_research(research(&home))
            .with_child_env(sandbox.child_env())
            .with_shell_dir(sandbox.root.clone())
            .with_leftovers(Leftovers::KillWithHost);
        Arc::new(crew_configure(tools, &home, workspace)) as Arc<dyn ToolHost>
    })
}

/// The groups a crew host gets from the profile home: `git` without `gh` (crew pushes and PRs go
/// through `Request::OpenPr`, track J) and the verify group's state (logs, screenshots, the Edge
/// driver; track D). Not the agent group (a crew agent never spawns agents) and no language
/// servers (track C).
fn crew_configure(tools: marlowe_exec::BuiltinTools, home: &Path, workspace: Option<&Path>) -> marlowe_exec::BuiltinTools {
    let setup = marlowe_exec::specs::HostSetup { home, workspace };
    let tools = marlowe_exec::specs::verify::configure(tools, &setup);
    // `report_findings` stores under the profile home; no `workflow` for crew agents (track W).
    let tools = marlowe_exec::specs::workflow::crew_configure(tools, &setup);
    tools.with_state(marlowe_exec::git::GitState::new(Some(home.to_path_buf())).without_gh())
}

/// The built-ins for a crew sandbox. A `None` sandbox has no writable root: its directory is a
/// cwd only, and its tools are rooted at the workspace it reads (a read-only workflow agent's
/// relative paths are the workspace's), with every write asking.
fn crew_host(sandbox: &Sandbox, workspace: Option<&Path>) -> marlowe_exec::BuiltinTools {
    let tools = match sandbox.kind {
        SandboxKind::None => marlowe_exec::BuiltinTools::new(sandbox.origin.clone().or_else(|| workspace.map(Path::to_path_buf))),
        _ => marlowe_exec::BuiltinTools::new_sandbox(sandbox.root.clone()),
    };
    match workspace {
        Some(ws) => tools.with_read_root(ws.to_path_buf()),
        None => tools,
    }
}

/// Name and address form for the system prompt: `<home>\user.json` when it exists (written by
/// `SetUser`; it is the whole answer, so a cleared address stays cleared), else env, else none.
pub fn user_info(home: &Path) -> UserInfo {
    marlowe_daemon::user::load(home).unwrap_or_else(|| UserInfo {
        name: std::env::var("MARLOWE_USER_NAME").ok(),
        address: std::env::var("MARLOWE_USER_ADDRESS").ok(),
    })
}

pub fn daemon_deps(home: &Path) -> DaemonDeps {
    // First, so the memory service's GPU probe can read it.
    let sentinel = start_sentinel(home);
    // A scripted run never reads Credential Manager.
    let credentials: Arc<dyn CredentialStore> =
        if scripted() { Arc::new(MemoryStore::new()) } else { Arc::new(KeyringStore::new()) };
    DaemonDeps {
        providers: provider_factory(credentials.clone(), home),
        tools: tool_factory(home),
        user: user_info(home),
        mind: open_mind(home, sentinel.as_ref().map(|s| s.sentinel.clone())),
        sentinel,
        voice: start_voice(home),
        // The daemon starts the supervisor with its models, approvals, memory and GPU state; deep
        // research runs v2 over the keyless channels and the corpus fetcher.
        crew: Some(crew_setup(home)),
    }
}

/// The daemon `marlowe -p --standalone` runs inside its own process: the same providers, tools and
/// user, but no Sentinel, voice or crew (nothing there outlives a print run). Memory only when asked.
pub fn standalone_deps(home: &Path, memory: bool) -> DaemonDeps {
    let credentials: Arc<dyn CredentialStore> =
        if scripted() { Arc::new(MemoryStore::new()) } else { Arc::new(KeyringStore::new()) };
    DaemonDeps {
        providers: provider_factory(credentials.clone(), home),
        tools: tool_factory(home),
        user: user_info(home),
        mind: if memory { open_mind(home, None) } else { None },
        sentinel: None,
        voice: None,
        crew: None,
    }
}

/// The crew: tools, config, and deep research v2's I/O with `<home>\research.json` applied. A
/// scripted run has no network, so it keeps deep research on v1.
fn crew_setup(home: &Path) -> CrewSetup {
    let settings = crate::research_wire::ResearchSettings::load(home);
    let config = CrewConfig { research: settings.v2(), ..CrewConfig::default() };
    let research = (!scripted()).then(|| crate::research_wire::research_io(home, &settings));
    CrewSetup { tools: crew_tool_factory(home), config, research }
}

/// The watchers over the persisted roots. A Sentinel that fails to start (or panics doing so)
/// leaves the daemon without watching, said on stderr; it never stops the daemon.
pub fn start_sentinel(home: &Path) -> Option<SentinelLink> {
    let (tx, events) = crossbeam_channel::bounded(SENTINEL_QUEUE);
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| marlowe_sentinel::Sentinel::start(home, Vec::new(), tx))) {
        Ok(Ok(sentinel)) => Some(SentinelLink { sentinel, events }),
        Ok(Err(e)) => {
            eprintln!("marlowe: the Sentinel is not running: {e}");
            None
        }
        Err(_) => {
            eprintln!("marlowe: the Sentinel failed to start");
            None
        }
    }
}

/// Spoken voice, off until asked for: without `MARLOWE_VOICE=1` nothing voice-related starts (no
/// speech engine, no sidecar, no greeting, no spoken alerts) and the daemon runs without a voice.
/// Asked for, but without a speech backend, the voice reports itself unavailable; only a panic
/// leaves the daemon silent. A voice sidecar (the Chatterbox clone) never sees an API key, and
/// starts on the GPU only when its footprint fits in the memory free there, less headroom.
pub fn start_voice(home: &Path) -> Option<VoiceLink> {
    if !marlowe_voice::requested(&|k| std::env::var(k).ok()) {
        return None;
    }
    let (tx, events) = crossbeam_channel::bounded(VOICE_QUEUE);
    let gpu = marlowe_sentinel::vram::SentinelGpu::open();
    let vram = marlowe_voice::VramCheck(Arc::new(move |need_mib| {
        use marlowe_hw::GpuSource;
        let g = gpu.snapshot()?.gpu;
        Some(g.free_mib - g.headroom_mib.max(marlowe_hw::plan::HEADROOM_MIB) >= need_mib)
    }));
    let start = || marlowe_voice::Voice::start_gated(home, tx, marlowe_brains::scrubbed_env(), Some(vram));
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(start)) {
        Ok(voice) => Some(VoiceLink { voice, events }),
        Err(_) => {
            eprintln!("marlowe: voice failed to start");
            None
        }
    }
}

/// The memory service for `home`. A profile that will not open leaves the daemon without memory,
/// said on stderr (the daemon log) rather than silently.
pub fn open_mind(home: &Path, sentinel: Option<marlowe_sentinel::Sentinel>) -> Option<marlowe_mind::Mind> {
    let cfg = marlowe_mind::MindConfig { gpu: Some(gpu_probe(sentinel)), await_sink: true, ..Default::default() };
    match marlowe_mind::Mind::open_with(home, cfg) {
        Ok(mind) => {
            if let Ok(Some(reason)) = mind.stats().map(|s| s.degraded) {
                eprintln!("marlowe: memory is degraded: {reason}");
            }
            Some(mind)
        }
        Err(e) => {
            eprintln!("marlowe: memory is unavailable: {e}");
            None
        }
    }
}

/// Whether the cross-encoder may borrow the GPU, from the Sentinel: its cached per-device
/// utilisation and live watched runs, plus a fresh NVML read of compute-only processes (a
/// training job or an inference server). No Sentinel or no NVML device is "unknown": CPU.
pub fn gpu_probe(sentinel: Option<marlowe_sentinel::Sentinel>) -> marlowe_mind::GpuProbe {
    let nvml = std::sync::Mutex::new(None::<marlowe_sentinel::gpu::Gpu>);
    marlowe_mind::GpuProbe::new(move || {
        let Some(s) = &sentinel else {
            return marlowe_mind::GpuVerdict::Unknown("no Sentinel".into());
        };
        let state = s.state();
        let compute = {
            let mut g = nvml.lock().unwrap_or_else(|e| e.into_inner());
            g.get_or_insert_with(marlowe_sentinel::gpu::Gpu::open).compute_only_except(&marlowe_voice::owns_process)
        };
        gpu_verdict(&state, &compute)
    })
}

/// Per device index, other processes computing on it (`None`: the list could not be read), or
/// why NVML could not be asked at all.
pub type ComputeLists = Result<Vec<(u32, Option<Vec<String>>)>, String>;

/// The pure half of [`gpu_probe`]. Fails closed: NVML that could not be opened, or a device
/// whose compute processes could not be read, is `Unknown` (CPU), never "nothing running". A
/// training job's python.exe is invisible otherwise, and its quiet moments (data loading, eval,
/// checkpoints) read as idle.
pub fn gpu_verdict(state: &marlowe_proto::WatchState, compute: &ComputeLists) -> marlowe_mind::GpuVerdict {
    use marlowe_mind::GpuVerdict;
    let compute = match compute {
        Ok(c) => c,
        Err(why) => return GpuVerdict::Unknown(format!("NVML: {why}")),
    };
    let mut loads: Vec<marlowe_mind::GpuLoad> = Vec::new();
    for d in &state.gpu.devices {
        let Some(Some(names)) = compute.iter().find(|(i, _)| *i == d.index).map(|(_, n)| n.clone()) else {
            return GpuVerdict::Unknown(format!("the compute processes on {} could not be read", d.name));
        };
        loads.push(marlowe_mind::GpuLoad { name: d.name.clone(), util_pct: d.util_pct, compute_only: names });
    }
    let live = state.runs.iter().filter(|r| r.status.is_live()).count();
    marlowe_mind::judge_gpu(&loads, live)
}

#[cfg(test)]
mod tests {
    use super::{crew_configure, crew_host};

    /// Voice does not exist until M6: nothing starts (no engine, no sidecar, no greeting) unless
    /// MARLOWE_VOICE asks for it.
    #[test]
    fn voice_does_not_start_unless_asked_for() {
        if std::env::var_os(marlowe_voice::ENV_ENABLE).is_some() {
            return;
        }
        assert!(super::start_voice(&std::env::temp_dir()).is_none());
    }
    use marlowe_agent::ToolHost;
    use marlowe_crew::{Sandbox, SandboxKind};

    /// The production layout: the sandbox under `%LOCALAPPDATA%\MarloweCowork`. Nothing is created
    /// on disk; the paths are only resolved.
    #[test]
    fn a_sandbox_under_app_data_frees_its_writes_and_its_reads_do_not_taint() {
        let Some(app) = std::env::var_os("LOCALAPPDATA") else { return };
        let home = std::path::Path::new(&app).join("marlowe-wire-test-not-created");
        let root = home.join("worktrees").join("r1");
        assert!(marlowe_exec::paths::is_broad_root(&root), "app data is broad for an ordinary root");
        let sb = Sandbox {
            kind: SandboxKind::Worktree,
            root: root.clone(),
            origin: None,
            branch: Some("crew/r1".into()),
            base: None,
            repo_digest: None,
            frozen: Vec::new(),
            target_dir: None,
        };
        let host = crew_host(&sb, None);
        let call = |name: &str, path: &std::path::Path| marlowe_model::ToolCall {
            id: "c".into(),
            name: name.into(),
            args: serde_json::json!({"path": path.display().to_string(), "content": "x"}),
        };
        assert!(!host.taints(&call("read", &root.join("src").join("lib.rs"))), "a read in the sandbox");
        assert!(!host.taints(&call("grep", &root)), "a grep over the sandbox");
        assert_eq!(host.consequence(&call("write", &root.join("a.txt"))), marlowe_agent::Consequence::Free);
        assert!(host.taints(&call("read", &home.join("elsewhere.txt"))), "a read outside still taints");

        // The user's workspace (outside app data) is readable without taint too.
        let ws = std::env::temp_dir().join("marlowe-wire-test-ws");
        let host = crew_host(&sb, Some(&ws));
        assert!(!host.taints(&call("read", &ws.join("notes.md"))));
        assert!(matches!(host.consequence(&call("write", &ws.join("notes.md"))), marlowe_agent::Consequence::Ask { .. }));
    }

    /// A read-only workflow agent's host (a `None` sandbox over the workspace): relative paths are
    /// the workspace's, reads there do not taint, and a write asks.
    #[test]
    fn a_read_view_is_rooted_at_the_workspace_and_every_write_asks() {
        let ws = std::env::temp_dir().join("marlowe-wire-test-read-view");
        let scratch = std::env::temp_dir().join("marlowe-wire-test-read-view-scratch");
        let sb = Sandbox { kind: SandboxKind::None, root: scratch.clone(), origin: Some(ws.clone()), branch: None, base: None, repo_digest: None, frozen: Vec::new(), target_dir: None };
        let raw = crew_host(&sb, Some(&ws));
        let call = |name: &str, path: &str| marlowe_model::ToolCall {
            id: "c".into(),
            name: name.into(),
            args: serde_json::json!({"path": path, "content": "x"}),
        };
        assert!(!raw.taints(&call("read", "crates/lib.rs")), "a relative read is in the workspace");
        // As a crew step sees it: the sandbox decides, and nothing is writable in this one.
        let host = marlowe_crew::CrewTools::new(std::sync::Arc::new(raw), vec!["read".into(), "write".into()], sb.clone());
        assert!(matches!(host.consequence(&call("write", "notes.md")), marlowe_agent::Consequence::Ask { .. }));
        assert!(matches!(host.consequence(&call("write", &scratch.join("a.txt").display().to_string())), marlowe_agent::Consequence::Ask { .. }));
    }

    /// Track J's finding 7: a crew host offers `git` (its consequences are the crew's) and never
    /// `gh`; it never offers the agent group.
    #[test]
    fn a_crew_host_lists_git_and_not_gh_or_agents() {
        let root = std::env::temp_dir().join("marlowe-wire-test-crew-git");
        let sb = Sandbox { kind: SandboxKind::Scratch, root: root.clone(), origin: None, branch: None, base: None, repo_digest: None, frozen: Vec::new(), target_dir: None };
        let home = std::env::temp_dir().join("marlowe-wire-test-crew-home");
        let host = crew_configure(crew_host(&sb, None), &home, None);
        let names: Vec<String> = host.specs().into_iter().map(|s| s.name).collect();
        assert!(names.iter().any(|n| n == "git"), "{names:?}");
        assert!(!names.iter().any(|n| n == "gh" || n == "agent" || n == "todo"), "{names:?}");
    }

    #[test]
    fn the_gpu_verdict_reads_util_compute_processes_and_live_runs() {
        use marlowe_mind::GpuVerdict;
        let mut st = marlowe_proto::WatchState::default();
        assert!(matches!(super::gpu_verdict(&st, &Ok(vec![])), GpuVerdict::Unknown(_)), "no device is unknown");
        let dev = serde_json::json!({"index": 0, "name": "RTX", "util_pct": 3, "mem_used_mb": 2000, "mem_total_mb": 16000});
        st.gpu.devices.push(serde_json::from_value(dev).unwrap());
        assert_eq!(super::gpu_verdict(&st, &Ok(vec![(0, Some(vec![]))])), GpuVerdict::Idle);
        assert!(matches!(super::gpu_verdict(&st, &Ok(vec![(0, Some(vec!["python.exe".into()]))])), GpuVerdict::Busy(_)));
        // Found in review: could not look is not nothing running. A quiet training job (3% util)
        // with an unreadable compute list, a device missing from the list, or no NVML: unknown.
        assert!(matches!(super::gpu_verdict(&st, &Ok(vec![(0, None)])), GpuVerdict::Unknown(_)));
        assert!(matches!(super::gpu_verdict(&st, &Ok(vec![])), GpuVerdict::Unknown(_)));
        assert!(matches!(super::gpu_verdict(&st, &Err("nvml.dll not found".into())), GpuVerdict::Unknown(_)));
        st.gpu.devices[0].util_pct = 37;
        assert!(matches!(super::gpu_verdict(&st, &Ok(vec![(0, Some(vec![]))])), GpuVerdict::Busy(_)));
    }
}
