//! Wikipedia (search with intro extracts and the linked Wikidata item) and Wikidata (entity
//! search, falling back to full-text search when the question is not an entity name).
//!
//! A Wikipedia article and the Wikidata item it links are different artifacts about one entity:
//! the article carries the item id as a claim (Wikipedia is not the QID authority), so the pair
//! resolves `Unresolved` — one root for corroboration, two rows on display.

use marlowe_proto::ChannelId;
use serde_json::Value;

use super::{clip, enc, get_json, limit, s, spec_of, SNIPPET_CHARS};
use crate::channel::{Channel, ChannelCtx, ChannelError, ChannelQuery, ChannelSpec};
use crate::source::{Identity, Namespace, SourceRef};

// ------------------------------------------------------------------ Wikipedia

pub struct WikipediaChannel {
    spec: ChannelSpec,
    lang: String,
}

impl WikipediaChannel {
    /// `lang` is the edition (`en`); anything that is not a plain language code falls back to `en`.
    pub fn new(lang: &str) -> Self {
        let ok = (2..=12).contains(&lang.len()) && lang.bytes().all(|b| b.is_ascii_lowercase() || b == b'-');
        let lang = if ok { lang.to_string() } else { "en".into() };
        let mut spec = spec_of(ChannelId::Wikipedia);
        spec.endpoint = format!("https://{lang}.wikipedia.org/w/api.php");
        Self { spec, lang }
    }

    pub fn lang(&self) -> &str {
        &self.lang
    }

    /// generator=search with intro extracts, canonical URLs and the Wikidata item, one request.
    pub fn search_url(&self, q: &ChannelQuery) -> String {
        format!(
            "{}?action=query&format=json&formatversion=2&generator=search&gsrsearch={}&gsrlimit={}\
             &prop=extracts%7Cinfo%7Cpageprops&exintro=1&explaintext=1&exlimit=max&inprop=url\
             &ppprop=wikibase_item&redirects=1",
            self.spec.endpoint,
            enc(q.text.trim()),
            limit(q).min(20)
        )
    }
}

/// The pages of a generator=search answer, in search rank order.
pub fn wikipedia_pages(v: &Value) -> Vec<SourceRef> {
    let mut pages: Vec<&Value> =
        v.get("query").and_then(|q| q.get("pages")).and_then(Value::as_array).into_iter().flatten().collect();
    pages.sort_by_key(|p| p.get("index").and_then(Value::as_u64).unwrap_or(u64::MAX));
    pages
        .into_iter()
        .filter(|p| p.get("missing").is_none())
        .filter_map(|p| {
            let title = s(p, "title")?;
            let url = s(p, "canonicalurl").or_else(|| s(p, "fullurl"))?;
            let mut identity = Identity::default().with_url(&url);
            if let Some(q) = p.get("pageprops").and_then(|pp| s(pp, "wikibase_item")) {
                identity = identity.with_qid(&q);
            }
            Some(
                SourceRef {
                    identity: Some(identity),
                    date: s(p, "touched").map(|t| t.chars().take(10).collect()),
                    snippet: clip(&s(p, "extract").unwrap_or_default(), SNIPPET_CHARS),
                    ..SourceRef::new(ChannelId::Wikipedia, &url, &title)
                }
                .with_meta("date_kind", "last_edited"),
            )
        })
        .collect()
}

impl Channel for WikipediaChannel {
    fn spec(&self) -> &ChannelSpec {
        &self.spec
    }

    fn search(&self, query: &ChannelQuery, ctx: &ChannelCtx<'_>) -> Result<Vec<SourceRef>, ChannelError> {
        let v = get_json(ctx, ChannelId::Wikipedia, self.search_url(query))?;
        if let Some(e) = v.get("error") {
            return Err(ChannelError::Parse { channel: ChannelId::Wikipedia, detail: e.to_string() });
        }
        Ok(wikipedia_pages(&v))
    }
}

