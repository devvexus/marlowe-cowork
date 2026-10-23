//! Prefix watch: does a request extend the one before it?
//!
//! A local model reuses its cache only for an exact prefix (a hybrid model, qwen35's recurrent
//! layers, for nothing else). Every request must therefore start with the previous one for the
//! same session and model: same system blocks, same tools in the same order, same messages, then
//! more. [`Fingerprint`] keeps a request's shape cheaply; [`diverges`] names the first section
//! that differs, so a break is found by its cause and not by a slow first token.
//!
//! One Ollama slot serves one request at a time per model (qwen35 allows no parallel requests), so
//! any other request on the model in between replaces the cache too: [`note_request`] counts them.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Mutex, OnceLock};

use crate::{ChatRequest, Message, Part, Role};

/// The shape of one request: a hash per section, enough to find the first that changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fingerprint {
    system: Vec<(u64, String)>,
    tools: Vec<(String, u64)>,
    /// One entry per message: its role and a hash per part.
    messages: Vec<(Role, Vec<u64>)>,
    /// Text of each message part's head, to say what changed (only the first 80 characters).
    heads: Vec<Vec<String>>,
}

fn hash_of(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

fn head(s: &str) -> String {
    let one: String = s.chars().take(80).collect();
    one.replace('\n', "\\n")
}

fn part_text(p: &Part) -> String {
    serde_json::to_string(p).unwrap_or_default()
}

fn message_parts(m: &Message) -> Vec<String> {
    m.content.iter().map(part_text).collect()
}

impl Fingerprint {
    pub fn of(req: &ChatRequest) -> Self {
        let system = req.system.iter().map(|b| (hash_of(&b.text), head(&b.text))).collect();
        let tools = req
            .tools
            .iter()
            .map(|t| (t.name.clone(), hash_of(&format!("{}\u{0}{}\u{0}{}", t.name, t.description, t.input_schema))))
            .collect();
        let mut messages = Vec::with_capacity(req.messages.len());
        let mut heads = Vec::with_capacity(req.messages.len());
        for m in &req.messages {
            let parts = message_parts(m);
            messages.push((m.role, parts.iter().map(|p| hash_of(p)).collect()));
            heads.push(parts.iter().map(|p| head(p)).collect());
        }
        Self { system, tools, messages, heads }
    }

    pub fn message_count(&self) -> usize {
        self.messages.len()
    }
}

/// The first place `next` stops extending `prev`, in words; `None` when it extends it.
pub fn diverges(prev: &Fingerprint, next: &Fingerprint) -> Option<String> {
    for i in 0..prev.system.len().max(next.system.len()) {
        match (prev.system.get(i), next.system.get(i)) {
            (Some(a), Some(b)) if a.0 == b.0 => {}
            (Some(a), Some(b)) => {
                return Some(format!("system block {i} changed: was \"{}\", now \"{}\"", a.1, b.1));
            }
            (Some(a), None) => return Some(format!("system block {i} removed (\"{}\")", a.1)),
            (None, Some(b)) => return Some(format!("system block {i} added (\"{}\")", b.1)),
            (None, None) => {}
        }
    }
    for i in 0..prev.tools.len().max(next.tools.len()) {
        match (prev.tools.get(i), next.tools.get(i)) {
            (Some(a), Some(b)) if a == b => {}
            (Some(a), Some(b)) if a.0 == b.0 => return Some(format!("tool {i} ({}) changed its description or schema", a.0)),
            (Some(a), Some(b)) => return Some(format!("tool list differs at {i}: was {}, now {}", a.0, b.0)),
            (Some(a), None) => return Some(format!("tool {} removed", a.0)),
            (None, Some(b)) => return Some(format!("tool {} added", b.0)),
            (None, None) => {}
        }
    }
    if next.messages.len() < prev.messages.len() {
        return Some(format!("history is shorter: {} messages, was {}", next.messages.len(), prev.messages.len()));
    }
    for (i, (a, b)) in prev.messages.iter().zip(&next.messages).enumerate() {
        if a == b {
            continue;
        }
        let who = format!("message {i} ({:?})", a.0).to_lowercase();
        if a.0 != b.0 {
            return Some(format!("{who} became {:?}", b.0));
        }
        for j in 0..a.1.len().max(b.1.len()) {
            if a.1.get(j) != b.1.get(j) {
                let was = prev.heads[i].get(j).map(String::as_str).unwrap_or("(none)");
                let now = next.heads[i].get(j).map(String::as_str).unwrap_or("(none)");
                return Some(format!("{who} part {j} changed: was \"{was}\", now \"{now}\""));
            }
        }
    }
    None
}

/// A hash of what identifies a conversation: its system prompt and its first message.
pub fn conversation_key(req: &ChatRequest) -> u64 {
    let mut h = DefaultHasher::new();
    req.system_text().hash(&mut h);
    if let Some(m) = req.messages.first() {
        serde_json::to_string(m).unwrap_or_default().hash(&mut h);
    }
    h.finish()
}

fn render_breaks() -> &'static Mutex<HashMap<(String, u64), Vec<String>>> {
    static BREAKS: OnceLock<Mutex<HashMap<(String, u64), Vec<String>>>> = OnceLock::new();
    BREAKS.get_or_init(Default::default)
}

