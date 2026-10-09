//! The brief's wording: what to call a run, how it ended, when, what its last line amounts to,
//! and how a result compares with its previous sibling. Pure; no I/O.

use chrono::{DateTime, Local, TimeZone};
use marlowe_proto::{RunInfo, RunStatus};
use marlowe_sentinel::logs::{self, Ending};
use marlowe_sentinel::metrics::{direction, Direction, ResultComparison};

use super::{capitalize, count_word, leaf, log_line_body, round_sig};

/// How a run that is no longer live ended, as the brief says it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Outcome {
    Finished,
    /// With a short reason when the last line gives one.
    Failed(Option<String>),
    /// Gone, and the log does not say how.
    Stopped,
}

impl Outcome {
    pub(super) fn verb(&self) -> &'static str {
        match self {
            Outcome::Finished => "finished",
            Outcome::Failed(_) => "failed",
            Outcome::Stopped => "stopped",
        }
    }
}

/// The Sentinel's status, corrected by the last line: a run the Sentinel could only call ended
/// may have said it shipped (or failed) in words its classifier did not know.
pub(super) fn outcome(r: &RunInfo) -> Outcome {
    let ending = r.last_line.as_deref().map(logs::ending).unwrap_or(Ending::Unknown);
    // A generic "an error" is no reason worth saying.
    let reason = |why: String| (why != "an error").then_some(why);
    match (r.status, ending) {
        // A failing last line wins over any status: never call a failed check a finish.
        (_, Ending::Failure(why)) => Outcome::Failed(reason(why)),
        (RunStatus::Failed, _) => Outcome::Failed(None),
        (RunStatus::Finished, _) | (_, Ending::Success) => Outcome::Finished,
        _ => Outcome::Stopped,
    }
}

/// A heartbeat-driven pipeline rather than a directory of logs.
pub(super) fn is_pipeline(r: &RunInfo) -> bool {
    r.heartbeat_ms.is_some() || r.name != leaf(&r.dir)
}

/// `22b`, `7b`, `1.5b`, `350m`.
fn is_size(t: &str) -> bool {
    let t = t.to_ascii_lowercase();
    let Some(num) = t.strip_suffix(['b', 'm', 'k']) else { return false };
    !num.is_empty() && num.chars().all(|c| c.is_ascii_digit() || c == '.') && num.starts_with(|c: char| c.is_ascii_digit())
}

/// A run name without the project prefix the root already says: `marlowe-valiance-22b-wnr-final`
/// under `valiance` -> `wnr-final`. Unchanged when the root's name is not in it.
pub(super) fn shorten(name: &str, root_leaf: &str) -> String {
    let mut starts = Vec::new();
    let mut at = 0;
    for tok in name.split(['-', '_']) {
        starts.push((at, tok));
        at += tok.len() + 1;
    }
    let Some(i) = starts.iter().position(|(_, t)| t.eq_ignore_ascii_case(root_leaf)) else { return name.to_string() };
    let mut j = i + 1;
    if starts.get(j).is_some_and(|(_, t)| is_size(t)) {
        j += 1;
    }
    match starts.get(j) {
        Some((offset, _)) => name[*offset..].to_string(),
        None => name.to_string(),
    }
}

/// `q4_k_s`, `iq4_xs`, `f16`, `bf16`, `int8`: a quantisation tag, said in capitals.
fn is_quant(t: &str) -> bool {
    let t = t.to_ascii_lowercase();
    let rest = t.strip_prefix("iq").or_else(|| t.strip_prefix('q'));
    rest.is_some_and(|r| r.starts_with(|c: char| c.is_ascii_digit()))
        || matches!(t.as_str(), "f16" | "bf16" | "f32" | "fp16" | "fp32" | "fp8" | "int4" | "int8")
}

fn ext_label(ext: &str) -> String {
    match ext.to_ascii_lowercase().as_str() {
        "safetensors" => "safetensors".into(),
        "pt" | "pth" | "ckpt" | "bin" => "checkpoint".into(),
        e if e.len() <= 4 => e.to_ascii_uppercase(),
        e => e.into(),
    }
}

