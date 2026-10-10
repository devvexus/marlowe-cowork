//! Claims with exact supporting quotes, grouped and counted over INDEPENDENT ROOTS of the
//! derivation graph (ADR-036 §4.5): three blogs quoting one paper are one root. Unresolved dedup
//! pairs collapse into one node for counting (§4.3).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::{Deserialize, Serialize};

use crate::dedup::SourceId;
use crate::links::DocRef;
use crate::source::TrustBucket;

/// A claim, numbered across the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ClaimId(pub u32);

/// Where a quote sits in a stored document: enough to re-read the exact passage.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Locator {
    pub doc: DocRef,
    /// Byte range into the stored document text.
    pub start: usize,
    pub end: usize,
    /// The nearest heading above the quote.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub section: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<u32>,
}

/// An exact supporting quote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quote {
    pub text: String,
    pub locator: Locator,
}

impl Quote {
    /// Whether `doc_text[start..end]` is this quote, byte for byte: the check that the
    /// compression role did not paraphrase.
    pub fn matches(&self, doc_text: &str) -> bool {
        doc_text.get(self.locator.start..self.locator.end) == Some(self.text.as_str())
    }

    /// Finds `quote` in `doc_text` and builds its locator; `None` when it is not verbatim.
    pub fn locate(doc: &DocRef, doc_text: &str, quote: &str) -> Option<Quote> {
        let q = quote.trim();
        if q.is_empty() {
            return None;
        }
        let start = doc_text.find(q)?;
        Some(Quote {
            text: q.to_string(),
            locator: Locator { doc: doc.clone(), start, end: start + q.len(), section: None, page: None },
        })
    }
}

/// One atomic claim a source makes, with its quote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claim {
    pub id: ClaimId,
    pub source: SourceId,
    /// The plan sub-question it answers.
    pub sub_question: u32,
    pub text: String,
    pub quote: Quote,
}

/// Does a claim back or contradict its group's statement?
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stance {
    Supports,
    Contradicts,
}

/// Claims that assert the same thing (or its negation).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimGroup {
    pub id: u32,
    pub statement: String,
    pub members: Vec<(ClaimId, Stance)>,
}

/// Derivation edges between sources (a blog post derives from the paper it quotes) plus the
/// unresolved dedup pairs that count as one.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DerivationGraph {
    /// (child, parent): `child` derives from `parent`.
    edges: BTreeSet<(SourceId, SourceId)>,
    same: BTreeSet<(SourceId, SourceId)>,
}

impl DerivationGraph {
    pub fn new() -> Self {
        Self::default()
    }

    /// `child` derives from `parent`. Self-edges are ignored.
    pub fn derives_from(&mut self, child: SourceId, parent: SourceId) {
        if child != parent {
            self.edges.insert((child, parent));
        }
    }

    /// `a` and `b` may be the same work: one node for counting.
    pub fn unresolved(&mut self, a: SourceId, b: SourceId) {
        if a != b {
            self.same.insert((a.min(b), a.max(b)));
        }
    }

    /// Unresolved pairs collapsed: each source maps to its smallest partner.
    fn canon(&self) -> impl Fn(SourceId) -> SourceId {
        fn find(p: &HashMap<SourceId, SourceId>, mut x: SourceId) -> SourceId {
            while let Some(&n) = p.get(&x) {
                x = n;
            }
            x
        }
        let mut parent: HashMap<SourceId, SourceId> = HashMap::new();
        for &(a, b) in &self.same {
            let (ra, rb) = (find(&parent, a), find(&parent, b));
            if ra != rb {
                parent.insert(ra.max(rb), ra.min(rb));
            }
        }
        let resolved: HashMap<SourceId, SourceId> = parent.keys().map(|&k| (k, find(&parent, k))).collect();
        move |x| *resolved.get(&x).unwrap_or(&x)
    }