// ------------------------------------------------------------------ Wikidata

pub struct WikidataChannel {
    spec: ChannelSpec,
}

impl Default for WikidataChannel {
    fn default() -> Self {
        Self::new()
    }
}

/// Words that make a question rather than a name; dropped before the full-text fallback.
const QUESTION_WORDS: [&str; 16] =
    ["who", "what", "when", "where", "which", "is", "was", "are", "were", "the", "a", "an", "of", "did", "does", "do"];

/// Name-like spans of a question, longest first: runs of capitalised words (`Hugging Face`,
/// `Bank of England`, `GPT-4`), question words excluded. At most two.
pub fn entity_spans(text: &str) -> Vec<String> {
    let words: Vec<&str> =
        text.split_whitespace().map(|w| w.trim_matches(|c: char| ",.;:?!\"'()".contains(c))).collect();
    let is_name = |w: &str| {
        w.chars().next().is_some_and(char::is_uppercase) && !QUESTION_WORDS.contains(&w.to_ascii_lowercase().as_str())
    };
    let mut spans: Vec<String> = Vec::new();
    let mut i = 0;
    while i < words.len() {
        if !is_name(words[i]) {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        while j < words.len()
            && (is_name(words[j]) || (words[j] == "of" && j + 1 < words.len() && is_name(words[j + 1])))
        {
            j += 1;
        }
        spans.push(words[i..j].join(" "));
        i = j;
    }
    spans.sort_by_key(|s| std::cmp::Reverse(s.len()));
    spans.dedup();
    spans.truncate(2);
    spans
}

fn item_ref(id: &str, label: &str, description: &str, aliases: &[String]) -> SourceRef {
    let url = format!("https://www.wikidata.org/wiki/{id}");
    let mut snippet = description.to_string();
    if !aliases.is_empty() {
        snippet.push_str(&format!(" (also: {})", aliases.join(", ")));
    }
    SourceRef {
        identity: Some(Identity::default().with_qid(id)),
        snippet: clip(snippet.trim(), SNIPPET_CHARS),
        ..SourceRef::new(ChannelId::Wikidata, &url, if label.is_empty() { id } else { label })
    }
}

/// `wbsearchentities` hits.
pub fn wikidata_hits(v: &Value) -> Vec<SourceRef> {
    v.get("search")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|h| {
            let id = s(h, "id")?;
            let aliases: Vec<String> = h
                .get("aliases")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|a| a.as_str().map(String::from))
                .collect();
            Some(item_ref(&id, &s(h, "label").unwrap_or_default(), &s(h, "description").unwrap_or_default(), &aliases))
        })
        .collect()
}

/// `wbgetentities` answers, in the order of `ids`.
pub fn wikidata_entities(v: &Value, ids: &[String]) -> Vec<SourceRef> {
    let Some(map) = v.get("entities").and_then(Value::as_object) else { return Vec::new() };
    ids.iter()
        .filter_map(|id| {
            let e = map.get(id)?;
            if e.get("missing").is_some() {
                return None;
            }
            let en = |k: &str| e.get(k).and_then(|x| x.get("en")).and_then(|x| s(x, "value")).unwrap_or_default();
            let aliases: Vec<String> = e
                .get("aliases")
                .and_then(|a| a.get("en"))
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|a| s(a, "value"))
                .take(5)
                .collect();
            Some(item_ref(id, &en("labels"), &en("descriptions"), &aliases))
        })
        .collect()
}

impl WikidataChannel {
    pub fn new() -> Self {
        Self { spec: spec_of(ChannelId::Wikidata) }
    }

    fn entities(&self, ids: &[String], ctx: &ChannelCtx<'_>) -> Result<Vec<SourceRef>, ChannelError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let url = format!(
            "{}?action=wbgetentities&format=json&ids={}&props=labels%7Cdescriptions%7Caliases&languages=en",
            self.spec.endpoint,
            enc(&ids.join("|"))
        );
        Ok(wikidata_entities(&get_json(ctx, ChannelId::Wikidata, url)?, ids))
    }
}

