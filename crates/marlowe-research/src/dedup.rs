//! Identity-key cascade dedup (ADR-036 §4). Routes merge into sources; sources never merge into
//! each other. Three outcomes: `Same` merges, `Distinct` starts a source, `Unresolved` starts a
//! source that displays separately but counts as one with its partner for corroboration.
//!
//! An identifier merges only when it is confirmed: at least one route authoritative for its
//! namespace and the other at least a curated index. A DOI printed on a page is a claim and
//! yields `Unresolved`, never `Same` (ADR-036 §5).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use marlowe_proto::ChannelId;

use crate::channel::{Assertion, ChannelSpec};
use crate::channels::builtin_spec;
use crate::source::{first_author_surname, normalize_title, Identity, Namespace, SourceRef};

/// A deduplicated source, numbered in arrival order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SourceId(pub u32);

/// What dedup decided for a route.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Resolution {
    Same,
    Unresolved,
    Distinct,
}

/// Which cascade tier fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchTier {
    Identifier(Namespace),
    TitleAuthorYear,
    CanonicalUrl,
}

/// Where a route landed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    pub source: SourceId,
    pub resolution: Resolution,
    pub tier: Option<MatchTier>,
    /// For `Unresolved`: the source it may be the same as.
    pub partner: Option<SourceId>,
}

/// A metadata field two routes disagree on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictField {
    Title,
    Year,
    Doi,
    FirstAuthor,
}

/// A disagreement between routes: evidence, never silently resolved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conflict {
    pub field: ConflictField,
    pub values: Vec<(ChannelId, String)>,
}

/// One artifact: an identity, its routes kept verbatim, and their conflicts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Source {
    pub id: SourceId,
    /// Reconciled view: earlier routes win per field.
    pub identity: Identity,
    pub routes: Vec<SourceRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<Conflict>,
    /// Any route reported a retraction; wins unconditionally.
    #[serde(default)]
    pub retracted: bool,
    /// Folded into this source once an authority confirmed they are one work; routes moved there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias_of: Option<SourceId>,
}

impl Source {
    /// The first route's title (display precedence is arrival order).
    pub fn title(&self) -> &str {
        self.routes.first().map_or("", |r| r.title.as_str())
    }

    pub fn channels(&self) -> Vec<ChannelId> {
        let mut c: Vec<ChannelId> = self.routes.iter().map(|r| r.channel).collect();
        c.sort();
        c.dedup();
        c
    }
}

/// Tells dedup how far a channel's identifiers may be believed.
pub trait IdentityAuthority {
    fn assertion(&self, channel: ChannelId, ns: Namespace) -> Assertion;
}

/// The built-in channel specs' authority.
#[derive(Debug, Clone, Copy, Default)]
pub struct BuiltinAuthority;

impl IdentityAuthority for BuiltinAuthority {
    fn assertion(&self, channel: ChannelId, ns: Namespace) -> Assertion {
        match builtin_spec(channel) {
            Some(s) => s.assertion(ns),
            // Memory: the user's own words, but never an identity authority.
            None if ns == Namespace::Url => Assertion::Authoritative,
            None => Assertion::Asserted,
        }
    }
}

impl IdentityAuthority for Vec<ChannelSpec> {
    fn assertion(&self, channel: ChannelId, ns: Namespace) -> Assertion {
        match self.iter().find(|s| s.id == channel) {
            Some(s) => s.assertion(ns),
            None => BuiltinAuthority.assertion(channel, ns),
        }
    }
}

/// Is a shared key between routes asserted as `a` and `b` confirmed?
fn confirmed(a: Assertion, b: Assertion) -> bool {
    a.max(b) == Assertion::Authoritative && a.min(b) >= Assertion::Indexed
}

/// A source indexed by title: its id, year and curation.
type TitleHit = (SourceId, Option<u16>, Assertion);

/// The deduplicator for one research run.
pub struct Deduper<A: IdentityAuthority = BuiltinAuthority> {
    sources: Vec<Source>,
    /// (namespace, value) -> every source holding it, with the strongest assertion seen there.
    by_key: HashMap<(Namespace, String), Vec<(SourceId, Assertion)>>,
    /// (normalised title, first-author surname) -> sources with the year and assertion.
    by_title: HashMap<(String, String), Vec<TitleHit>>,
    unresolved: Vec<(SourceId, SourceId)>,
    authority: A,
}

impl Default for Deduper<BuiltinAuthority> {
    fn default() -> Self {
        Self::new(BuiltinAuthority)
    }
}

