//! The only place concrete provider and tool crates are named.

use std::path::Path;
use std::sync::Arc;

use marlowe_agent::{ToolHost, UserInfo};
use marlowe_brains::{api_key, CredentialStore, KeyringStore, MemoryStore, ProviderConfig, ProviderFactory};
use marlowe_crew::{CrewConfig, CrewSetup, CrewToolFactory, Sandbox, SandboxKind};
use marlowe_daemon::{DaemonDeps, SentinelLink, ToolHostFactory, VoiceLink};
use marlowe_exec::{Leftovers, ResearchConfig, SearchBackend, SearchResponse, SearchResult};
use marlowe_model::{
    CancelToken, ChatRequest, Effort, Message, Provider, ProviderError, ProviderKind, ToolSpec,
};

/// Sentinel events queued for the daemon's forwarder; a full queue drops, never blocks a watcher.
const SENTINEL_QUEUE: usize = 1024;
/// Voice events (envelope chunks at 30 Hz) queued for the forwarder.
const VOICE_QUEUE: usize = 256;

/// Small model that runs the OpenRouter server-side search tool.
const SEARCH_MODEL_DEFAULT: &str = "google/gemini-2.5-flash";
const SEARCH_MODEL_ENV: &str = "MARLOWE_SEARCH_MODEL";
const SEARCH_TOOL: &str = "openrouter:web_search";

fn make_ollama(cfg: &ProviderConfig) -> Result<Arc<dyn Provider>, ProviderError> {
    // Every request for a model sends that model's context policy window (one allocation per
    // model), so Ollama never reloads a runner because two agents asked for different sizes.
    let p = marlowe_provider::OllamaProvider::from_base_url(cfg.base_url.as_deref())?.with_allocations(cfg.context.clone());
    Ok(Arc::new(p))
}

/// Never fails: the catalog works without a key; `stream` then returns an Auth error.
fn make_openrouter(cfg: &ProviderConfig) -> Result<Arc<dyn Provider>, ProviderError> {
    Ok(Arc::new(marlowe_openrouter::OpenRouterProvider::from_key(cfg.api_key.as_deref())))
}

/// Provider factory with Ollama and OpenRouter registered, keys from Credential Manager.
pub fn provider_factory(credentials: Arc<dyn CredentialStore>) -> ProviderFactory {
    let mut f = ProviderFactory::new(credentials);
    f.register(ProviderKind::Ollama, make_ollama);
    f.register(ProviderKind::OpenRouter, make_openrouter);
    let ollama_url = std::env::var("MARLOWE_OLLAMA_ADDR").ok();
    if let Some(url) = &ollama_url {
        f.set_base_url(ProviderKind::Ollama, url.clone());
    }
    // The context planner reads Ollama's store, its runner log and the GPU; it changes nothing
    // but the server settings, and only after an approval.
    let endpoint = ollama_url
        .as_deref()
        .and_then(marlowe_provider::LocalEndpoint::parse)
        .unwrap_or_else(marlowe_provider::LocalEndpoint::default_ollama);
    f.set_hardware(marlowe_brains::Hardware {
        models: Arc::new(marlowe_provider::OllamaHw::new(endpoint.clone())),
        gpu: Arc::new(marlowe_sentinel::vram::SentinelGpu::open()),
        server: Arc::new(
            marlowe_provider::OllamaServerControl::new(endpoint).with_scrub(marlowe_brains::scrubbed_env().iter().copied()),
        ),
    });
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

/// Web search through OpenRouter's server tool; errors when no key exists.
pub(crate) fn web_search(
    credentials: &dyn CredentialStore,
    query: &str,
    cancel: &CancelToken,
) -> Result<SearchResponse, String> {
    let key = api_key(ProviderKind::OpenRouter, credentials)
        .ok_or("web_search needs an OpenRouter key (OPENROUTER_API_KEY or the Library)")?;
    let provider = marlowe_openrouter::OpenRouterProvider::from_key(Some(&key));
    let model = std::env::var(SEARCH_MODEL_ENV).unwrap_or_else(|_| SEARCH_MODEL_DEFAULT.to_string());
    let mut req = ChatRequest::new(model, vec![Message::user(query)]);
    req.effort = Effort::Off;
    req.max_output_tokens = Some(1500);
    req.system = vec![marlowe_model::SystemBlock {
        text: "Search the web for the user's query. Answer briefly and list each source as a \
               markdown link with its URL."
            .into(),
        cache: false,
    }];
    req.tools = vec![ToolSpec {
        name: SEARCH_TOOL.into(),
        description: "Server-side web search".into(),
        input_schema: serde_json::json!({ "type": "object", "properties": {} }),
    }];
    let done = provider.stream(&req, cancel, &mut |_| {}).map_err(|e| e.to_string())?;
    Ok(SearchResponse { results: cited_links(&done.text), answer: done.text })
}

/// Markdown links `[title](https://..)` in order, deduplicated by URL.
fn cited_links(text: &str) -> Vec<SearchResult> {
    let mut out: Vec<SearchResult> = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        let after = &rest[open + 1..];
        let Some(close) = after.find("](") else { break };
        let title = &after[..close];
        let tail = &after[close + 2..];
        let Some(end) = tail.find(')') else { break };
        let url = tail[..end].trim();
        if (url.starts_with("https://") || url.starts_with("http://"))
            && !title.contains('[')
            && !out.iter().any(|r| r.url == url)
        {
            out.push(SearchResult { title: title.trim().to_string(), url: url.to_string(), snippet: String::new() });
        }
        rest = &tail[end..];
    }
    out
}

