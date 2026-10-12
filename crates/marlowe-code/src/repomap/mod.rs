//! The ranked, budgeted repo map (design 3.3, 2.1 T27): the definitions that matter most in a
//! repository, as signature lines grouped by file, cut to a token budget (1.2k at a 32k context,
//! 4k for frontier models; [`budget_for`]).
//!
//! Ranking is PageRank over files: a file that references a name defined in another file votes
//! for it, weighted by how often, and damped for names many files define (`new`, `get`) and for
//! private-looking ones. Focus files (the ones a session touched) get the teleport mass, so the
//! map leans toward their neighbourhood. Each file's rank then flows to the definitions its
//! referrers used. The cut is a binary search on how many top definitions fit the budget, so
//! the map never exceeds it.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::index::{Index, Symbol};

/// The definition kinds a map shows (fields, variants and locals are noise at this altitude).
const SHOWN: &[&str] = &["class", "interface", "function", "method", "module", "macro", "constant", "type"];

/// Names defined in more files than this carry no signal about which file is meant.
const MAX_DEFINERS: usize = 12;

/// Rough tokens of `text`, as the agent's context meter counts them (about three characters per
/// token), so a budget here means the same as in the prefix budget.
pub fn estimate_tokens(text: &str) -> usize {
    text.chars().count().div_ceil(3)
}

/// The map budget for a model with `context_tokens` (design 3.4): 1.2k up to 32k, 2k up to 128k,
/// 4k above.
pub fn budget_for(context_tokens: u32) -> usize {
    match context_tokens {
        0..=32_768 => 1_200,
        32_769..=131_072 => 2_000,
        _ => 4_000,
    }
}

/// What to map.
#[derive(Debug, Clone, Default)]
pub struct MapOptions {
    /// Token budget for the whole map.
    pub budget_tokens: usize,
    /// Absolute paths the session is working in; they and their neighbours rank higher.
    pub focus: Vec<PathBuf>,
    /// Names the user or model mentioned; their definitions rank higher.
    pub mentioned: Vec<String>,
    /// Only files under this directory (absolute); `None` maps the whole index.
    pub under: Option<PathBuf>,
}

/// One ranked definition.
#[derive(Debug, Clone)]
struct Ranked {
    sym: Symbol,
    rank: f64,
}

/// The rendered map, at most `opts.budget_tokens`; empty when the index has no definitions.
pub fn render(index: &Index, opts: &MapOptions) -> String {
    let (defs, refs) = index.graph();
    let under = opts.under.as_deref().map(crate::index::norm_key);
    let keep = |p: &Path| {
        under.as_deref().is_none_or(|u| {
            let k = crate::index::norm_key(p);
            k == u || (k.starts_with(u) && k.as_bytes().get(u.len()) == Some(&b'/'))
        })
    };
    let defs: Vec<Symbol> = defs.into_iter().filter(|d| SHOWN.contains(&d.kind.as_str()) && keep(&d.path)).collect();
    let refs: Vec<(PathBuf, String, u32)> = refs.into_iter().filter(|(p, ..)| keep(p)).collect();
    let ranked = rank(&defs, &refs, opts);
    fit(index.root(), &ranked, opts.budget_tokens)
}