impl<A: IdentityAuthority> Deduper<A> {
    pub fn new(authority: A) -> Self {
        Self {
            sources: Vec::new(),
            by_key: HashMap::new(),
            by_title: HashMap::new(),
            unresolved: Vec::new(),
            authority,
        }
    }
}

/// Where an identifier match sends a route: the source, the tier, and the key to fold on when
/// the route is the namespace's authority.
type Target = (SourceId, MatchTier, Option<(Namespace, String)>);

impl<A: IdentityAuthority> Deduper<A> {
    /// Places a route: merges it into a source, or starts one. A route from the authority for
    /// an identifier also folds in every other curated source holding that identifier: the
    /// confirmation ADR-036 §5 asks for (see [`Deduper::unconfirmed`]).
    pub fn add(&mut self, route: SourceRef) -> Placement {
        let identity = route.effective_identity();
        let mut partner: Option<(SourceId, MatchTier)> = None;

        // Tiers 1 and 3: identifiers in cascade order, the canonical URL last.
        if let Some((sid, tier, fold)) = self.identifier_target(&route, &identity, &mut partner) {
            let placed = self.merge(sid, route, identity, tier);
            if let Some((ns, v)) = fold {
                self.fold_holders(placed.source, ns, &v);
            }
            return placed;
        }

        // Tier 2: normalised title + first-author surname, promoted to Same by year ±1.
        let title_key = first_author_surname(&route.authors).map(|a| (normalize_title(&route.title), a));
        if let Some(tk) = title_key.as_ref().filter(|(t, _)| !t.is_empty()) {
            let mine = self.curation(route.channel);
            let year = route.year();
            let hits = self.by_title.get(tk).cloned().unwrap_or_default();
            for (sid, their_year, theirs) in hits {
                let sid = self.resolve(sid);
                let years_agree = matches!((year, their_year), (Some(a), Some(b)) if a.abs_diff(b) <= 1);
                if years_agree && mine.min(theirs) >= Assertion::Indexed {
                    return self.merge(sid, route, identity, MatchTier::TitleAuthorYear);
                }
                partner.get_or_insert((sid, MatchTier::TitleAuthorYear));
            }
        }

        let sid = SourceId(self.sources.len() as u32);
        self.index(sid, &route, &identity);
        let retracted = route.retracted;
        self.sources.push(Source {
            id: sid,
            identity,
            routes: vec![route],
            conflicts: Vec::new(),
            retracted,
            alias_of: None,
        });
        match partner {
            Some((p, tier)) => {
                self.unresolved.push((p, sid));
                Placement { source: sid, resolution: Resolution::Unresolved, tier: Some(tier), partner: Some(p) }
            }
            None => Placement { source: sid, resolution: Resolution::Distinct, tier: None, partner: None },
        }
    }

    /// The first confirmed identifier match, recording the first unconfirmed one as `partner`.
    fn identifier_target(
        &self,
        route: &SourceRef,
        identity: &Identity,
        partner: &mut Option<(SourceId, MatchTier)>,
    ) -> Option<Target> {
        for (ns, v) in identity.keys() {
            let mine = self.authority.assertion(route.channel, ns);
            let Some(holders) = self.by_key.get(&(ns, v.clone())) else { continue };
            for &(sid, theirs) in holders {
                let sid = self.resolve(sid);
                let tier = if ns == Namespace::Url { MatchTier::CanonicalUrl } else { MatchTier::Identifier(ns) };
                if ns == Namespace::Url || confirmed(mine, theirs) {
                    let fold = (mine == Assertion::Authoritative && ns != Namespace::Url).then(|| (ns, v.clone()));
                    return Some((sid, tier, fold));
                }
                partner.get_or_insert((sid, tier));
            }
        }
        None
    }

    /// How curated a channel's metadata is: its strongest assertion over identifier namespaces.
    /// A title match merges only between routes that are both at least `Indexed`.
    fn curation(&self, channel: ChannelId) -> Assertion {
        const IDS: [Namespace; 8] = [
            Namespace::Doi,
            Namespace::Arxiv,
            Namespace::Pmid,
            Namespace::Pmcid,
            Namespace::S2,
            Namespace::OpenAlex,
            Namespace::Qid,
            Namespace::Repo,
        ];
        IDS.iter().map(|ns| self.authority.assertion(channel, *ns)).max().unwrap_or(Assertion::Asserted)
    }

