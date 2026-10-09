//! Training metric logs: HF `trainer_state.json` (`log_history`), `*.jsonl`, `*.csv`.

use serde_json::Value;

use crate::metrics::{better, direction, sanitize_non_finite, Direction};

/// One logged record: flat numeric fields (`step`, `loss`, `eval_loss`, `learning_rate`, ...).
/// Non-finite values (Python's `NaN`/`Infinity`) are kept.
pub type Record = Vec<(String, f64)>;

/// What a training log says now.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TrainingSummary {
    pub step: Option<u64>,
    pub loss: Option<f64>,
    pub eval_loss: Option<f64>,
    /// A NaN or inf loss was logged.
    pub non_finite: bool,
    /// The latest loss is above 3x the running minimum before it.
    pub diverged: bool,
    /// The eval metric has not improved across the last N evals.
    pub plateau: bool,
    /// `max_steps`/`global_step` say training reached its end.
    pub finished: bool,
}

/// Records from a `trainer_state.json` text (its `log_history`), plus `global_step`/`max_steps`.
pub fn parse_trainer_state(text: &str) -> Option<(Vec<Record>, Option<u64>, Option<u64>)> {
    let (clean, _) = sanitize_non_finite(text);
    let v: Value = serde_json::from_str(&clean).ok()?;
    let history = v.get("log_history")?.as_array()?;
    let records = history.iter().filter_map(record_of).collect();
    let int = |k: &str| v.get(k).and_then(Value::as_u64);
    Some((records, int("global_step"), int("max_steps")))
}

/// One JSONL line's numeric fields.
pub fn parse_jsonl_line(line: &str) -> Option<Record> {
    let line = line.trim();
    if !line.starts_with('{') {
        return None;
    }
    let (clean, _) = sanitize_non_finite(line);
    record_of(&serde_json::from_str(&clean).ok()?)
}

/// One CSV row's numeric fields under `header` (plain comma-separated, optional quotes).
pub fn parse_csv_row(header: &str, row: &str) -> Option<Record> {
    let unquote = |s: &str| s.trim().trim_matches('"').trim().to_string();
    let rec: Record = header
        .split(',')
        .map(unquote)
        .zip(row.split(',').map(unquote))
        .filter(|(k, _)| !k.is_empty())
        .filter_map(|(k, v)| v.parse::<f64>().ok().map(|x| (k, x)))
        .collect();
    (!rec.is_empty()).then_some(rec)
}

/// Numeric fields of a flat JSON object; `"NaN"`/`"Infinity"` strings count as numbers.
fn record_of(v: &Value) -> Option<Record> {
    let rec: Record = v
        .as_object()?
        .iter()
        .filter_map(|(k, v)| {
            let x = match v {
                Value::Number(n) => n.as_f64()?,
                Value::String(s) => match s.as_str() {
                    "NaN" => f64::NAN,
                    "Infinity" => f64::INFINITY,
                    "-Infinity" => f64::NEG_INFINITY,
                    _ => return None,
                },
                _ => return None,
            };
            Some((k.clone(), x))
        })
        .collect();
    (!rec.is_empty()).then_some(rec)
}

/// Whether a record looks like a training metric line (not a dataset row).
pub fn looks_like_metrics(rec: &Record) -> bool {
    rec.iter().any(|(k, _)| {
        let k = k.to_ascii_lowercase();
        matches!(k.as_str(), "step" | "global_step" | "epoch" | "iter" | "iteration") || direction(&k).is_some()
    })
}

fn get(rec: &Record, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|k| rec.iter().find(|(n, _)| n == k).map(|(_, v)| *v))
}

const STEP: &[&str] = &["step", "global_step", "iteration", "iter"];
const LOSS: &[&str] = &["loss", "train_loss", "train/loss"];
const EVAL_LOSS: &[&str] = &["eval_loss", "eval/loss", "val_loss", "validation_loss"];

/// The eval metric to judge a plateau on: `eval_loss` (or kin), else the first `eval_*`/`val_*`
/// field with a known direction.
fn eval_key(records: &[Record]) -> Option<(String, Direction)> {
    for r in records {
        if let Some((k, _)) = r.iter().find(|(k, _)| EVAL_LOSS.contains(&k.as_str())) {
            return Some((k.clone(), Direction::Lower));
        }
    }
    records.iter().flat_map(|r| r.iter()).find_map(|(k, _)| {
        let l = k.to_ascii_lowercase();
        let evalish = l.starts_with("eval") || l.starts_with("val_") || l.starts_with("val/");
        let skip = l.ends_with("runtime") || l.ends_with("per_second");
        if evalish && !skip { direction(k).map(|d| (k.clone(), d)) } else { None }
    })
}