/// The engine's own chat template rendered a request that does not start with the previous
/// prompt plus the model's reply (the provider found it: it renders both through llama-server's
/// `/apply-template`). `detail` names the first differing byte and the message.
pub fn report_render_break(model: &str, key: u64, detail: String) {
    let mut map = render_breaks().lock().unwrap_or_else(|p| p.into_inner());
    if map.len() > 256 {
        map.clear();
    }
    map.entry((model.to_string(), key)).or_default().push(detail);
}

/// Starts a render break the model's own chat template caused: the request carried the earlier
/// reasoning and the template printed less of it than before (Qwen's prints only the newest
/// turn's reasoning, with no switch to keep more). Not Marlowe's to fix and not a violation: it
/// costs a re-read of the turn that just ended, once per user message.
pub const TEMPLATE_DROP: &str = "template-drop: ";

/// The render breaks reported for this conversation since the last call.
pub fn take_render_breaks(model: &str, key: u64) -> Vec<String> {
    render_breaks().lock().unwrap_or_else(|p| p.into_inner()).remove(&(model.to_string(), key)).unwrap_or_default()
}

fn violations_log() -> &'static Mutex<Vec<String>> {
    static V: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
    V.get_or_init(Default::default)
}

/// A prefix break outside compaction is a bug: it is kept here (as well as logged) so a test can
/// fail on it. Bounded; the oldest are dropped.
pub fn record_violation(line: String) {
    let mut v = violations_log().lock().unwrap_or_else(|p| p.into_inner());
    if v.len() >= 256 {
        v.remove(0);
    }
    v.push(line);
}

/// Every violation recorded in this process, oldest first.
pub fn violations() -> Vec<String> {
    violations_log().lock().unwrap_or_else(|p| p.into_inner()).clone()
}

/// Forgets the recorded violations (a test starts clean).
pub fn clear_violations() {
    violations_log().lock().unwrap_or_else(|p| p.into_inner()).clear();
}

/// Panics, naming each one, when a violation was recorded: the end of a test that must not break
/// the cache.
pub fn assert_no_violations() {
    let v = violations();
    assert!(v.is_empty(), "the prompt cache was broken {} time(s) outside compaction:\n{}", v.len(), v.join("\n"));
}

/// What the watch remembers per session and model.
struct Seen {
    print: Fingerprint,
    /// What the conversation began with ([`anchor`]); another start is another conversation.
    anchor: u64,
    /// [`slot_seq`] once the request had been made.
    slot: u64,
}

fn seen() -> &'static Mutex<HashMap<(String, String), Seen>> {
    static SEEN: OnceLock<Mutex<HashMap<(String, String), Seen>>> = OnceLock::new();
    SEEN.get_or_init(Default::default)
}

fn slots() -> &'static Mutex<HashMap<String, u64>> {
    static SLOTS: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();
    SLOTS.get_or_init(Default::default)
}

/// Counts a request made to `model`'s engine (the provider calls it as the request leaves).
pub fn note_request(model: &str) {
    *slots().lock().unwrap_or_else(|p| p.into_inner()).entry(model.to_string()).or_insert(0) += 1;
}

fn slot_seq(model: &str) -> u64 {
    slots().lock().unwrap_or_else(|p| p.into_inner()).get(model).copied().unwrap_or(0)
}

/// The cause given when the prompt extends the last one but another request used the slot.
const SLOT_ONLY: &str = "the prompt itself extends the last one";

/// A break in a session's prefix, as the daemon log words it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Broken {
    pub cause: String,
    /// Other requests reached the model's engine since this session's last one.
    pub others: u64,
}

impl Broken {
    /// True when the request itself no longer starts with the last one (not merely a slot shared).
    pub fn diverged(&self) -> bool {
        self.cause != SLOT_ONLY
    }

    pub fn line(&self, session: &str, model: &str) -> String {
        let slot = if self.others > 0 {
            format!("; {} other request(s) used the {model} slot since (one slot per model: its cache was replaced)", self.others)
        } else {
            String::new()
        };
        format!("marlowe: prompt cache break in session {session} on {model}: {}{slot}", self.cause)
    }
}

/// The words the conversation began with: the last text of its first user message (a tail or note
/// rides ahead of it, a compaction summary replaces it). Crew steps share one session id and each
/// begins with its own prompt, so a request that starts elsewhere is not compared with the last.
fn anchor(req: &ChatRequest) -> u64 {
    let first = req.messages.iter().find(|m| m.role == Role::User);
    let text = first
        .and_then(|m| m.content.iter().rev().find_map(|p| if let Part::Text { text } = p { Some(text.as_str()) } else { None }))
        .unwrap_or("");
    hash_of(text)
}

