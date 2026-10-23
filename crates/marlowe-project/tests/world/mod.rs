//! A recorded little world for the research phases: channel results, pages and model replies come
//! from `tests/fixtures/research/`. Nothing here touches a network or a model.

#![allow(dead_code)]

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use marlowe_model::{CancelToken, ChatRequest};
use marlowe_project::research::{Admit, Failure, Fetched, MemoryEntry, MemorySink, PageFetch, Resolver, ResearchDeps, Web};
use marlowe_research::{ChannelError, ChannelId, ChannelQuery, SourceRef};
use serde_json::{json, Value};

use crate::common::{Fake, Say};

/// The recorded world.
pub struct World {
    pub json: Value,
    dir: PathBuf,
}

impl World {
    pub fn load() -> Arc<World> {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/research");
        let json: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("world.json")).expect("world.json")).expect("valid json");
        Arc::new(World { json, dir })
    }

    pub fn page_text(&self, file: &str) -> String {
        std::fs::read_to_string(self.dir.join("pages").join(file)).unwrap_or_else(|e| panic!("{file}: {e}"))
    }
}

// ---- channels ---------------------------------------------------------------------------------

/// Channel search over the recorded results. `searches` logs every query.
pub struct FakeWeb {
    pub world: Arc<World>,
    pub searches: Mutex<Vec<(ChannelId, String, Option<u16>)>>,
    pub down: Mutex<Vec<ChannelId>>,
}

impl FakeWeb {
    pub fn new(world: &Arc<World>) -> Arc<FakeWeb> {
        Arc::new(FakeWeb { world: world.clone(), searches: Mutex::new(Vec::new()), down: Mutex::new(Vec::new()) })
    }

    pub fn queries(&self) -> Vec<String> {
        self.searches.lock().unwrap().iter().map(|s| s.1.clone()).collect()
    }
}

impl Web for FakeWeb {
    fn channels(&self) -> Vec<ChannelId> {
        vec![ChannelId::Web, ChannelId::Arxiv]
    }

    fn search(&self, channel: ChannelId, q: &ChannelQuery, _c: &CancelToken) -> Result<Vec<SourceRef>, ChannelError> {
        self.searches.lock().unwrap().push((channel, q.text.clone(), q.since_year));
        if self.down.lock().unwrap().contains(&channel) {
            return Err(ChannelError::RateLimited { channel, retry_after: None });
        }
        if channel != ChannelId::Web {
            return Ok(Vec::new());
        }
        let text = q.text.to_lowercase();
        let mut out = Vec::new();
        for entry in self.world.json["search"].as_array().unwrap() {
            if text.contains(entry["when"].as_str().unwrap()) {
                for r in entry["results"].as_array().unwrap() {
                    out.push(SourceRef::new(ChannelId::Web, r["url"].as_str().unwrap(), r["title"].as_str().unwrap()));
                    out.last_mut().unwrap().snippet = r["snippet"].as_str().unwrap().into();
                }
            }
        }
        out.truncate(q.limit);
        Ok(out)
    }
}

/// Page reading over the recorded pages: asks `admit` for the first url and every redirect hop, as
/// a real reader must, and logs what it actually requested.
pub struct FakePages {
    pub world: Arc<World>,
    pub requested: Mutex<Vec<String>>,
}

impl FakePages {
    pub fn new(world: &Arc<World>) -> Arc<FakePages> {
        Arc::new(FakePages { world: world.clone(), requested: Mutex::new(Vec::new()) })
    }

    pub fn requests(&self) -> Vec<String> {
        self.requested.lock().unwrap().clone()
    }
}

impl PageFetch for FakePages {
    fn read(&self, url: &str, admit: Admit<'_>, _c: &CancelToken) -> Fetched {
        if let Err(why) = admit(url) {
            return Fetched::failed(url, Failure::Blocked, why);
        }
        self.requested.lock().unwrap().push(url.to_string());
        let Some(page) = self.world.json["pages"].get(url) else { return Fetched::failed(url, Failure::Unreachable, "the host could not be reached") };
        if let Some(to) = page.get("redirect").and_then(Value::as_str) {
            if let Err(why) = admit(to) {
                return Fetched::failed(url, Failure::Blocked, why);
            }
            self.requested.lock().unwrap().push(to.to_string());
            return Fetched::failed(url, Failure::Unreachable, "the host could not be reached");
        }
        Fetched {
            url: url.into(),
            final_url: url.into(),
            title: page.get("title").and_then(Value::as_str).map(str::to_string),
            text: self.world.page_text(page["file"].as_str().unwrap()),
            ..Default::default()
        }
    }
}

