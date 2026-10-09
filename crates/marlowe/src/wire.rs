//! The only place concrete provider and tool crates are named.

use std::path::Path;
use std::sync::Arc;

use marlowe_agent::{ToolHost, UserInfo};
use marlowe_brains::{api_key, CredentialStore, KeyringStore, ProviderConfig, ProviderFactory};
use marlowe_daemon::{DaemonDeps, SentinelLink, ToolHostFactory, VoiceLink};
use marlowe_exec::{SearchBackend, SearchResponse, SearchResult};
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
    let p = marlowe_provider::OllamaProvider::from_base_url(cfg.base_url.as_deref())?;
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
    if let Ok(url) = std::env::var("MARLOWE_OLLAMA_ADDR") {
        f.set_base_url(ProviderKind::Ollama, url);
    }
    f
}

/// Web search through OpenRouter's server tool; errors when no key exists.
fn web_search(
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

/// Tool host factory: builtin tools rooted at the session workspace. With no workspace every
/// write asks.
pub fn tool_factory(credentials: Arc<dyn CredentialStore>) -> ToolHostFactory {
    Arc::new(move |workspace: Option<&Path>| {
        let creds = credentials.clone();
        let search: Arc<dyn SearchBackend> =
            Arc::new(move |q: &str, cancel: &CancelToken| web_search(creds.as_ref(), q, cancel));
        let tools = marlowe_exec::BuiltinTools::new(workspace.map(Path::to_path_buf)).with_search(search);
        Arc::new(tools) as Arc<dyn ToolHost>
    })
}

/// Name and address form for the system prompt: `<home>\user.json`, else env, else none.
pub fn user_info(home: &Path) -> UserInfo {
    #[derive(serde::Deserialize, Default)]
    struct File {
        name: Option<String>,
        address: Option<String>,
    }
    let file: File = std::fs::read_to_string(home.join("user.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    UserInfo {
        name: std::env::var("MARLOWE_USER_NAME").ok().or(file.name),
        address: std::env::var("MARLOWE_USER_ADDRESS").ok().or(file.address),
    }
}

pub fn daemon_deps(home: &Path) -> DaemonDeps {
    let credentials: Arc<dyn CredentialStore> = Arc::new(KeyringStore::new());
    DaemonDeps {
        providers: provider_factory(credentials.clone()),
        tools: tool_factory(credentials),
        user: user_info(home),
        mind: open_mind(home),
        sentinel: start_sentinel(home),
        voice: start_voice(home),
    }
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
pub fn open_mind(home: &Path) -> Option<marlowe_mind::Mind> {
    match marlowe_mind::Mind::open(home) {
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

#[cfg(test)]
mod tests {
    use super::cited_links;

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
