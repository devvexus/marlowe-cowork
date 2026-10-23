//! What a model's own chat template does with the model's earlier thinking.
//!
//! A reasoning model's reply goes back to it with its thinking (`reasoning_content`), and the
//! template decides what the model reads next: some print every earlier turn's thinking, some have
//! a switch (`preserve_thinking`) that does, and some print it only for the turn in progress and
//! drop it the moment a user message follows, with no switch. Which one a model has is asked of
//! the template itself: a probe conversation is rendered through llama-server's `/apply-template`
//! and the earlier thinking is looked for in the result. Nothing here knows a model's name, and
//! nothing rewrites a template.
//!
//! The answer is kept per model file (the blob's digest), in memory and in a small JSON file, so
//! a model is asked once.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{json, Map, Value};

use crate::http::{self, LocalEndpoint};

const TIMEOUT: Duration = Duration::from_secs(10);
const EARLY: &str = "RSN-ALPHA";
const LATE: &str = "RSN-BETA";

/// What the template does with thinking from earlier turns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Thinking {
    /// Printed by default: nothing to set.
    Keeps,
    /// Dropped unless these `chat_template_kwargs` are set (`preserve_thinking: true`, ...).
    Switch(Vec<(String, bool)>),
    /// Printed only for the turn in progress: a user message after it drops it, and there is no
    /// switch. The model's own rule, followed as it is; the cost is taken off the user's path by a
    /// pre-read ([`super::warm`]).
    Drops,
    /// Never printed, even for the turn in progress: a template without thinking.
    Ignores,
    /// The template could not be asked (the server has no `/apply-template`, or it refused).
    Unknown,
}

impl Thinking {
    /// The `chat_template_kwargs` that keep earlier thinking, beyond `enable_thinking`. A template
    /// that could not be asked gets the usual switches: one without them ignores unknown names.
    pub fn kwargs(&self) -> Vec<(String, Value)> {
        match self {
            Thinking::Switch(k) => k.iter().map(|(n, v)| (n.clone(), json!(v))).collect(),
            Thinking::Unknown => vec![("preserve_thinking".into(), json!(true)), ("clear_thinking".into(), json!(false))],
            _ => Vec::new(),
        }
    }

    /// Whether a new user message rewrites how earlier turns print (thinking is dropped from
    /// them), which the next request pays for with a re-read unless it is pre-read.
    pub fn rewrites_history(&self) -> bool {
        matches!(self, Thinking::Drops)
    }

    /// One line for the engine's notes.
    pub fn describe(&self) -> String {
        match self {
            Thinking::Keeps => "its template keeps earlier thinking".into(),
            Thinking::Switch(k) => format!(
                "its template keeps earlier thinking when {} {}",
                k.iter().map(|(n, v)| format!("{n}={v}")).collect::<Vec<_>>().join(", "),
                if k.len() == 1 { "is set" } else { "are set" }
            ),
            Thinking::Drops => "its template drops earlier thinking at each new user message and has no switch; the turn that just ended is read ahead in the background".into(),
            Thinking::Ignores => "its template prints no thinking".into(),
            Thinking::Unknown => "its template could not be asked about thinking".into(),
        }
    }

    fn to_json(&self) -> Value {
        match self {
            Thinking::Keeps => json!({"kind": "keeps"}),
            Thinking::Switch(k) => json!({"kind": "switch", "kwargs": k.iter().map(|(n, v)| (n.clone(), json!(v))).collect::<Map<_, _>>()}),
            Thinking::Drops => json!({"kind": "drops"}),
            Thinking::Ignores => json!({"kind": "ignores"}),
            Thinking::Unknown => json!({"kind": "unknown"}),
        }
    }

    fn from_json(v: &Value) -> Option<Self> {
        Some(match v.get("kind")?.as_str()? {
            "keeps" => Thinking::Keeps,
            "drops" => Thinking::Drops,
            "ignores" => Thinking::Ignores,
            "switch" => Thinking::Switch(v.get("kwargs")?.as_object()?.iter().filter_map(|(n, b)| Some((n.clone(), b.as_bool()?))).collect()),
            _ => return None,
        })
    }
}

fn probe_messages() -> Vec<Value> {
    vec![
        json!({"role": "system", "content": "You are a test."}),
        json!({"role": "user", "content": "U1 please read a"}),
        json!({"role": "assistant", "content": "", "reasoning_content": EARLY,
               "tool_calls": [{"id": "c1", "index": 0, "type": "function", "function": {"name": "read", "arguments": "{\"path\":\"a\"}"}}]}),
        json!({"role": "tool", "tool_call_id": "c1", "name": "read", "content": "T1-RESULT"}),
        json!({"role": "assistant", "content": "C-BETA", "reasoning_content": LATE}),
        json!({"role": "user", "content": "U2 thanks"}),
    ]
}

fn probe_tools() -> Value {
    json!([{"type": "function", "function": {"name": "read", "description": "read a file",
        "parameters": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}}}])
}

/// The switches templates are known to offer, tried in this order; a template that has none of
/// them ignores them all.
const SWITCHES: &[&[(&str, bool)]] = &[&[("preserve_thinking", true)], &[("clear_thinking", false)], &[("preserve_thinking", true), ("clear_thinking", false)]];

fn render(endpoint: &LocalEndpoint, messages: &[Value], extra: &[(&str, bool)]) -> Option<String> {
    let mut kwargs = Map::new();
    kwargs.insert("enable_thinking".into(), json!(true));
    for (n, v) in extra {
        kwargs.insert((*n).into(), json!(v));
    }
    let body = json!({"messages": messages, "tools": probe_tools(), "chat_template_kwargs": kwargs});
    let v = http::post_json(endpoint, "/apply-template", &body, TIMEOUT).ok()?;
    v.get("prompt").and_then(Value::as_str).map(str::to_string)
}