/// Host names: the ones the world calls private point at 10.0.0.9, the rest at a public address.
pub struct Dns(pub Arc<World>);

impl Resolver for Dns {
    fn resolve(&self, host: &str) -> Result<Vec<IpAddr>, String> {
        let private = self.0.json["hosts"]["private"].as_array().unwrap().iter().any(|h| h == host);
        Ok(vec![if private { "10.0.0.9" } else { "93.184.216.34" }.parse().unwrap()])
    }
}

/// A memory sink that keeps what it was given.
#[derive(Default)]
pub struct Memory(pub Mutex<Vec<MemoryEntry>>);

impl MemorySink for Memory {
    fn remember(&self, e: &MemoryEntry) -> Result<(), String> {
        self.0.lock().unwrap().push(e.clone());
        Ok(())
    }
}

/// The dependencies of a research run in `home`.
pub struct Rig {
    pub deps: Arc<ResearchDeps>,
    pub web: Arc<FakeWeb>,
    pub pages: Arc<FakePages>,
    pub memory: Arc<Memory>,
    pub output: PathBuf,
}

pub fn rig(world: &Arc<World>, home: &std::path::Path) -> Rig {
    let web = FakeWeb::new(world);
    let pages = FakePages::new(world);
    let memory = Arc::new(Memory::default());
    let output = home.join("output");
    let deps = Arc::new(ResearchDeps::new(home.join("projects"), output.clone(), web.clone(), pages.clone(), Arc::new(Dns(world.clone())), memory.clone()));
    Rig { deps, web, pages, memory, output }
}

// ---- the recorded models ---------------------------------------------------------------------------

fn user(req: &ChatRequest) -> String {
    req.messages.last().map(|m| m.joined_text()).unwrap_or_default()
}

fn system(req: &ChatRequest) -> String {
    req.system.iter().map(|b| b.text.as_str()).collect::<Vec<_>>().join("\n")
}

/// `N1: text` lines (or `[C3] text`, `S2: text`) of a prompt.
fn lines_with(text: &str, prefix: char) -> Vec<(u32, String)> {
    text.lines()
        .filter_map(|l| {
            let l = l.trim_start();
            let rest = l.strip_prefix(prefix)?;
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            let after = rest[digits.len()..].strip_prefix(": ")?;
            Some((digits.parse().ok()?, after.to_string()))
        })
        .collect()
}

fn tokens(s: &str) -> Vec<String> {
    s.split(|c: char| !(c.is_alphanumeric() || c == '.' || c == '-'))
        .filter(|w| w.chars().any(|c| c.is_ascii_digit()) || w.chars().count() >= 5)
        .map(|w| w.trim_matches('.').to_lowercase())
        .filter(|w| !w.is_empty())
        .collect()
}

/// Whether `evidence` carries at least half of the tokens of `claim`.
pub fn supports(claim: &str, evidence: &str) -> bool {
    let want = tokens(claim);
    let have = evidence.to_lowercase();
    !want.is_empty() && want.iter().filter(|t| have.contains(t.as_str())).count() * 2 >= want.len()
}

/// What the world's models say. `lead` picks the recorded decision (`report` or `keep`).
pub fn model(world: &Arc<World>, lead: &'static str) -> Arc<Fake> {
    Fake::with(responder(world, lead))
}