/// `marlowe-valiance-22b-wnr-final-truewidth-q4_k_s.gguf` (run `marlowe-valiance-22b-wnr-final`,
/// short `wnr-final`) -> `wnr-final truewidth Q4_K_S GGUF`.
pub(super) fn artifact_phrase(file: &str, run_name: &str, short: &str, root_leaf: Option<&str>) -> String {
    let (stem, ext) = file.rsplit_once('.').unwrap_or((file, ""));
    let mut words: Vec<String> = Vec::new();
    let rest = match stem.strip_prefix(run_name).filter(|_| !run_name.is_empty()) {
        Some(rest) => {
            words.push(short.to_string());
            rest.trim_start_matches(['-', '_']).to_string()
        }
        None => root_leaf.map_or_else(|| stem.to_string(), |r| shorten(stem, r)),
    };
    for t in rest.split('-').filter(|t| !t.is_empty()) {
        words.push(if is_quant(t) { t.to_ascii_uppercase() } else { t.to_string() });
    }
    if !ext.is_empty() {
        words.push(ext_label(ext));
    }
    words.join(" ")
}

/// The file a line names: a token whose last component has a short alphabetic extension.
fn file_in(line: &str) -> Option<&str> {
    line.split_whitespace().map(|w| w.trim_matches(|c: char| ",;:()'\"`".contains(c))).find_map(|w| {
        let name = leaf(w);
        let (stem, ext) = name.rsplit_once('.')?;
        let ok = !stem.is_empty()
            && (1..=12).contains(&ext.len())
            && ext.starts_with(|c: char| c.is_ascii_alphabetic())
            && ext.chars().all(|c| c.is_ascii_alphanumeric());
        ok.then_some(name)
    })
}

/// What a finishing line amounts to, for the lead run: `the wnr-final truewidth Q4_K_S GGUF is
/// written and its check passed`, `quantised to 4.63 BPW`. `None` when the line says nothing a
/// person would repeat (it is never quoted).
pub(super) fn paraphrase(line: &str, run_name: &str, short: &str, root_leaf: Option<&str>) -> Option<String> {
    // Only a line that itself states success is repeated (`not written`, `check FAILED` are not).
    if logs::ending(line) != Ending::Success {
        return None;
    }
    let body = log_line_body(line);
    let lower = body.to_ascii_lowercase();
    let has = |w: &str| lower.split(|c: char| !c.is_ascii_alphanumeric()).any(|x| x == w);
    let verb = [("written", "written"), ("wrote", "written"), ("saved", "saved"), ("exported", "exported"), ("shipped", "shipped")]
        .into_iter()
        .find(|(w, _)| has(w))
        .map(|(_, v)| v);
    if let (Some(verb), Some(file)) = (verb, file_in(&body)) {
        let what = artifact_phrase(file, run_name, short, root_leaf);
        let checked = ["check ok", "checks ok", "check passed", "checks passed", "verified"].iter().any(|c| lower.contains(c));
        let check = if checked { " and its check passed" } else { "" };
        return Some(format!("the {what} is {verb}{check}"));
    }
    if lower.contains("quant size") {
        let words: Vec<&str> = body.split_whitespace().collect();
        let i = words.iter().position(|w| w.trim_matches(|c: char| !c.is_ascii_alphanumeric()).eq_ignore_ascii_case("bpw"))?;
        let n = words.get(i.checked_sub(1)?)?.trim_matches(|c: char| !(c.is_ascii_digit() || c == '.'));
        return (!n.is_empty()).then(|| format!("quantised to {n} BPW"));
    }
    None
}

/// A log line nobody would say aloud: long, columns, `=` padding, snake_case identifiers, or
/// unit soup. Such lines are paraphrased or left out, never quoted.
pub(super) fn is_noisy(line: &str) -> bool {
    let l = log_line_body(line);
    let snake = l.split_whitespace().any(|w| {
        let w = w.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_');
        w.contains('_') && w.chars().any(|c| c.is_ascii_alphabetic()) && file_in(w).is_none()
    });
    let numeric = l.split_whitespace().filter(|w| w.chars().any(|c| c.is_ascii_digit())).count();
    l.is_empty() || l.chars().count() > 60 || l.contains('|') || l.contains("==") || l.contains(" = ") || l.contains("  ") || snake || numeric > 3
}