fn search_backend(credentials: &Arc<dyn CredentialStore>) -> Arc<dyn SearchBackend> {
    if scripted() {
        return Arc::new(|_: &str, _: &CancelToken| Err::<SearchResponse, String>("web_search is off under the scripted provider".into()));
    }
    let creds = credentials.clone();
    Arc::new(move |q: &str, cancel: &CancelToken| web_search(creds.as_ref(), q, cancel))
}

/// arXiv and paper settings: the on-disk cache under `<home>\cache`, the global rate limit.
fn research(home: &Path) -> ResearchConfig {
    ResearchConfig::default().with_cache_dir(home.join("cache"))
}

/// Tool host factory: builtin tools rooted at the session workspace. With no workspace every
/// write asks.
pub fn tool_factory(credentials: Arc<dyn CredentialStore>, home: &Path) -> ToolHostFactory {
    let home = home.to_path_buf();
    Arc::new(move |workspace: Option<&Path>| {
        let tools = marlowe_exec::BuiltinTools::new(workspace.map(Path::to_path_buf))
            .with_search(search_backend(&credentials))
            .with_research(research(&home));
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
pub fn crew_tool_factory(credentials: Arc<dyn CredentialStore>, home: &Path) -> CrewToolFactory {
    let home = home.to_path_buf();
    Arc::new(move |sandbox: &Sandbox, workspace: Option<&Path>| {
        let tools = crew_host(sandbox, workspace)
            .with_search(search_backend(&credentials))
            .with_research(research(&home))
            .with_child_env(sandbox.child_env())
            .with_shell_dir(sandbox.root.clone())
            .with_leftovers(Leftovers::KillWithHost);
        Arc::new(tools) as Arc<dyn ToolHost>
    })
}

/// The built-ins for a crew sandbox. A `None` sandbox has no writable root: its directory is a
/// cwd only.
fn crew_host(sandbox: &Sandbox, workspace: Option<&Path>) -> marlowe_exec::BuiltinTools {
    let tools = match sandbox.kind {
        SandboxKind::None => marlowe_exec::BuiltinTools::new(None),
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
        providers: provider_factory(credentials.clone()),
        tools: tool_factory(credentials.clone(), home),
        user: user_info(home),
        mind: open_mind(home, sentinel.as_ref().map(|s| s.sentinel.clone())),
        sentinel,
        voice: start_voice(home),
        // The daemon starts the supervisor with its models, approvals, memory and GPU state; deep
        // research runs v2 over the keyless channels and the corpus fetcher.
        crew: Some(crew_setup(credentials, home)),
    }
}

/// The crew: tools, config, and deep research v2's I/O with `<home>\research.json` applied. A
/// scripted run has no network, so it keeps deep research on v1.
fn crew_setup(credentials: Arc<dyn CredentialStore>, home: &Path) -> CrewSetup {
    let settings = crate::research_wire::ResearchSettings::load(home);
    let config = CrewConfig { research: settings.v2(), ..CrewConfig::default() };
    let research = (!scripted()).then(|| crate::research_wire::research_io(home, &settings, credentials.clone()));
    CrewSetup { tools: crew_tool_factory(credentials, home), config, research }
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

/// Spoken voice. Without a speech backend the voice reports itself unavailable; only a panic
/// leaves the daemon silent.
pub fn start_voice(home: &Path) -> Option<VoiceLink> {
    let (tx, events) = crossbeam_channel::bounded(VOICE_QUEUE);
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| marlowe_voice::Voice::start(home, tx))) {
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
    let cfg = marlowe_mind::MindConfig { gpu: Some(gpu_probe(sentinel)), ..Default::default() };
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
            g.get_or_insert_with(marlowe_sentinel::gpu::Gpu::open).compute_only()
        };
        gpu_verdict(&state, &compute)
    })
}