    /// The roots `s` ultimately derives from (itself when it derives from nothing), after
    /// collapsing unresolved pairs. Cycles terminate.
    pub fn roots_of(&self, s: SourceId) -> BTreeSet<SourceId> {
        let canon = self.canon();
        let mut parents: BTreeMap<SourceId, BTreeSet<SourceId>> = BTreeMap::new();
        for &(c, p) in &self.edges {
            let (c, p) = (canon(c), canon(p));
            if c != p {
                parents.entry(c).or_default().insert(p);
            }
        }
        let mut roots = BTreeSet::new();
        let mut seen = BTreeSet::new();
        let mut stack = vec![canon(s)];
        while let Some(n) = stack.pop() {
            if !seen.insert(n) {
                continue;
            }
            match parents.get(&n) {
                Some(ps) if !ps.is_empty() => stack.extend(ps.iter().copied()),
                _ => {
                    roots.insert(n);
                }
            }
        }
        if roots.is_empty() {
            // A pure cycle: the smallest member stands for it.
            roots.insert(*seen.iter().next().unwrap_or(&canon(s)));
        }
        roots
    }

    /// Groups of `sources` that share a root; sources in different groups are independent.
    pub fn root_groups(&self, sources: &[SourceId]) -> Vec<Vec<SourceId>> {
        let mut groups: Vec<(BTreeSet<SourceId>, Vec<SourceId>)> = Vec::new();
        let mut uniq: Vec<SourceId> = sources.to_vec();
        uniq.sort();
        uniq.dedup();
        for s in uniq {
            let roots = self.roots_of(s);
            let hits: Vec<usize> = (0..groups.len()).filter(|&i| !groups[i].0.is_disjoint(&roots)).collect();
            let mut merged = (roots, vec![s]);
            for &i in hits.iter().rev() {
                let (r, m) = groups.remove(i);
                merged.0.extend(r);
                merged.1.extend(m);
            }
            merged.1.sort();
            groups.push(merged);
        }
        let mut out: Vec<Vec<SourceId>> = groups.into_iter().map(|g| g.1).collect();
        out.sort();
        out
    }

    /// How many independent roots back `sources`.
    pub fn independent_roots(&self, sources: &[SourceId]) -> usize {
        self.root_groups(sources).len()
    }
}

/// How a claim group stands after corroboration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Standing {
    /// Two or more independent roots, none contradicting.
    Established,
    /// One root, none contradicting.
    Suggested,
    /// Independent roots on both sides.
    Contested,
    /// Only the user's memory states it.
    Remembered,
    /// No source states it; the synthesis infers it.
    Inferred,
}

/// A claim group's corroboration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Corroboration {
    pub group: u32,
    /// Independent fetched roots supporting the statement.
    pub supporting_roots: usize,
    pub contradicting_roots: usize,
    /// Memory sources supporting it, counted apart (their own trust bucket).
    pub memory_support: usize,
    pub standing: Standing,
}

