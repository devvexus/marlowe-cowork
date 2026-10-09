//! Metric/result JSON: flatten numeric leaves, compare with the most similar sibling, best-so-far.

use std::path::Path;

/// Which way is better for a metric. Only claimed for names the heuristics recognise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    /// loss, kl, perplexity/ppl, error/err, nll, mse, ...
    Lower,
    /// acc/accuracy, top1/top5, retained, f1, score, ...
    Higher,
}

const LOWER: &[&str] = &[
    "loss", "kl", "kld", "perplexity", "ppl", "error", "err", "errors", "wer", "cer", "mse", "mae", "rmse", "nll", "bpb",
    "bpc", "regret", "latency",
];
const HIGHER: &[&str] = &[
    "acc", "accuracy", "retained", "retention", "f1", "score", "precision", "recall", "auc", "auroc", "bleu", "em",
    "exact", "pass", "winrate", "reward", "ndcg", "mrr", "hit", "hits",
];

/// The direction for a (possibly dotted) metric name, judged on its last component.
/// `None` for names the heuristics do not know (`unigram`, `lr`, `step`): never guess.
pub fn direction(name: &str) -> Option<Direction> {
    let last = name.rsplit(['.', '/']).next().unwrap_or(name).to_ascii_lowercase();
    let mut lower = false;
    let mut higher = false;
    for tok in last.split(|c: char| !c.is_ascii_alphanumeric()).filter(|t| !t.is_empty()) {
        // `top1`, `top5`, `acc1`, `rouge1`, `rougeL`.
        let stem = tok.trim_end_matches(|c: char| c.is_ascii_digit());
        lower |= LOWER.contains(&tok) || LOWER.contains(&stem);
        higher |= HIGHER.contains(&tok)
            || HIGHER.contains(&stem)
            || (stem == "top" && stem.len() < tok.len())
            || tok.starts_with("rouge");
    }
    match (lower, higher) {
        (true, false) => Some(Direction::Lower),
        (false, true) => Some(Direction::Higher),
        _ => None,
    }
}

/// Flat `(dotted.path, value)` pairs for every finite numeric leaf, in document order.
/// `{"thinking":{"loss":1.218}}` -> `[("thinking.loss", 1.218)]`; array items are `name.0`.
pub fn flatten(v: &serde_json::Value) -> Vec<(String, f64)> {
    fn walk(v: &serde_json::Value, path: &mut String, out: &mut Vec<(String, f64)>) {
        let mut push = |key: &str, child: &serde_json::Value, path: &mut String| {
            let len = path.len();
            if !path.is_empty() {
                path.push('.');
            }
            path.push_str(key);
            walk(child, path, out);
            path.truncate(len);
        };
        match v {
            serde_json::Value::Number(n) => {
                if let Some(x) = n.as_f64().filter(|x| x.is_finite()) {
                    out.push((path.clone(), x));
                }
            }
            serde_json::Value::Object(m) => m.iter().for_each(|(k, c)| push(k, c, path)),
            serde_json::Value::Array(a) => a.iter().enumerate().for_each(|(i, c)| push(&i.to_string(), c, path)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(v, &mut String::new(), &mut out);
    out
}

/// Python's `json.dump` writes bare `NaN`, `Infinity` and `-Infinity`, which strict JSON refuses.
/// Rewrites them outside strings as the strings `"NaN"` / `"Infinity"` / `"-Infinity"` and
/// returns how many were found.
pub fn sanitize_non_finite(text: &str) -> (String, usize) {
    let mut out = String::with_capacity(text.len() + 16);
    let mut count = 0;
    let mut in_str = false;
    let mut escaped = false;
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        if in_str {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_str = false;
            }
            rest = &rest[c.len_utf8()..];
            continue;
        }
        let token = ["-Infinity", "Infinity", "NaN"].into_iter().find(|t| rest.starts_with(t));
        if let Some(t) = token {
            out.push('"');
            out.push_str(t);
            out.push('"');
            count += 1;
            rest = &rest[t.len()..];
            continue;
        }
        if c == '"' {
            in_str = true;
        }
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    (out, count)
}

/// Whether a path is a candidate metric file: `*.json`, not under `node_modules` or `.git`,
/// at most `max_len` bytes, and not `heartbeat.json`/`trainer_state.json`/`state.json`
/// (handled elsewhere).
pub fn is_metric_file(path: &Path, len: u64, max_len: u64) -> bool {
    let Some(name) = path.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase()) else { return false };
    let excluded = path.components().any(|c| {
        let c = c.as_os_str().to_string_lossy();
        c == "node_modules" || c == ".git"
    });
    name.ends_with(".json")
        && !matches!(name.as_str(), "heartbeat.json" | "trainer_state.json" | "state.json")
        && !excluded
        && len > 0
        && len <= max_len
}

/// A file stem split into its family and trailing counter:
/// `audit3_marlowe-valiance-22b-wnr4` -> `("audit3_marlowe-valiance-22b-wnr", Some(4))`.
pub fn family(stem: &str) -> (String, Option<u64>) {
    let head = stem.trim_end_matches(|c: char| c.is_ascii_digit());
    let digits = &stem[head.len()..];
    match digits.parse::<u64>() {
        Ok(n) if !head.is_empty() => (head.to_string(), Some(n)),
        _ => (stem.to_string(), None),
    }
}

/// The most similar sibling stem to `stem` among `siblings`: same family with the nearest lower
/// counter first (else the nearest higher), else the longest shared prefix in the same
/// directory when it covers most of both names. `None` when nothing is similar enough.
pub fn most_similar<'a>(stem: &str, siblings: &'a [String]) -> Option<&'a str> {
    let (fam, n) = family(stem);
    let others = || siblings.iter().map(String::as_str).filter(move |s| *s != stem);
    if let Some(n) = n {
        let same: Vec<(u64, &str)> = others().filter_map(|s| match family(s) {
            (f, Some(m)) if f == fam => Some((m, s)),
            _ => None,
        }).collect();
        let lower = same.iter().filter(|(m, _)| *m < n).max_by_key(|(m, _)| *m);
        let higher = same.iter().filter(|(m, _)| *m > n).min_by_key(|(m, _)| *m);
        if let Some((_, s)) = lower.or(higher) {
            return Some(s);
        }
    }
    let shared = |s: &str| stem.bytes().zip(s.bytes()).take_while(|(a, b)| a == b).count();
    others()
        .map(|s| (shared(s), s))
        .filter(|(p, s)| *p >= 4 && *p * 10 >= stem.len().max(s.len()) * 6)
        .max_by(|(pa, sa), (pb, sb)| pa.cmp(pb).then(sb.cmp(sa)))
        .map(|(_, s)| s)
}