/// The pure half of [`gpu_probe`].
pub fn gpu_verdict(state: &marlowe_proto::WatchState, compute: &[(u32, Vec<String>)]) -> marlowe_mind::GpuVerdict {
    let loads: Vec<marlowe_mind::GpuLoad> = state
        .gpu
        .devices
        .iter()
        .map(|d| marlowe_mind::GpuLoad {
            name: d.name.clone(),
            util_pct: d.util_pct,
            compute_only: compute.iter().find(|(i, _)| *i == d.index).map(|(_, n)| n.clone()).unwrap_or_default(),
        })
        .collect();
    let live = state.runs.iter().filter(|r| r.status.is_live()).count();
    marlowe_mind::judge_gpu(&loads, live)
}

#[cfg(test)]
mod tests {
    use super::{cited_links, crew_host};
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

    #[test]
    fn the_gpu_verdict_reads_util_compute_processes_and_live_runs() {
        use marlowe_mind::GpuVerdict;
        let mut st = marlowe_proto::WatchState::default();
        assert!(matches!(super::gpu_verdict(&st, &[]), GpuVerdict::Unknown(_)), "no device is unknown");
        let dev = serde_json::json!({"index": 0, "name": "RTX", "util_pct": 3, "mem_used_mb": 2000, "mem_total_mb": 16000});
        st.gpu.devices.push(serde_json::from_value(dev).unwrap());
        assert_eq!(super::gpu_verdict(&st, &[(0, vec![])]), GpuVerdict::Idle);
        assert!(matches!(super::gpu_verdict(&st, &[(0, vec!["python.exe".into()])]), GpuVerdict::Busy(_)));
        st.gpu.devices[0].util_pct = 37;
        assert!(matches!(super::gpu_verdict(&st, &[(0, vec![])]), GpuVerdict::Busy(_)));
    }

    #[test]
    fn cited_links_are_parsed_in_order_and_deduplicated() {
        let text = "See [Rust](https://rust-lang.org) and [docs](https://doc.rust-lang.org/).\n\
                    Again [Rust](https://rust-lang.org); not [this](ftp://x) or [broken](https://y";
        let links = cited_links(text);
        let urls: Vec<_> = links.iter().map(|l| l.url.as_str()).collect();
        assert_eq!(urls, ["https://rust-lang.org", "https://doc.rust-lang.org/"]);
        assert_eq!(links[1].title, "docs");
    }
}