/// Summarises records; a plateau needs `plateau_evals` evals without improvement after an
/// earlier best.
pub fn summarize(records: &[Record], max_steps: Option<u64>, plateau_evals: usize) -> TrainingSummary {
    let step = records.iter().rev().find_map(|r| get(r, STEP)).filter(|s| s.is_finite() && *s >= 0.0).map(|s| s as u64);
    let losses: Vec<f64> = records.iter().filter_map(|r| get(r, LOSS)).collect();
    let loss = losses.last().copied();
    let eval = eval_key(records);
    let evals: Vec<f64> = eval.as_ref().map(|(k, _)| records.iter().filter_map(|r| get(r, &[k.as_str()])).collect()).unwrap_or_default();
    let eval_loss = records.iter().rev().find_map(|r| get(r, EVAL_LOSS));
    let non_finite = losses.iter().chain(&evals).any(|x| !x.is_finite())
        || records.iter().any(|r| get(r, &["grad_norm"]).is_some_and(|g| !g.is_finite()));

    let finite: Vec<f64> = losses.iter().copied().filter(|x| x.is_finite()).collect();
    let diverged = finite.len() >= 3 && {
        let (last, earlier) = finite.split_last().expect("len >= 3");
        let min = earlier.iter().copied().fold(f64::INFINITY, f64::min);
        min > 0.0 && *last > 3.0 * min
    };

    let plateau = match &eval {
        Some((_, d)) if plateau_evals > 0 => {
            let vals: Vec<f64> = evals.iter().copied().filter(|x| x.is_finite()).collect();
            vals.len() > plateau_evals && {
                let (prior, recent) = vals.split_at(vals.len() - plateau_evals);
                let best = prior.iter().copied().reduce(|a, b| if better(*d, b, a) { b } else { a }).expect("non-empty");
                // Improvement must beat the earlier best by more than noise (0.01% relative).
                let tol = best.abs() * 1e-4;
                !recent.iter().any(|x| match d {
                    Direction::Lower => *x < best - tol,
                    Direction::Higher => *x > best + tol,
                })
            }
        }
        _ => false,
    };

    TrainingSummary {
        step,
        loss,
        eval_loss,
        non_finite,
        diverged,
        plateau,
        finished: matches!((step, max_steps), (Some(s), Some(m)) if m > 0 && s >= m),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trainer_state_with_nan_and_finish() {
        let text = r#"{"global_step": 40, "max_steps": 40, "log_history": [
            {"epoch": 0.1, "step": 10, "loss": 2.5, "learning_rate": 1e-4},
            {"epoch": 0.2, "step": 20, "loss": NaN, "grad_norm": NaN},
            {"epoch": 0.3, "step": 30, "eval_loss": 2.1, "eval_runtime": 3.2},
            {"epoch": 0.4, "step": 40, "loss": 2.2}
        ]}"#;
        let (records, global, max) = parse_trainer_state(text).unwrap();
        assert_eq!((records.len(), global, max), (4, Some(40), Some(40)));
        let s = summarize(&records, max, 5);
        assert!(s.non_finite && s.finished && !s.diverged && !s.plateau);
        assert_eq!((s.step, s.loss, s.eval_loss), (Some(40), Some(2.2), Some(2.1)));
        assert!(parse_trainer_state("{}").is_none());
    }

    #[test]
    fn divergence_is_three_times_the_running_min() {
        let recs: Vec<Record> = [3.0, 1.0, 0.9, 1.2, 2.6].iter().enumerate().map(|(i, l)| vec![("step".into(), i as f64), ("loss".into(), *l)]).collect();
        assert!(!summarize(&recs, None, 3).diverged);
        let mut more = recs.clone();
        more.push(vec![("step".into(), 9.0), ("loss".into(), 2.71)]);
        assert!(summarize(&more, None, 3).diverged);
    }

    #[test]
    fn plateauing_jsonl() {
        let lines = [
            r#"{"step": 100, "loss": 2.0, "eval_loss": 1.90}"#,
            r#"{"step": 200, "loss": 1.8, "eval_loss": 1.70}"#,
            r#"{"step": 300, "loss": 1.7, "eval_loss": 1.72}"#,
            r#"{"step": 400, "loss": 1.6, "eval_loss": 1.71}"#,
            r#"not json"#,
            r#"{"step": 500, "loss": 1.6, "eval_loss": 1.70}"#,
        ];
        let recs: Vec<Record> = lines.iter().filter_map(|l| parse_jsonl_line(l)).collect();
        assert_eq!(recs.len(), 5);
        assert!(looks_like_metrics(&recs[0]));
        let s = summarize(&recs, None, 3);
        assert!(s.plateau);
        assert_eq!(s.step, Some(500));
        // One improvement in the window breaks it.
        let mut better = recs.clone();
        better.push(parse_jsonl_line(r#"{"step": 600, "eval_loss": 1.60}"#).unwrap());
        assert!(!summarize(&better, None, 3).plateau);
        // Higher-is-better eval metrics work too; unknown ones never plateau.
        let acc: Vec<Record> = [0.5, 0.7, 0.69, 0.7, 0.68].iter().map(|a| vec![("eval_accuracy".into(), *a)]).collect();
        assert!(summarize(&acc, None, 3).plateau);
        let odd: Vec<Record> = [5.0, 5.0, 5.0, 5.0, 5.0].iter().map(|a| vec![("eval_unigram".into(), *a)]).collect();
        assert!(!summarize(&odd, None, 3).plateau);
    }

    #[test]
    fn csv_rows_and_dataset_lines() {
        let rec = parse_csv_row("step,loss,\"tag\"", "10, 1.5 ,warm").unwrap();
        assert_eq!(rec, vec![("step".to_string(), 10.0), ("loss".to_string(), 1.5)]);
        assert!(parse_csv_row("a,b", "x,y").is_none());
        let nan = parse_csv_row("step,loss", "11,nan").unwrap();
        assert!(nan[1].1.is_nan());
        let data = parse_jsonl_line(r#"{"id": 7, "text": "hello", "label": 1}"#).unwrap();
        assert!(!looks_like_metrics(&data));
    }
}