/// Before a request: compares it with the session's last one on the same model. `compacted` says
/// history was rewritten on purpose since (compaction logs itself). Returns the break, if any; the
/// request becomes the one to extend either way. Call [`made`] once the request has returned.
pub fn check(session: &str, req: &ChatRequest, compacted: bool) -> Option<Broken> {
    let print = Fingerprint::of(req);
    let anchor = anchor(req);
    let mut map = seen().lock().unwrap_or_else(|p| p.into_inner());
    let key = (session.to_string(), req.model.clone());
    if map.get(&key).is_some_and(|prev| prev.anchor != anchor) {
        map.remove(&key);
    }
    let found = map.get(&key).and_then(|prev| {
        let cause = diverges(&prev.print, &print)?;
        let others = slot_seq(&req.model).saturating_sub(prev.slot);
        (!compacted).then_some(Broken { cause, others })
    });
    // A foreign request alone (no diff) leaves the cache replaced too.
    let found = found.or_else(|| {
        let prev = map.get(&key)?;
        let others = slot_seq(&req.model).saturating_sub(prev.slot);
        (others > 0 && !compacted).then_some(Broken { cause: SLOT_ONLY.into(), others })
    });
    if map.len() > 256 {
        map.clear();
    }
    map.insert(key, Seen { print, anchor, slot: slot_seq(&req.model) });
    found
}

/// After the request returned: its own count is not "another request".
pub fn made(session: &str, model: &str) {
    let mut map = seen().lock().unwrap_or_else(|p| p.into_inner());
    if let Some(s) = map.get_mut(&(session.to_string(), model.to_string())) {
        s.slot = slot_seq(model);
    }
}

/// Forgets a session (it was deleted).
pub fn forget(session: &str) {
    seen().lock().unwrap_or_else(|p| p.into_inner()).retain(|k, _| k.0 != session);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SystemBlock, ToolSpec};

    fn req(system: &str, tool_desc: &str, msgs: &[&str]) -> ChatRequest {
        let mut r = ChatRequest::new("m", msgs.iter().map(|t| Message::user(*t)).collect());
        r.system = vec![SystemBlock { text: system.into(), cache: false }];
        r.tools = vec![ToolSpec { name: "t".into(), description: tool_desc.into(), input_schema: serde_json::json!({}) }];
        r
    }

    #[test]
    fn a_longer_request_extends_and_each_section_names_its_break() {
        let a = Fingerprint::of(&req("sys", "d", &["one"]));
        assert_eq!(diverges(&a, &Fingerprint::of(&req("sys", "d", &["one", "two"]))), None);
        assert!(diverges(&a, &Fingerprint::of(&req("sys2", "d", &["one"]))).unwrap().starts_with("system block 0 changed"));
        assert!(diverges(&a, &Fingerprint::of(&req("sys", "d2", &["one"]))).unwrap().contains("tool 0 (t)"));
        let m = diverges(&a, &Fingerprint::of(&req("sys", "d", &["ONE", "two"]))).unwrap();
        assert!(m.starts_with("message 0 (user) part 0 changed"), "{m}");
        assert!(diverges(&Fingerprint::of(&req("sys", "d", &["one", "two"])), &a).unwrap().contains("shorter"));
    }

    #[test]
    fn the_watch_reports_a_break_once_and_not_after_compaction() {
        let s = "prefix-watch-test";
        assert!(check(s, &req("sys", "d", &["one"]), false).is_none(), "nothing to compare with yet");
        assert!(check(s, &req("sys", "d", &["one", "two"]), false).is_none());
        let b = check(s, &req("sys!", "d", &["one", "two", "three"]), false).expect("a break");
        assert!(b.cause.starts_with("system block 0"), "{}", b.cause);
        assert!(check(s, &req("sys!", "d", &["one", "two", "three", "four"]), false).is_none(), "reported once");
        assert!(check(s, &req("other", "d", &["x"]), true).is_none(), "compaction logs itself");
        forget(s);
    }

    #[test]
    fn another_request_on_the_slot_is_named() {
        let s = "prefix-watch-slot";
        let model = "slot-test-model";
        let mut r = req("sys", "d", &["one"]);
        r.model = model.into();
        assert!(check(s, &r, false).is_none());
        note_request(model);
        made(s, model);
        note_request(model);
        r.messages.push(Message::user("two"));
        let b = check(s, &r, false).expect("the slot was used by another request");
        assert_eq!(b.others, 1);
        assert!(b.line(s, model).contains("1 other request(s) used the slot-test-model slot"), "{}", b.line(s, model));
        forget(s);
    }
}