/// The recorded replies as a function, for a test that wants to add something of its own.
pub fn responder(world: &Arc<World>, lead: &'static str) -> impl Fn(&ChatRequest, usize, &CancelToken) -> Say + Send + Sync + 'static {
    let world = world.clone();
    let attempts: Mutex<HashMap<String, usize>> = Mutex::new(HashMap::new());
    move |req: &ChatRequest, _n: usize, _c: &CancelToken| {
        let props: Vec<String> = req.format.as_ref().and_then(|f| f.get("properties")).and_then(Value::as_object).map(|o| o.keys().cloned().collect()).unwrap_or_default();
        let has = |k: &str| props.iter().any(|p| p == k);
        let u = user(req);
        if has("unknown_names") {
            let v = world.json["vectors"].as_object().unwrap();
            return match v.iter().find(|(k, _)| u.contains(k.as_str())) {
                Some((_, reply)) => Say::Json(reply.clone()),
                None => Say::Json(json!({"vectors": [], "unknown_names": []})),
            };
        }
        if has("relevancy") {
            // A retry appends a turn after the first prompt, so the page is named in an earlier message.
            let all: String = req.messages.iter().map(|m| m.joined_text()).collect::<Vec<_>>().join("
");
            let title = all.split("The page \"").nth(1).and_then(|r| r.split('"').next()).unwrap_or("").to_string();
            let replies = world.json["analyze"][&title].as_array().unwrap_or_else(|| panic!("no recorded reply for page {title:?}"));
            let mut a = attempts.lock().unwrap();
            let n = a.entry(title).or_insert(0);
            let reply = replies[(*n).min(replies.len() - 1)].clone();
            *n += 1;
            return Say::Json(reply);
        }
        if has("groups") {
            // A model that groups claims with exactly the same words.
            let new = lines_with(&u, 'N');
            let old = lines_with(&u, 'E');
            let mut groups: Vec<Value> = Vec::new();
            let mut done: Vec<u32> = Vec::new();
            for (i, text) in &new {
                if done.contains(i) {
                    continue;
                }
                let mut members: Vec<String> = new.iter().filter(|(j, t)| t == text && !done.contains(j)).map(|(j, _)| format!("N{j}")).collect();
                for (j, t) in &old {
                    if t == text {
                        members.insert(0, format!("E{j}"));
                    }
                }
                done.extend(new.iter().filter(|(_, t)| t == text).map(|(j, _)| *j));
                groups.push(json!({"members": members, "claim": text, "contradicts": []}));
            }
            return Say::Json(json!({ "groups": groups }));
        }
        if has("verdict") && has("used") {
            let claim = u.split("<<<\n").nth(1).and_then(|r| r.split("\n>>>").next()).unwrap_or("").to_string();
            let body = u.split("<<<\n").nth(2).unwrap_or("").to_string();
            let found = body.split("\n\n").position(|p| supports(&claim, p));
            return Say::Json(match found {
                Some(i) => json!({"reason": "A passage states it.", "verdict": "upheld", "corrected_claim": null, "used": [i + 1]}),
                None => json!({"reason": "The passages do not say it.", "verdict": "unsupported", "corrected_claim": null, "used": []}),
            });
        }
        if has("gaps") && has("focus") {
            return Say::Json(world.json["lead"][lead].clone());
        }
        if has("text") && has("end") {
            let claims: Vec<(u32, String)> = u
                .lines()
                .filter_map(|l| {
                    let rest = l.strip_prefix("[C")?;
                    let (n, after) = rest.split_once("] ")?;
                    let text = after.rsplit_once(" (score").map(|(t, _)| t).unwrap_or(after);
                    Some((n.parse().ok()?, text.to_string()))
                })
                .collect();
            let mut text: Vec<String> = claims.iter().map(|(n, t)| format!("{t} [C{n}].")).collect();
            if system(req).contains("section of the report") && u.contains("Kimi") {
                // An overreach: a sentence its own citation does not support.
                if let Some((n, _)) = claims.first() {
                    text.push(format!("Kimi K2.6 beats every other model on every benchmark [C{n}]."));
                }
            }
            return Say::Json(json!({"text": text.join(" "), "end": "END"}));
        }
        if has("verdicts") {
            let sentences = lines_with(&u, 'S');
            let mut verdicts = Vec::new();
            for (n, s) in &sentences {
                let at = u.find(&format!("S{n}: ")).unwrap();
                let block: String = u[at..].lines().skip(1).take_while(|l| l.trim_start().starts_with("passage")).collect::<Vec<_>>().join("\n");
                let sentence = s.split(" [C").next().unwrap_or(s);
                let v = if supports(sentence, &block) { "supported" } else { "unsupported" };
                verdicts.push(json!({"n": n, "verdict": v, "reason": "checked against the passages"}));
            }
            return Say::Json(json!({ "verdicts": verdicts }));
        }
        Say::Schema
    }
}