/// Asks the template of the model `endpoint` serves what it does with earlier thinking.
pub fn probe(endpoint: &LocalEndpoint) -> Thinking {
    let messages = probe_messages();
    let Some(plain) = render(endpoint, &messages, &[]) else { return Thinking::Unknown };
    if plain.contains(EARLY) {
        return Thinking::Keeps;
    }
    for switch in SWITCHES {
        if render(endpoint, &messages, switch).is_some_and(|p| p.contains(EARLY)) {
            return Thinking::Switch(switch.iter().map(|(n, v)| ((*n).to_string(), *v)).collect());
        }
    }
    // Thinking that is printed while it is the turn in progress and gone once a user message
    // follows is dropped; thinking that is never printed is simply not part of the template.
    match render(endpoint, &messages[..5], &[]) {
        Some(p) if p.contains(EARLY) || p.contains(LATE) => Thinking::Drops,
        Some(_) => Thinking::Ignores,
        None => Thinking::Unknown,
    }
}

/// The model file's identity: the blob's file name (`sha256-<digest>` in Ollama's store).
pub fn digest_of(blob: &Path) -> String {
    let name = blob.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    name.strip_prefix("sha256-").unwrap_or(&name).to_string()
}

/// The answers, per model file.
pub struct ThinkingCache {
    file: Option<PathBuf>,
    map: Mutex<Option<HashMap<String, Thinking>>>,
}

impl ThinkingCache {
    pub fn new(file: Option<PathBuf>) -> Self {
        Self { file, map: Mutex::new(None) }
    }

    fn load(&self) -> HashMap<String, Thinking> {
        let text = self.file.as_deref().and_then(|p| std::fs::read_to_string(p).ok());
        let v: Value = text.and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(Value::Null);
        v.as_object().into_iter().flatten().filter_map(|(k, v)| Some((k.clone(), Thinking::from_json(v)?))).collect()
    }

    pub fn get(&self, digest: &str) -> Option<Thinking> {
        let mut g = self.map.lock().unwrap_or_else(|e| e.into_inner());
        g.get_or_insert_with(|| self.load()).get(digest).cloned()
    }

    /// Remembers the answer (never an `Unknown`: that is asked again at the next load).
    pub fn put(&self, digest: &str, t: &Thinking) {
        if *t == Thinking::Unknown {
            return;
        }
        let mut g = self.map.lock().unwrap_or_else(|e| e.into_inner());
        let map = g.get_or_insert_with(|| self.load());
        map.insert(digest.to_string(), t.clone());
        if let Some(p) = &self.file {
            let v: Map<String, Value> = map.iter().map(|(k, t)| (k.clone(), t.to_json())).collect();
            if let Some(dir) = p.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(p, Value::Object(v).to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llama::testkit::{chatml_with, Handler, Http, Reply, Tpl};
    use std::sync::Arc;

    fn server(tpl: Option<Tpl>) -> (Http, LocalEndpoint) {
        let handler: Arc<Handler> = Arc::new(move |_, path, body| match (path, tpl) {
            ("/apply-template", Some(t)) => Reply::json(json!({"prompt": chatml_with(body, t)})),
            _ => Reply::status(404, json!({})),
        });
        let http = Http::start(handler);
        let ep = http.endpoint();
        (http, ep)
    }

    fn asked(tpl: Option<Tpl>) -> Thinking {
        let (_http, ep) = server(tpl);
        probe(&ep)
    }

    #[test]
    fn each_kind_of_template_is_told_apart_by_what_it_prints() {
        assert_eq!(asked(Some(Tpl::Keeps)), Thinking::Keeps);
        assert_eq!(asked(Some(Tpl::Switch)), Thinking::Switch(vec![("preserve_thinking".into(), true)]));
        assert_eq!(asked(Some(Tpl::Drops)), Thinking::Drops);
        assert_eq!(asked(Some(Tpl::Ignores)), Thinking::Ignores);
        assert_eq!(asked(None), Thinking::Unknown, "a server with no /apply-template cannot be asked");
    }

    #[test]
    fn what_is_sent_follows_the_answer() {
        assert!(Thinking::Keeps.kwargs().is_empty() && Thinking::Drops.kwargs().is_empty() && Thinking::Ignores.kwargs().is_empty());
        assert_eq!(Thinking::Switch(vec![("preserve_thinking".into(), true)]).kwargs(), vec![("preserve_thinking".to_string(), json!(true))]);
        let unknown = Thinking::Unknown.kwargs();
        assert_eq!(unknown.len(), 2, "an unasked template gets the usual switches, which one without them ignores");
        assert!(Thinking::Drops.rewrites_history() && !Thinking::Keeps.rewrites_history() && !Thinking::Ignores.rewrites_history());
    }

    #[test]
    fn the_answer_is_kept_per_model_file_and_survives_a_restart() {
        let dir = crate::llama::testkit::TempDir::new();
        let file = dir.path().join("sub").join("thinking.json");
        let cache = ThinkingCache::new(Some(file.clone()));
        assert_eq!(cache.get("abc"), None);
        cache.put("abc", &Thinking::Drops);
        cache.put("def", &Thinking::Switch(vec![("preserve_thinking".into(), true)]));
        cache.put("ghi", &Thinking::Unknown);
        let again = ThinkingCache::new(Some(file));
        assert_eq!(again.get("abc"), Some(Thinking::Drops));
        assert_eq!(again.get("def"), Some(Thinking::Switch(vec![("preserve_thinking".into(), true)])));
        assert_eq!(again.get("ghi"), None, "an unanswered probe is asked again");
        assert_eq!(digest_of(Path::new("C:/x/blobs/sha256-0123abcd")), "0123abcd");
    }
}