/// PageRank over files, then each file's rank shared out to the definitions its referrers used.
fn rank(defs: &[Symbol], refs: &[(PathBuf, String, u32)], opts: &MapOptions) -> Vec<Ranked> {
    // Files are nodes.
    let mut ids: HashMap<&Path, usize> = HashMap::new();
    let mut files: Vec<&Path> = Vec::new();
    for p in defs.iter().map(|d| d.path.as_path()).chain(refs.iter().map(|r| r.0.as_path())) {
        if !ids.contains_key(p) {
            ids.insert(p, files.len());
            files.push(p);
        }
    }
    let n = files.len();
    if n == 0 {
        return Vec::new();
    }

    let mut definers: HashMap<&str, HashSet<usize>> = HashMap::new();
    for d in defs {
        definers.entry(d.name.as_str()).or_default().insert(ids[d.path.as_path()]);
    }
    let mentioned: HashSet<&str> = opts.mentioned.iter().map(String::as_str).collect();

    // Edges: referencing file -> defining file, per name.
    let mut edges: Vec<(usize, usize, f64, &str)> = Vec::new();
    for (path, name, count) in refs {
        let Some(defs_of) = definers.get(name.as_str()) else { continue };
        if defs_of.len() > MAX_DEFINERS {
            continue;
        }
        let from = ids[path.as_path()];
        let mut w = (*count as f64).sqrt() / defs_of.len() as f64;
        if name.starts_with('_') || name.len() < 3 {
            w *= 0.1;
        }
        if mentioned.contains(name.as_str()) {
            w *= 10.0;
        }
        for &to in defs_of {
            if to != from {
                edges.push((from, to, w, name.as_str()));
            }
        }
    }

    // Personalisation: focus files get the teleport mass when there are any.
    let focus: HashSet<usize> = opts
        .focus
        .iter()
        .filter_map(|f| {
            let k = crate::index::norm_key(f);
            files.iter().position(|p| crate::index::norm_key(p) == k)
        })
        .collect();
    let teleport: Vec<f64> = if focus.is_empty() {
        vec![1.0 / n as f64; n]
    } else {
        (0..n).map(|i| if focus.contains(&i) { 1.0 / focus.len() as f64 } else { 0.0 }).collect()
    };

    let mut out_w = vec![0.0; n];
    for &(from, _, w, _) in &edges {
        out_w[from] += w;
    }
    let damping = 0.85;
    let mut pr = teleport.clone();
    for _ in 0..40 {
        let mut next: Vec<f64> = teleport.iter().map(|t| (1.0 - damping) * t).collect();
        let dangling: f64 = (0..n).filter(|&i| out_w[i] == 0.0).map(|i| pr[i]).sum();
        for i in 0..n {
            next[i] += damping * dangling * teleport[i];
        }
        for &(from, to, w, _) in &edges {
            next[to] += damping * pr[from] * w / out_w[from];
        }
        let delta: f64 = next.iter().zip(&pr).map(|(a, b)| (a - b).abs()).sum();
        pr = next;
        if delta < 1e-9 {
            break;
        }
    }

    // Each edge carries rank to the definitions of its name in the target file.
    let mut def_rank: HashMap<(usize, &str), f64> = HashMap::new();
    for &(from, to, w, name) in &edges {
        *def_rank.entry((to, name)).or_default() += pr[from] * w / out_w[from];
    }
    let mut ranked: Vec<Ranked> = defs
        .iter()
        .map(|d| {
            let f = ids[d.path.as_path()];
            let used = def_rank.get(&(f, d.name.as_str())).copied().unwrap_or(0.0);
            // Unreferenced definitions still order by their file's rank, well below used ones.
            let mut rank = used + pr[f] * 1e-3;
            if mentioned.contains(d.name.as_str()) {
                rank *= 10.0;
            }
            if focus.contains(&f) {
                rank += pr[f] * 1e-2;
            }
            Ranked { sym: d.clone(), rank }
        })
        .collect();
    ranked.sort_by(|a, b| {
        b.rank
            .partial_cmp(&a.rank)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.sym.path.cmp(&b.sym.path))
            .then_with(|| a.sym.line.cmp(&b.sym.line))
    });
    ranked
}