/// One metric's change against the sibling.
#[derive(Debug, Clone, PartialEq)]
pub struct Delta {
    pub metric: String,
    pub before: f64,
    pub after: f64,
    pub direction: Option<Direction>,
    /// Better than every earlier value in the family (only when `direction` is known).
    pub best_so_far: bool,
}

impl Delta {
    /// True when the change is an improvement; `None` without a known direction.
    pub fn improved(&self) -> Option<bool> {
        self.direction.map(|d| match d {
            Direction::Lower => self.after < self.before,
            Direction::Higher => self.after > self.before,
        })
    }
}

/// Whether `a` beats `b` in direction `d`.
pub fn better(d: Direction, a: f64, b: f64) -> bool {
    match d {
        Direction::Lower => a < b,
        Direction::Higher => a > b,
    }
}

/// Deltas for metrics present in both; `best` gives the family's best so far per metric
/// (excluding `now`). Best-so-far needs a known direction and must beat both `before` and `best`.
pub fn compare(now: &[(String, f64)], before: &[(String, f64)], best: &dyn Fn(&str) -> Option<f64>) -> Vec<Delta> {
    now.iter()
        .filter_map(|(metric, after)| {
            let before = before.iter().find(|(m, _)| m == metric)?.1;
            let direction = direction(metric);
            let best_so_far = direction.is_some_and(|d| better(d, *after, before) && best(metric).is_none_or(|b| better(d, *after, b)));
            Some(Delta { metric: metric.clone(), before, after: *after, direction, best_so_far })
        })
        .collect()
}

/// A number, flat: four significant digits, trailing zeros dropped (`1.2101365` -> `1.21`,
/// `0.08030` -> `0.0803`, `250` -> `250`).
pub fn fmt_num(v: f64) -> String {
    if !v.is_finite() {
        return if v.is_nan() { "NaN".into() } else if v > 0.0 { "inf".into() } else { "-inf".into() };
    }
    if v == v.trunc() && v.abs() < 1e15 {
        return format!("{}", v as i64);
    }
    let mag = v.abs().log10().floor() as i32;
    let decimals = (3 - mag).clamp(0, 8) as usize;
    let s = format!("{v:.decimals$}");
    let s = if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s };
    if s == "-0" { "0".into() } else { s }
}

/// `+0.0079` / `-0.0024`.
pub fn fmt_signed(v: f64) -> String {
    if v >= 0.0 { format!("+{}", fmt_num(v)) } else { fmt_num(v) }
}