    fn index(&mut self, sid: SourceId, route: &SourceRef, identity: &Identity) {
        for (ns, v) in identity.keys() {
            let a = self.authority.assertion(route.channel, ns);
            let holders = self.by_key.entry((ns, v)).or_default();
            match holders.iter_mut().find(|(s, _)| *s == sid) {
                Some(h) => h.1 = h.1.max(a),
                None => holders.push((sid, a)),
            }
        }
        if let Some(author) = first_author_surname(&route.authors) {
            let t = normalize_title(&route.title);
            if !t.is_empty() {
                let a = self.curation(route.channel);
                self.by_title.entry((t, author)).or_default().push((sid, route.year(), a));
            }
        }
    }

    fn merge(&mut self, sid: SourceId, route: SourceRef, identity: Identity, tier: MatchTier) -> Placement {
        self.index(sid, &route, &identity);
        let s = &mut self.sources[sid.0 as usize];
        s.identity.absorb(&identity);
        s.retracted |= route.retracted;
        s.routes.push(route);
        s.conflicts = conflicts(&s.routes);
        Placement { source: sid, resolution: Resolution::Same, tier: Some(tier), partner: None }
    }

    /// The authority just confirmed `(ns, v)` on `canon`: every other source holding it from a
    /// curated index is the same work. Their routes move to `canon`; they become aliases.
    fn fold_holders(&mut self, canon: SourceId, ns: Namespace, v: &str) {
        let holders = self.by_key.get(&(ns, v.to_string())).cloned().unwrap_or_default();
        for (sid, a) in holders {
            let sid = self.resolve(sid);
            if sid == canon || a < Assertion::Indexed {
                continue;
            }
            let (moved, old_identity, old_retracted) = {
                let s = &mut self.sources[sid.0 as usize];
                s.alias_of = Some(canon);
                s.conflicts.clear();
                (std::mem::take(&mut s.routes), s.identity.clone(), s.retracted)
            };
            for r in &moved {
                let id = r.effective_identity();
                self.index(canon, r, &id);
            }
            let c = &mut self.sources[canon.0 as usize];
            c.identity.absorb(&old_identity);
            c.retracted |= old_retracted;
            c.routes.extend(moved);
            c.conflicts = conflicts(&c.routes);
            for pair in &mut self.unresolved {
                for x in [&mut pair.0, &mut pair.1] {
                    if *x == sid {
                        *x = canon;
                    }
                }
            }
        }
        self.unresolved.retain(|(a, b)| a != b);
        let mut seen = std::collections::HashSet::new();
        self.unresolved.retain(|p| seen.insert((p.0.min(p.1), p.0.max(p.1))));
    }

    /// The live source `sid` now stands for (itself unless it was folded into another).
    pub fn resolve(&self, mut sid: SourceId) -> SourceId {
        while let Some(next) = self.sources.get(sid.0 as usize).and_then(|s| s.alias_of) {
            sid = next;
        }
        sid
    }

    /// Records that a route reported a retraction; it wins over every precedence rule.
    pub fn mark_retracted(&mut self, sid: SourceId) {
        let sid = self.resolve(sid);
        if let Some(s) = self.sources.get_mut(sid.0 as usize) {
            s.retracted = true;
        }
    }

    /// Every source ever created, aliases included (`alias_of` set, no routes). Most callers want
    /// [`Deduper::live`].
    pub fn sources(&self) -> &[Source] {
        &self.sources
    }

    /// The sources that stand for themselves.
    pub fn live(&self) -> impl Iterator<Item = &Source> {
        self.sources.iter().filter(|s| s.alias_of.is_none())
    }

    /// The source `sid` stands for, following aliases.
    pub fn get(&self, sid: SourceId) -> Option<&Source> {
        self.sources.get(self.resolve(sid).0 as usize)
    }

    /// Pairs dedup could not decide: count as one, display as two.
    pub fn unresolved(&self) -> &[(SourceId, SourceId)] {
        &self.unresolved
    }

    /// Identifiers two or more live sources share without the namespace's authority having
    /// confirmed them: look each up on the authoritative channel
    /// ([`crate::ChannelRegistry::lookup`]) and [`Deduper::add`] the answer.
    pub fn unconfirmed(&self) -> Vec<(Namespace, String)> {
        let mut out: Vec<(Namespace, String)> = self
            .by_key
            .iter()
            .filter(|((ns, _), _)| *ns != Namespace::Url)
            .filter(|(_, holders)| {
                let mut live: Vec<SourceId> = holders.iter().map(|(s, _)| self.resolve(*s)).collect();
                live.sort();
                live.dedup();
                live.len() >= 2 && holders.iter().all(|(_, a)| *a < Assertion::Authoritative)
            })
            .map(|(k, _)| k.clone())
            .collect();
        out.sort();
        out
    }