/// The largest prefix of `ranked` whose rendering fits `budget` tokens (binary search).
fn fit(root: &Path, ranked: &[Ranked], budget: usize) -> String {
    if ranked.is_empty() || budget == 0 {
        return String::new();
    }
    let (mut lo, mut hi) = (0usize, ranked.len());
    let mut best = String::new();
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let text = draw(root, &ranked[..mid]);
        if estimate_tokens(&text) <= budget {
            best = text;
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    best
}

/// Files in order of their best definition, each with its definitions in line order:
///
/// ```text
/// src/engine.rs
///   12│ pub struct Engine {
///   40│ pub fn start(&self) -> Result<()> {
/// ```
fn draw(root: &Path, top: &[Ranked]) -> String {
    let mut order: Vec<&Path> = Vec::new();
    let mut by_file: HashMap<&Path, Vec<&Symbol>> = HashMap::new();
    for r in top {
        let p = r.sym.path.as_path();
        if !by_file.contains_key(p) {
            order.push(p);
        }
        by_file.entry(p).or_default().push(&r.sym);
    }
    let mut out = String::new();
    for p in order {
        let rel = p.strip_prefix(root).unwrap_or(p).to_string_lossy().replace('\\', "/");
        out.push_str(&rel);
        out.push('\n');
        let mut syms = by_file.remove(p).unwrap_or_default();
        syms.sort_by_key(|s| s.line);
        syms.dedup_by_key(|s| s.line);
        for s in syms {
            let sig = if s.sig.is_empty() { s.name.as_str() } else { s.sig.as_str() };
            out.push_str(&format!("{:>5}│ {}\n", s.line, sig.trim_end_matches('{').trim_end()));
        }
    }
    out
}

#[cfg(all(test, not(target_arch = "aarch64")))]
mod tests {
    use marlowe_os::workspace::LocalFs;

    use super::*;
    use crate::index::tests::Tmp;

    fn fixture() -> (Tmp, Index) {
        let t = Tmp::new("repomap");
        t.put(
            "src/core.rs",
            "pub struct Engine { n: u32 }\nimpl Engine {\n    pub fn start(&self) -> u32 { self.n }\n    pub fn stop(&self) {}\n}\npub fn shared_helper() {}\n",
        );
        for i in 0..6 {
            t.put(
                &format!("src/user{i}.rs"),
                &format!(
                    "use crate::core::Engine;\npub fn run{i}(e: &Engine) -> u32 {{ shared_helper(); e.start() }}\n"
                ),
            );
        }
        t.put("src/lonely.rs", "pub fn nobody_calls_me() {}\npub struct Unused;\n");
        let idx = Index::in_memory(&t.0).unwrap();
        idx.refresh(&LocalFs).unwrap();
        (t, idx)
    }

    #[test]
    fn parity_t27_repo_map_respects_its_budget_and_ranks_the_hub_first() {
        let (_t, idx) = fixture();
        let full = render(&idx, &MapOptions { budget_tokens: 100_000, ..Default::default() });
        assert!(full.starts_with("src/core.rs\n"), "the most referenced file leads:\n{full}");
        assert!(full.contains("pub struct Engine"), "{full}");
        assert!(full.contains("nobody_calls_me"), "a large budget shows everything:\n{full}");
        for budget in [10, 25, 40, 60, 120, 400] {
            let map = render(&idx, &MapOptions { budget_tokens: budget, ..Default::default() });
            assert!(estimate_tokens(&map) <= budget, "budget {budget}: {} tokens\n{map}", estimate_tokens(&map));
        }
        let small = render(&idx, &MapOptions { budget_tokens: 25, ..Default::default() });
        assert!(small.contains("Engine") || small.contains("start"), "{small}");
        assert!(!small.contains("nobody_calls_me"), "unreferenced definitions go first:\n{small}");
        assert!(render(&idx, &MapOptions { budget_tokens: 0, ..Default::default() }).is_empty());
    }

    #[test]
    fn focus_and_mentions_pull_their_definitions_in() {
        let (t, idx) = fixture();
        let opts = MapOptions {
            budget_tokens: 30,
            focus: vec![t.0.join("src").join("lonely.rs")],
            mentioned: vec!["nobody_calls_me".into()],
            under: None,
        };
        let map = render(&idx, &opts);
        assert!(map.contains("nobody_calls_me"), "{map}");
        let under = render(
            &idx,
            &MapOptions { budget_tokens: 1000, under: Some(t.0.join("src").join("lonely.rs")), ..Default::default() },
        );
        assert!(under.starts_with("src/lonely.rs") && !under.contains("core.rs"), "{under}");
    }

    #[test]
    fn budgets_follow_the_context_window() {
        assert_eq!(budget_for(32_768), 1_200);
        assert_eq!(budget_for(65_536), 2_000);
        assert_eq!(budget_for(200_000), 4_000);
    }
}