/// Metric names without the prefix every one of them shares (`thinking.loss` -> `loss` when all
/// metrics sit under `thinking`).
pub fn short_names(names: &[&str]) -> Vec<String> {
    let first = |n: &str| n.split_once('.').map(|(a, _)| a.to_string());
    let common = names.first().and_then(|n| first(n)).filter(|p| names.iter().all(|n| first(n).as_deref() == Some(p)));
    names
        .iter()
        .map(|n| match &common {
            Some(p) => n[p.len() + 1..].to_string(),
            None => n.to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn directions_only_for_known_names() {
        for n in ["loss", "eval_loss", "thinking.kl", "perplexity", "val/ppl", "error_rate", "train.nll"] {
            assert_eq!(direction(n), Some(Direction::Lower), "{n}");
        }
        for n in ["acc", "eval_accuracy", "thinking.top1", "top5", "retained", "f1", "score", "pass@1", "rougeL"] {
            assert_eq!(direction(n), Some(Direction::Higher), "{n}");
        }
        for n in ["unigram", "lr", "learning_rate", "step", "epoch", "t0", "attempts", "loss_acc"] {
            assert_eq!(direction(n), None, "{n}");
        }
    }

    #[test]
    fn flattens_numeric_leaves() {
        let v = json!({"thinking":{"loss":1.218,"kl":0.0803,"top1":0.875,"unigram":7.21,"retained":0.9905},"tag":"x","ok":true,"xs":[1,2]});
        let f = flatten(&v);
        let names: Vec<&str> = f.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"thinking.loss") && names.contains(&"xs.1"));
        assert_eq!(f.len(), 7);
        assert_eq!(f.iter().find(|(n, _)| n == "thinking.kl").unwrap().1, 0.0803);
    }

    #[test]
    fn sanitizes_python_non_finite() {
        let (s, n) = sanitize_non_finite(r#"{"loss": NaN, "x": -Infinity, "s": "NaN inside \" string", "y": Infinity}"#);
        assert_eq!(n, 3);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["loss"], "NaN");
        assert_eq!(v["s"], "NaN inside \" string");
    }

    #[test]
    fn families_and_siblings() {
        assert_eq!(family("audit3_marlowe-valiance-22b-wnr4"), ("audit3_marlowe-valiance-22b-wnr".into(), Some(4)));
        assert_eq!(family("results"), ("results".into(), None));
        assert_eq!(family("123"), ("123".into(), None));
        let sib: Vec<String> = ["audit3_marlowe-valiance-22b-refit1", "audit3_marlowe-valiance-22b-wnr2", "audit3_marlowe-valiance-22b-wnr3", "audit3_marlowe-valiance-22b-wnr5"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(most_similar("audit3_marlowe-valiance-22b-wnr4", &sib), Some("audit3_marlowe-valiance-22b-wnr3"));
        assert_eq!(most_similar("audit3_marlowe-valiance-22b-wnr1", &sib), Some("audit3_marlowe-valiance-22b-wnr2"));
        // No counter: the longest shared prefix, if it is long enough.
        let s2: Vec<String> = vec!["eval_results_base".into(), "queue".into()];
        assert_eq!(most_similar("eval_results_ft", &s2), Some("eval_results_base"));
        assert_eq!(most_similar("summary", &s2), None);
        assert!(is_metric_file(Path::new("a/b/audit3_x.json"), 100, 1 << 20));
        assert!(!is_metric_file(Path::new("a/node_modules/x.json"), 100, 1 << 20));
        assert!(!is_metric_file(Path::new("a/heartbeat.json"), 100, 1 << 20));
        assert!(!is_metric_file(Path::new("a/big.json"), 2 << 20, 1 << 20));
    }

    #[test]
    fn compares_and_flags_best_so_far() {
        let wnr3 = vec![("thinking.loss".to_string(), 1.2101), ("thinking.top1".to_string(), 0.8753), ("thinking.unigram".to_string(), 7.209)];
        let wnr4 = vec![("thinking.loss".to_string(), 1.218), ("thinking.top1".to_string(), 0.8760), ("thinking.unigram".to_string(), 7.21)];
        let best = |m: &str| match m {
            "thinking.loss" => Some(1.20),
            "thinking.top1" => Some(0.8755),
            _ => None,
        };
        let d = compare(&wnr4, &wnr3, &best);
        assert_eq!(d.len(), 3);
        assert_eq!((d[0].improved(), d[0].best_so_far), (Some(false), false));
        assert_eq!((d[1].improved(), d[1].best_so_far), (Some(true), true));
        assert_eq!((d[2].improved(), d[2].best_so_far), (None, false));
    }

    #[test]
    fn formats_numbers_flat() {
        assert_eq!(fmt_num(1.2101365725199382), "1.21");
        assert_eq!(fmt_num(0.08271126720743875), "0.08271");
        assert_eq!(fmt_num(0.0803), "0.0803");
        assert_eq!(fmt_num(250.0), "250");
        assert_eq!(fmt_num(123.456), "123.5");
        assert_eq!(fmt_signed(-0.0024), "-0.0024");
        assert_eq!(fmt_signed(0.0079), "+0.0079");
        assert_eq!(short_names(&["thinking.loss", "thinking.kl"]), vec!["loss", "kl"]);
        assert_eq!(short_names(&["a.loss", "b.kl"]), vec!["a.loss", "b.kl"]);
    }
}
