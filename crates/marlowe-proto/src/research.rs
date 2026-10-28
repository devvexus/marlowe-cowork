//! Research values shared by the daemon, `marlowe-research` and the clients: the source channels
//! and the sub-question a query is built for.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// A research source channel. Keyless by construction: no channel carries a credential
/// (ADR-036 §2). `Memory` is the user's own store, recalled through the Mind and kept in its own
/// trust bucket; every other channel returns untrusted content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelId {
    Arxiv,
    OpenAlex,
    Crossref,
    SemanticScholar,
    Wikipedia,
    Wikidata,
    Github,
    HackerNews,
    StackExchange,
    Pubmed,
    Rss,
    /// General web search: SearXNG when configured, else DuckDuckGo HTML lite, else a hosted
    /// search the daemon supplies.
    Web,
    Memory,
}

impl ChannelId {
    /// Every channel, in display order.
    pub const ALL: [ChannelId; 13] = [
        ChannelId::Arxiv,
        ChannelId::OpenAlex,
        ChannelId::Crossref,
        ChannelId::SemanticScholar,
        ChannelId::Wikipedia,
        ChannelId::Wikidata,
        ChannelId::Github,
        ChannelId::HackerNews,
        ChannelId::StackExchange,
        ChannelId::Pubmed,
        ChannelId::Rss,
        ChannelId::Web,
        ChannelId::Memory,
    ];

    /// Stable, model-visible name.
    pub fn as_str(self) -> &'static str {
        match self {
            ChannelId::Arxiv => "arxiv",
            ChannelId::OpenAlex => "open_alex",
            ChannelId::Crossref => "crossref",
            ChannelId::SemanticScholar => "semantic_scholar",
            ChannelId::Wikipedia => "wikipedia",
            ChannelId::Wikidata => "wikidata",
            ChannelId::Github => "github",
            ChannelId::HackerNews => "hacker_news",
            ChannelId::StackExchange => "stack_exchange",
            ChannelId::Pubmed => "pubmed",
            ChannelId::Rss => "rss",
            ChannelId::Web => "web",
            ChannelId::Memory => "memory",
        }
    }

    /// The name a person reads: `Semantic Scholar`, `arXiv`.
    pub fn label(self) -> &'static str {
        match self {
            ChannelId::Arxiv => "arXiv",
            ChannelId::OpenAlex => "OpenAlex",
            ChannelId::Crossref => "Crossref",
            ChannelId::SemanticScholar => "Semantic Scholar",
            ChannelId::Wikipedia => "Wikipedia",
            ChannelId::Wikidata => "Wikidata",
            ChannelId::Github => "GitHub",
            ChannelId::HackerNews => "Hacker News",
            ChannelId::StackExchange => "Stack Exchange",
            ChannelId::Pubmed => "PubMed",
            ChannelId::Rss => "RSS",
            ChannelId::Web => "Web",
            ChannelId::Memory => "Memory",
        }
    }

    /// False only for `Memory`, which is read through the Mind, never the network.
    pub fn is_network(self) -> bool {
        self != ChannelId::Memory
    }
}

impl fmt::Display for ChannelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A string that is not a channel.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown research channel '{0}'")]
pub struct ChannelParseError(pub String);

impl FromStr for ChannelId {
    type Err = ChannelParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let k = s.trim().to_ascii_lowercase().replace(['-', ' '], "_");
        let alias = match k.as_str() {
            "openalex" => "open_alex",
            "s2" | "semanticscholar" => "semantic_scholar",
            "hn" | "hackernews" => "hacker_news",
            "stackexchange" | "stackoverflow" | "stack_overflow" => "stack_exchange",
            "atom" | "feed" => "rss",
            "searxng" | "search" | "web_search" => "web",
            other => other,
        };
        ChannelId::ALL.into_iter().find(|c| c.as_str() == alias).ok_or_else(|| ChannelParseError(s.to_string()))
    }
}

/// One sub-question, with the channels routed to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubQuestion {
    /// 1-based, stable across edits; checkpoint keys use it.
    pub id: u32,
    pub question: String,
    pub channels: Vec<ChannelId>,
    /// Why these channels, in one line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rationale: Option<String>,
    /// Short keyword queries for the channels (named methods, model families, benchmarks), as
    /// a search engine would match them. Empty: the question itself is searched.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queries: Vec<String>,
}