    /// Live sources.
    pub fn len(&self) -> usize {
        self.live().count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Field disagreements across a source's routes.
fn conflicts(routes: &[SourceRef]) -> Vec<Conflict> {
    let mut out = Vec::new();
    let mut field = |f: ConflictField, get: &dyn Fn(&SourceRef) -> Option<String>, norm: &dyn Fn(&str) -> String| {
        let vals: Vec<(ChannelId, String)> = routes.iter().filter_map(|r| get(r).map(|v| (r.channel, v))).collect();
        let mut distinct: Vec<String> = vals.iter().map(|(_, v)| norm(v)).collect();
        distinct.sort();
        distinct.dedup();
        if distinct.len() > 1 {
            out.push(Conflict { field: f, values: vals });
        }
    };
    field(ConflictField::Title, &|r| Some(r.title.clone()).filter(|t| !t.trim().is_empty()), &normalize_title);
    field(ConflictField::Year, &|r| r.year().map(|y| y.to_string()), &str::to_string);
    field(ConflictField::Doi, &|r| r.identity.as_ref().and_then(|i| i.doi.clone()), &str::to_string);
    field(ConflictField::FirstAuthor, &|r| first_author_surname(&r.authors), &str::to_string);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paper(channel: ChannelId, url: &str, id: Identity) -> SourceRef {
        SourceRef {
            identity: Some(id),
            authors: vec!["Edward J. Hu".into()],
            date: Some("2021-06-17".into()),
            ..SourceRef::new(channel, url, "LoRA: Low-Rank Adaptation of Large Language Models")
        }
    }

    #[test]
    fn the_same_paper_via_three_channels_is_one_source_with_three_routes() {
        let mut d = Deduper::default();
        let a = d.add(paper(
            ChannelId::Arxiv,
            "https://arxiv.org/abs/2106.09685v2",
            Identity::default().with_arxiv("2106.09685v2"),
        ));
        let s2 = Identity::default().with_arxiv("2106.09685").with_s2("abc123").with_doi("10.48550/arXiv.2106.09685");
        let b = d.add(paper(ChannelId::SemanticScholar, "https://www.semanticscholar.org/paper/abc123", s2));
        let oa = Identity::default().with_openalex("W3168867926").with_arxiv("arXiv:2106.09685");
        let c = d.add(paper(ChannelId::OpenAlex, "https://openalex.org/W3168867926", oa));
        assert_eq!(a.resolution, Resolution::Distinct);
        assert_eq!((b.resolution, b.tier), (Resolution::Same, Some(MatchTier::Identifier(Namespace::Arxiv))));
        assert_eq!((c.resolution, c.source), (Resolution::Same, a.source));
        assert_eq!(d.len(), 1);
        assert_eq!(d.sources()[0].routes.len(), 3);
        assert_eq!(d.sources()[0].identity.s2.as_deref(), Some("abc123"));
    }

    #[test]
    fn a_doi_printed_on_a_page_is_a_claim_and_stays_unresolved() {
        let mut d = Deduper::default();
        let real = d.add(paper(
            ChannelId::Crossref,
            "https://doi.org/10.1000/real",
            Identity::default().with_doi("10.1000/REAL"),
        ));
        let page = SourceRef {
            identity: Some(Identity::default().with_doi("10.1000/real")),
            ..SourceRef::new(ChannelId::Web, "https://blog.example.com/post", "An amazing result")
        };
        let p = d.add(page);
        assert_eq!((p.resolution, p.partner), (Resolution::Unresolved, Some(real.source)));
        assert_eq!(d.len(), 2, "displayed separately");
        assert_eq!(d.unresolved(), &[(real.source, p.source)]);
    }

    #[test]
    fn title_author_needs_the_year_and_curated_routes_to_merge() {
        let mut d = Deduper::default();
        let base = |ch, url: &str, date: &str| SourceRef {
            authors: vec!["Vaswani, Ashish".into()],
            date: Some(date.into()),
            ..SourceRef::new(ch, url, "Attention Is All You Need")
        };
        let a = d.add(base(ChannelId::Crossref, "https://doi.org/10.5555/x", "2017"));
        let b = d.add(base(ChannelId::OpenAlex, "https://openalex.org/W1", "2018-01-02"));
        assert_eq!((b.resolution, b.tier), (Resolution::Same, Some(MatchTier::TitleAuthorYear)));
        let c = d.add(base(ChannelId::SemanticScholar, "https://s2.org/p/9", "2023"));
        assert_eq!(c.resolution, Resolution::Unresolved, "years disagree");
        let e = d.add(base(ChannelId::Web, "https://medium.com/x", "2017"));
        assert_eq!(e.resolution, Resolution::Unresolved, "a page's title is a claim");
        assert_eq!(a.source, b.source);
    }

    #[test]
    fn canonical_urls_merge_and_a_negative_control_merges_nothing() {
        let mut d = Deduper::default();
        let a = d.add(SourceRef::new(
            ChannelId::HackerNews,
            "https://news.ycombinator.com/item?id=1&utm_source=x",
            "Show HN",
        ));
        let b = d.add(SourceRef::new(ChannelId::Web, "http://news.ycombinator.com/item?id=1", "Show HN | Hacker News"));
        assert_eq!((b.resolution, b.source, b.tier), (Resolution::Same, a.source, Some(MatchTier::CanonicalUrl)));
        assert_eq!(d.sources()[0].conflicts.len(), 1, "titles disagree and the conflict is kept");

        let mut n = Deduper::default();
        let titles = ["Deep Learning", "Deep Learning: A Survey", "Introduction", "Attention"];
        for (i, t) in titles.iter().enumerate() {
            let r = SourceRef {
                authors: vec![format!("Author{i}")],
                ..SourceRef::new(ChannelId::Crossref, &format!("https://example.org/p/{i}"), t)
            };
            assert_eq!(n.add(r).resolution, Resolution::Distinct);
        }
        assert_eq!(n.len(), 4);
        assert!(n.unresolved().is_empty());

        // Found live: an arXiv paper and its HTML page picked off a link table are one source.
        let mut x = Deduper::default();
        let paper = x.add(SourceRef {
            identity: Some(Identity::default().with_arxiv("2601.13580").with_url("https://arxiv.org/abs/2601.13580")),
            ..SourceRef::new(ChannelId::Arxiv, "https://arxiv.org/abs/2601.13580", "Neural Organ Transplantation")
        });
        let html = x.add(SourceRef::new(ChannelId::Web, "https://arxiv.org/html/2601.13580v1", "HTML (experimental)"));
        assert_eq!((html.resolution, html.source), (Resolution::Same, paper.source));
        assert_eq!(x.len(), 1);
    }
    #[test]
    fn two_indexes_sharing_a_doi_wait_for_the_authority_then_fold() {
        let mut d = Deduper::default();
        let doi = "10.1145/3292500.3330701";
        let s2 = d.add(paper(ChannelId::SemanticScholar, "https://s2.org/p/1", Identity::default().with_doi(doi)));
        let oa = SourceRef {
            date: Some("2025".into()),
            ..paper(ChannelId::OpenAlex, "https://openalex.org/W9", Identity::default().with_doi(doi))
        };
        let o = d.add(oa);
        let page = d.add(SourceRef {
            identity: Some(Identity::default().with_doi(doi)),
            ..SourceRef::new(ChannelId::Web, "https://blog.example.com/x", "Blog")
        });
        assert_eq!(o.resolution, Resolution::Unresolved, "two indexes are not the authority");
        assert_eq!(d.unconfirmed(), vec![(Namespace::Doi, doi.to_string())]);
        assert_eq!(d.len(), 3);

        let mut cr = paper(ChannelId::Crossref, &format!("https://doi.org/{doi}"), Identity::default().with_doi(doi));
        cr.retracted = true;
        let c = d.add(cr);
        assert_eq!((c.resolution, c.source), (Resolution::Same, s2.source));
        assert_eq!(d.len(), 2, "OpenAlex folded in; the page stays a claim");
        assert_eq!(d.resolve(o.source), s2.source);
        assert_eq!(d.get(o.source).unwrap().routes.len(), 3);
        assert!(d.get(s2.source).unwrap().retracted, "a retraction from any route wins");
        assert_eq!(d.unresolved(), &[(s2.source, page.source)]);
        assert!(d.unconfirmed().is_empty());
        assert!(d.get(s2.source).unwrap().conflicts.iter().any(|c| c.field == ConflictField::Year));
    }
}