/// Calendar days from `then` to `now`, local.
fn days_ago(ms: i64, now: &DateTime<Local>) -> Option<(i64, DateTime<Local>)> {
    let t = Local.timestamp_millis_opt(ms).single()?;
    Some(((now.date_naive() - t.date_naive()).num_days(), t))
}

/// `at 06:54` today, `yesterday at 10:29`, `on Tuesday` within the week, `on 2 October` before.
pub(super) fn when(ms: i64, now: &DateTime<Local>) -> String {
    match days_ago(ms, now) {
        Some((d, t)) if d <= 0 => format!("at {}", t.format("%H:%M")),
        Some((1, t)) => format!("yesterday at {}", t.format("%H:%M")),
        Some((d, t)) => day_name(d, &t),
        None => String::new(),
    }
}

fn day_name(days: i64, t: &DateTime<Local>) -> String {
    if days < 7 {
        format!("on {}", t.format("%A"))
    } else {
        format!("on {}", t.format("%-d %B"))
    }
}

/// The day without the clock, for runs said together: `None` today.
pub(super) fn day_word(ms: i64, now: &DateTime<Local>) -> Option<String> {
    match days_ago(ms, now)? {
        (d, _) if d <= 0 => None,
        (1, _) => Some("yesterday".into()),
        (d, t) => Some(day_name(d, &t)),
    }
}

// ---------------------------------------------------------------- results

/// Primary metric per suite, most telling first. Anything else with a known direction follows.
const PRIMARY: &[&str] = &["kl", "kld", "nll", "loss", "ppl", "perplexity", "top1", "acc", "accuracy", "f1", "score", "retained", "retention"];

/// Read as a share: a value in [0, 1.5] is said in percent.
fn is_share(leaf: &str) -> bool {
    let l = leaf.to_ascii_lowercase();
    let stem = l.trim_end_matches(|c: char| c.is_ascii_digit());
    matches!(stem, "top" | "acc" | "accuracy" | "retained" | "retention" | "pass" | "winrate" | "hit" | "f")
}

fn metric_label(leaf: &str) -> String {
    let l = leaf.to_ascii_lowercase();
    match l.as_str() {
        "kl" | "kld" | "nll" | "mse" | "mae" | "rmse" | "wer" | "cer" | "bpb" | "bpc" | "auc" | "f1" => l.to_ascii_uppercase(),
        "ppl" => "perplexity".into(),
        "acc" => "accuracy".into(),
        _ => match l.strip_prefix("top").filter(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit())) {
            Some(n) => format!("top-{n}"),
            None => leaf.to_string(),
        },
    }
}

/// Three significant figures; a share in percent.
fn say_value(leaf: &str, v: f64) -> String {
    if is_share(leaf) && (0.0..=1.5).contains(&v) {
        format!("{} percent", round_sig(v * 100.0, 3))
    } else {
        round_sig(v, 3)
    }
}

/// One suite's primary metric against the sibling.
struct Move {
    suite: String,
    leaf: String,
    before: f64,
    after: f64,
    improved: bool,
    /// Relative change, positive when better.
    gain: f64,
    /// The suite's `retained`, when it has one.
    retained: Option<(String, f64)>,
}

impl Move {
    /// `code KL`, or `KL` for a file without suites.
    fn what(&self) -> String {
        let m = metric_label(&self.leaf);
        if self.suite.is_empty() { m } else { format!("{} {m}", self.suite) }
    }

    fn moved(&self, v: f64) -> String {
        if self.after == self.before {
            return format!("{} holds at {}", self.what(), say_value(&self.leaf, v));
        }
        let way = if self.after < self.before { "down" } else { "up" };
        format!("{} is {way} to {}", self.what(), say_value(&self.leaf, v))
    }
}

fn split_metric(full: &str) -> (&str, &str) {
    full.rsplit_once('.').unwrap_or(("", full))
}