impl Channel for WikidataChannel {
    fn spec(&self) -> &ChannelSpec {
        &self.spec
    }

    fn search(&self, query: &ChannelQuery, ctx: &ChannelCtx<'_>) -> Result<Vec<SourceRef>, ChannelError> {
        let n = limit(query).min(50);
        let text = query.text.trim().trim_end_matches('?');
        let url = format!(
            "{}?action=wbsearchentities&format=json&language=en&uselang=en&type=item&limit={n}&search={}",
            self.spec.endpoint,
            enc(text)
        );
        let hits = wikidata_hits(&get_json(ctx, ChannelId::Wikidata, url)?);
        if !hits.is_empty() {
            return Ok(hits);
        }
        // A question, not a name: try the names in it.
        for span in entity_spans(text).into_iter().filter(|s| s != text) {
            let url = format!(
                "{}?action=wbsearchentities&format=json&language=en&uselang=en&type=item&limit={n}&search={}",
                self.spec.endpoint,
                enc(&span)
            );
            let hits = wikidata_hits(&get_json(ctx, ChannelId::Wikidata, url)?);
            if !hits.is_empty() {
                return Ok(hits);
            }
        }
        // Not an entity name: full-text search for item ids, then their labels.
        let words: Vec<&str> =
            text.split_whitespace().filter(|w| !QUESTION_WORDS.contains(&w.to_ascii_lowercase().as_str())).collect();
        if words.is_empty() {
            return Ok(Vec::new());
        }
        let url = format!(
            "{}?action=query&format=json&list=search&srnamespace=0&srlimit={}&srsearch={}",
            self.spec.endpoint,
            n.min(20),
            enc(&words.join(" "))
        );
        let v = get_json(ctx, ChannelId::Wikidata, url)?;
        let ids: Vec<String> = v
            .get("query")
            .and_then(|q| q.get("search"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|h| s(h, "title"))
            .filter(|t| Identity::default().with_qid(t).qid.is_some())
            .collect();
        self.entities(&ids, ctx)
    }

    fn lookup(&self, ns: Namespace, id: &str, ctx: &ChannelCtx<'_>) -> Result<Option<SourceRef>, ChannelError> {
        let Some(q) = (ns == Namespace::Qid).then(|| Identity::default().with_qid(id).qid).flatten() else {
            return Err(ChannelError::NoLookup { channel: ChannelId::Wikidata, namespace: ns });
        };
        Ok(self.entities(&[q], ctx)?.into_iter().next())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn names_are_found_inside_questions() {
        assert_eq!(entity_spans("who founded Hugging Face?"), ["Hugging Face"]);
        assert_eq!(entity_spans("When did the Bank of England adopt GPT-4"), ["Bank of England", "GPT-4"]);
        assert!(entity_spans("what is a monad").is_empty());
    }

    #[test]
    fn languages_are_validated_and_pages_keep_rank_order() {
        assert_eq!(WikipediaChannel::new("de").lang(), "de");
        assert_eq!(WikipediaChannel::new("evil.com/x").lang(), "en");
        let v = json!({"query": {"pages": [
            {"index": 2, "title": "B", "fullurl": "https://en.wikipedia.org/wiki/B", "extract": "b"},
            {"index": 1, "title": "A", "canonicalurl": "https://en.wikipedia.org/wiki/A", "pageprops": {"wikibase_item": "Q1"}},
            {"index": 3, "title": "C", "missing": true}
        ]}});
        let p = wikipedia_pages(&v);
        assert_eq!(p.iter().map(|r| r.title.as_str()).collect::<Vec<_>>(), ["A", "B"]);
        assert_eq!(p[0].identity.as_ref().unwrap().qid.as_deref(), Some("Q1"));
    }
}