/// Corroborates every group. `claims` must hold every member; `bucket` says which sources are
/// the user's memory.
pub fn corroborate(
    groups: &[ClaimGroup],
    claims: &[Claim],
    graph: &DerivationGraph,
    bucket: &dyn Fn(SourceId) -> TrustBucket,
) -> Vec<Corroboration> {
    let by_id: HashMap<ClaimId, &Claim> = claims.iter().map(|c| (c.id, c)).collect();
    groups
        .iter()
        .map(|g| {
            let side = |stance: Stance, memory: bool| -> Vec<SourceId> {
                g.members
                    .iter()
                    .filter(|(_, s)| *s == stance)
                    .filter_map(|(id, _)| by_id.get(id).map(|c| c.source))
                    .filter(|s| (bucket(*s) == TrustBucket::Memory) == memory)
                    .collect()
            };
            let supporting_roots = graph.independent_roots(&side(Stance::Supports, false));
            let contradicting_roots = graph.independent_roots(&side(Stance::Contradicts, false));
            let memory_support = graph.independent_roots(&side(Stance::Supports, true));
            let standing = match (supporting_roots, contradicting_roots) {
                (s, c) if s > 0 && c > 0 => Standing::Contested,
                (s, _) if s >= 2 => Standing::Established,
                (1, _) => Standing::Suggested,
                (0, c) if c > 0 => Standing::Contested,
                _ if memory_support > 0 => Standing::Remembered,
                _ => Standing::Inferred,
            };
            Corroboration { group: g.id, supporting_roots, contradicting_roots, memory_support, standing }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAPER: SourceId = SourceId(0);

    #[test]
    fn three_blogs_quoting_one_paper_are_one_root() {
        let mut g = DerivationGraph::new();
        for b in 1..=3 {
            g.derives_from(SourceId(b), PAPER);
        }
        assert_eq!(g.independent_roots(&[SourceId(1), SourceId(2), SourceId(3)]), 1);
        assert_eq!(g.independent_roots(&[PAPER, SourceId(1)]), 1, "a source and its derivative are one root");
        assert_eq!(g.independent_roots(&[PAPER, SourceId(9)]), 2);
        assert_eq!(
            g.root_groups(&[SourceId(9), SourceId(2), PAPER]),
            vec![vec![PAPER, SourceId(2)], vec![SourceId(9)]]
        );
    }

    #[test]
    fn unresolved_pairs_count_once_and_cycles_terminate() {
        let mut g = DerivationGraph::new();
        g.unresolved(SourceId(4), SourceId(5));
        assert_eq!(g.independent_roots(&[SourceId(4), SourceId(5)]), 1);
        g.derives_from(SourceId(6), SourceId(7));
        g.derives_from(SourceId(7), SourceId(6));
        assert_eq!(g.independent_roots(&[SourceId(6), SourceId(7)]), 1);
        // A shared ancestor two levels up still collapses.
        g.derives_from(SourceId(10), SourceId(11));
        g.derives_from(SourceId(11), SourceId(12));
        g.derives_from(SourceId(13), SourceId(12));
        assert_eq!(g.independent_roots(&[SourceId(10), SourceId(13)]), 1);
    }

    fn claim(id: u32, source: u32) -> Claim {
        let doc = DocRef::of(b"doc");
        Claim {
            id: ClaimId(id),
            source: SourceId(source),
            sub_question: 1,
            text: "rank 8 matches rank 64".into(),
            quote: Quote::locate(&doc, "we find rank 8 matches rank 64 here", "rank 8 matches rank 64").unwrap(),
        }
    }

    #[test]
    fn standing_follows_roots_and_memory_is_counted_apart() {
        let mut graph = DerivationGraph::new();
        graph.derives_from(SourceId(2), SourceId(1));
        let claims: Vec<Claim> = [(1, 1), (2, 2), (3, 3), (4, 4), (5, 50)].iter().map(|&(c, s)| claim(c, s)).collect();
        let groups = vec![
            ClaimGroup {
                id: 1,
                statement: "s".into(),
                members: vec![(ClaimId(1), Stance::Supports), (ClaimId(2), Stance::Supports)],
            },
            ClaimGroup {
                id: 2,
                statement: "s".into(),
                members: vec![(ClaimId(1), Stance::Supports), (ClaimId(3), Stance::Supports)],
            },
            ClaimGroup {
                id: 3,
                statement: "s".into(),
                members: vec![(ClaimId(3), Stance::Supports), (ClaimId(4), Stance::Contradicts)],
            },
            ClaimGroup { id: 4, statement: "s".into(), members: vec![(ClaimId(5), Stance::Supports)] },
        ];
        let bucket = |s: SourceId| if s.0 >= 50 { TrustBucket::Memory } else { TrustBucket::Untrusted };
        let c = corroborate(&groups, &claims, &graph, &bucket);
        let st: Vec<Standing> = c.iter().map(|x| x.standing).collect();
        assert_eq!(st, [Standing::Suggested, Standing::Established, Standing::Contested, Standing::Remembered]);
        assert_eq!(c[0].supporting_roots, 1, "a source and its derivative");
        assert_eq!(c[3].memory_support, 1);
    }

    #[test]
    fn quotes_are_verbatim_or_absent() {
        let doc = DocRef::of(b"x");
        let text = "Alpha. We find that rank 8 suffices. Omega.";
        let q = Quote::locate(&doc, text, " We find that rank 8 suffices. ").unwrap();
        assert!(q.matches(text));
        assert_eq!(&text[q.locator.start..q.locator.end], "We find that rank 8 suffices.");
        assert!(Quote::locate(&doc, text, "we find that rank eight suffices").is_none());
        assert!(!q.matches("different text entirely, long enough to index into"));
    }
}