fn moves(r: &ResultComparison) -> Vec<Move> {
    let mut suites: Vec<&str> = Vec::new();
    for (m, _) in &r.now {
        let (s, _) = split_metric(m);
        if !suites.contains(&s) {
            suites.push(s);
        }
    }
    let rank = |leaf: &str| {
        let l = leaf.to_ascii_lowercase();
        PRIMARY.iter().position(|p| *p == l).unwrap_or(PRIMARY.len())
    };
    let mut out = Vec::new();
    for suite in suites {
        let in_suite = |list: &'_ [(String, f64)]| -> Vec<(String, f64)> {
            list.iter().filter(|(m, _)| split_metric(m).0 == suite).map(|(m, v)| (split_metric(m).1.to_string(), *v)).collect()
        };
        let (now, before) = (in_suite(&r.now), in_suite(&r.before));
        let best = now
            .iter()
            .filter_map(|(leaf, after)| {
                let d = direction(leaf)?;
                let b = before.iter().find(|(l, _)| l == leaf)?.1;
                Some((rank(leaf), leaf.clone(), d, b, *after))
            })
            .min_by_key(|(rk, ..)| *rk);
        let Some((_, leaf, d, before_v, after)) = best else { continue };
        let delta = (after - before_v) / before_v.abs().max(f64::MIN_POSITIVE);
        let gain = match d {
            Direction::Lower => -delta,
            Direction::Higher => delta,
        };
        let retained = now
            .iter()
            .find(|(l, _)| {
                let l = l.to_ascii_lowercase();
                (l == "retained" || l == "retention") && l != leaf.to_ascii_lowercase()
            })
            .cloned();
        out.push(Move { suite: suite.to_string(), leaf, before: before_v, after, improved: gain > 0.0, gain, retained });
    }
    out
}

/// `audit2_marlowe-...-final` -> `audit`.
fn family_label(stem: &str) -> String {
    let first = stem.split(['_', '-']).next().unwrap_or("");
    let word = first.trim_end_matches(|c: char| c.is_ascii_digit()).to_ascii_lowercase();
    if word.is_empty() { "results".into() } else { word }
}

/// `audit2_marlowe-valiance-22b-wnr7` -> `wnr7`.
fn sibling_label(stem: &str) -> &str {
    stem.rsplit(['-', '_']).next().filter(|s| !s.is_empty()).unwrap_or(stem)
}

/// `all four suites`, `both suites`.
fn all(n: usize) -> String {
    if n == 2 { "both suites".into() } else { format!("all {} suites", count_word(n as u64)) }
}

/// How a result compares with its previous sibling, at most two numbers: `Its audit beat wnr7 on
/// all four suites; code KL is down to 0.071 with 99.1 percent retained`. `lead` opens it (`Its`,
/// `The latest`). The bool is true when every suite improved. `None` when no metric compares.
pub(super) fn result_sentence(r: &ResultComparison, lead: &str) -> Option<(String, bool)> {
    let moves = moves(r);
    let n = moves.len();
    let k = moves.iter().filter(|m| m.improved).count();
    let worse = moves.iter().filter(|m| m.gain < 0.0).count();
    let best = moves.iter().max_by(|a, b| a.gain.total_cmp(&b.gain))?;
    let worst = moves.iter().min_by(|a, b| a.gain.total_cmp(&b.gain))?;
    let head = format!("{lead} {}", family_label(&r.stem));
    let sib = sibling_label(&r.sibling);
    let text = if n == 1 {
        let m = &moves[0];
        let verb = if m.improved { "beat" } else if m.after == m.before { "matched" } else { "trailed" };
        format!("{head} {verb} {sib}: {}, from {}", m.moved(m.after), say_value(&m.leaf, m.before))
    } else if k == n {
        let tail = match &best.retained {
            Some((leaf, v)) => format!(" with {} {leaf}", say_value(leaf, *v)),
            None => format!(", from {}", say_value(&best.leaf, best.before)),
        };
        format!("{head} beat {sib} on {}; {}{tail}", all(n), best.moved(best.after))
    } else if worse == n {
        format!("{head} trailed {sib} on {}; {}", all(n), worst.moved(worst.after))
    } else if k == 0 {
        format!("{head} did not beat {sib} on any suite; {}", worst.moved(worst.after))
    } else {
        format!(
            "{head} beat {sib} on {} of {} suites; {}, but {}",
            count_word(k as u64),
            count_word(n as u64),
            best.moved(best.after),
            worst.moved(worst.after)
        )
    };
    Some((capitalize(&text), k == n))
}
