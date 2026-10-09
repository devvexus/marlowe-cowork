//! Training logs (`trainer_state.json`, `*.jsonl`, `*.csv`) and metric/result JSON: the readers
//! that turn their contents into training summaries, sibling deltas, bests and notices.

use std::path::Path;

use marlowe_proto::{NoticeSource, Severity};

use super::{file_name, path_key, raise, trainer_dir, Engine, Kind, Run, HISTORY_BYTES, MAX_NEW_METRIC_LOG, MAX_RECORDS, MAX_TRAINER_STATE, FAMILY_SEED_MAX};
use crate::logs;
use crate::metrics::{self, better, direction, fmt_num, fmt_signed};
use crate::notice::NoticeDraft;
use crate::trainer::{self, Record, TrainingSummary};

impl Engine {
    pub(super) fn tail_metric_log(&mut self, path: &Path, len: u64, silent: bool) -> bool {
        let csv = self.files[path].kind == Kind::Csv;
        if self.files[path].ignored {
            return true;
        }
        let from = self.files[path].mark.offset;
        if from == 0 && len > MAX_NEW_METRIC_LOG {
            let st = self.files.get_mut(path).expect("present");
            st.ignored = true;
            st.mark.offset = len;
            return true;
        }
        // After a restart: re-read recent history silently so transitions are judged on it.
        let mut prev = self.files[path].summary.clone();
        if !self.files[path].history_loaded && from > 0 {
            let start = from.saturating_sub(HISTORY_BYTES);
            let lines = logs::read_range(path, start, from).unwrap_or_default();
            let header = if csv { logs::read_range(path, 0, 4096).ok().and_then(|l| l.into_iter().next()) } else { None };
            self.spend(from - start);
            let st = self.files.get_mut(path).expect("present");
            st.header = header.clone();
            st.records = lines.iter().filter_map(|l| parse_record(l, header.as_deref(), csv)).collect();
            st.summary = trainer::summarize(&st.records, None, self.cfg.plateau_evals);
            prev = st.summary.clone();
        }
        let Ok(a) = logs::read_appended(path, from, self.budget, self.long_line()) else { return true };
        self.spend((a.next_offset - if a.restarted { 0 } else { from }).max(1));
        let plateau_evals = self.cfg.plateau_evals;
        let st = self.files.get_mut(path).expect("present");
        st.history_loaded = true;
        if a.restarted {
            st.records.clear();
            st.header = None;
        }
        for line in &a.lines {
            if csv && st.header.is_none() {
                st.header = Some(line.clone());
                continue;
            }
            let Some(rec) = parse_record(line, st.header.as_deref(), csv) else { continue };
            if st.records.is_empty() && !trainer::looks_like_metrics(&rec) {
                // A dataset, not a metric log: never read again.
                st.ignored = true;
                st.mark.offset = len;
                return true;
            }
            st.records.push(rec);
        }
        if st.records.len() > MAX_RECORDS {
            let extra = st.records.len() - MAX_RECORDS;
            st.records.drain(..extra);
        }
        st.mark.offset = a.next_offset;
        let summary = trainer::summarize(&st.records, None, plateau_evals);
        st.summary = summary.clone();
        let done = a.caught_up();
        if !silent {
            self.training_notices(&prev, &summary, path, &file_name(path));
        }
        if let Some(run) = path.parent().and_then(|d| self.runs.get_mut(d)) {
            run.step = summary.step.or(run.step);
            run.loss = summary.loss.or(run.loss);
        }
        done
    }

    pub(super) fn read_trainer(&mut self, path: &Path, len: u64, mtime: i64, silent: bool) -> bool {
        if len > MAX_TRAINER_STATE {
            return true;
        }
        let Ok(bytes) = logs::read_capped(path, MAX_TRAINER_STATE) else { return true };
        self.spend(bytes.len() as u64);
        let Some((records, global, max)) = trainer::parse_trainer_state(&String::from_utf8_lossy(&bytes)) else { return true };
        let mut s = trainer::summarize(&records, max, self.cfg.plateau_evals);
        if s.step.is_none() {
            s.step = global;
        }
        if matches!((global, max), (Some(g), Some(m)) if m > 0 && g >= m) {
            s.finished = true;
        }
        let dir = trainer_dir(path);
        let prev = self.trainer_prev.get(&dir).cloned().unwrap_or_default();
        if !silent {
            self.training_notices(&prev, &s, &dir, &file_name(&dir));
        }
        let run = self.runs.entry(dir.clone()).or_insert_with(|| Run::new(!silent));
        run.step = s.step;
        run.loss = s.loss;
        run.trainer_finished = s.finished;
        run.trainer_ms = Some(run.trainer_ms.unwrap_or(0).max(mtime));
        self.trainer_prev.insert(dir, s);
        true
    }

    /// Notices for a training log whose summary turned bad (or finished) since `prev`.
    fn training_notices(&self, prev: &TrainingSummary, now: &TrainingSummary, subject: &Path, label: &str) {
        let step = now.step.map(|s| format!("step {s}")).unwrap_or_else(|| "latest record".into());
        let loss = now.loss.map(|l| format!("loss {}", fmt_num(l)));
        let eval = now.eval_loss.map(|l| format!("eval_loss {}", fmt_num(l)));
        let join = |parts: &[Option<String>]| parts.iter().flatten().cloned().collect::<Vec<_>>().join(" · ");
        let mut drafts = Vec::new();
        if now.non_finite && !prev.non_finite {
            drafts.push((Severity::Alarm, "nan", format!("{label}: loss went NaN/inf"), join(&[Some(step.clone()), loss.clone()])));
        }
        if now.diverged && !prev.diverged {
            drafts.push((Severity::Alarm, "diverged", format!("{label}: loss diverged"), format!("{} · over 3x its running minimum", join(&[Some(step.clone()), loss.clone()]))));
        }
        if now.plateau && !prev.plateau {
            let n = self.cfg.plateau_evals;
            drafts.push((Severity::Attention, "plateau", format!("{label}: eval plateaued"), format!("no improvement in the last {n} evals · {}", join(&[Some(step.clone()), eval.clone()]))));
        }
        if now.finished && !prev.finished {
            drafts.push((Severity::Good, "finished", format!("{label} finished training"), join(&[Some(step), loss, eval])));
        }
        for (sev, kind, title, detail) in drafts {
            raise(&self.store, &self.sink, NoticeDraft::new(NoticeSource::Trainer, sev, kind, title, detail, path_key(subject)));
        }
    }

    /// Flat metrics of a JSON file: stored ones, else read (counted against the budget).
    fn metrics_of(&mut self, path: &Path) -> Option<Vec<(String, f64)>> {
        let key = path_key(path);
        if let Ok(Some(m)) = self.store.metrics(&key) {
            return Some(m);
        }
        let (m, _) = self.load_metrics(path)?;
        let _ = self.store.set_metrics(&key, &m);
        Some(m)
    }

    fn load_metrics(&mut self, path: &Path) -> Option<(Vec<(String, f64)>, usize)> {
        let bytes = logs::read_capped(path, self.cfg.max_metric_json + 1).ok()?;
        self.spend(bytes.len() as u64);
        if bytes.len() as u64 > self.cfg.max_metric_json {
            return None;
        }
        let (clean, non_finite) = metrics::sanitize_non_finite(&String::from_utf8_lossy(&bytes));
        let v: serde_json::Value = serde_json::from_str(&clean).ok()?;
        Some((metrics::flatten(&v), non_finite))
    }

    /// A new or changed metric/result JSON: compare with its most similar sibling, update the
    /// family's bests, and raise one notice.
    pub(super) fn read_metric(&mut self, path: &Path, len: u64, silent: bool) -> bool {
        if !metrics::is_metric_file(path, len, self.cfg.max_metric_json) {
            return true;
        }
        let key = path_key(path);
        let previous = self.store.metrics(&key).ok().flatten();
        let Some((now, non_finite)) = self.load_metrics(path) else { return true };
        let _ = self.store.set_metrics(&key, &now);
        let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        if silent {
            return true;
        }
        if non_finite > 0 {
            let d = NoticeDraft::new(NoticeSource::Metric, Severity::Alarm, "nan", format!("{stem}: NaN/inf in results"), format!("{non_finite} non-finite value(s)"), key.clone());
            raise(&self.store, &self.sink, d);
        }
        let known: Vec<(String, f64)> = now.iter().filter(|(m, _)| direction(m).is_some()).cloned().collect();
        if known.is_empty() {
            return true;
        }
        let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
        let siblings: Vec<String> = std::fs::read_dir(&dir)
            .map(|rd| {
                rd.flatten()
                    .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
                    .filter(|e| metrics::is_metric_file(&e.path(), e.metadata().map(|m| m.len()).unwrap_or(0), self.cfg.max_metric_json))
                    .filter_map(|e| e.path().file_stem().map(|s| s.to_string_lossy().into_owned()))
                    .filter(|s| *s != stem)
                    .collect()
            })
            .unwrap_or_default();
        let sibling = metrics::most_similar(&stem, &siblings).map(str::to_string);
        let (before, before_label) = match &sibling {
            Some(s) => (self.metrics_of(&dir.join(format!("{s}.json"))), s.rsplit(['-', '_']).next().unwrap_or(s).to_string()),
            None => (previous, "before".to_string()),
        };
        let (fam, _) = metrics::family(&stem);
        let fam_key = format!("{}|{fam}", path_key(&dir));
        if !self.store.has_family(&fam_key).unwrap_or(true) {
            // Seed the family's bests from its other members.
            let members: Vec<String> = siblings.iter().filter(|s| metrics::family(s).0 == fam).take(FAMILY_SEED_MAX).cloned().collect();
            for s in members {
                for (m, v) in self.metrics_of(&dir.join(format!("{s}.json"))).unwrap_or_default() {
                    self.update_best(&fam_key, &m, v);
                }
            }
        }
        let store = self.store.clone();
        let best = |m: &str| store.best(&fam_key, m).ok().flatten();
        let deltas = before.as_deref().map(|b| metrics::compare(&known, b, &best)).unwrap_or_default();
        for (m, v) in &known {
            self.update_best(&fam_key, m, *v);
        }
        let names: Vec<&str> = known.iter().map(|(m, _)| m.as_str()).collect();
        let short = metrics::short_names(&names);
        // Bests first: a reader that keeps only the leading parts keeps a best and its number.
        let is_best = |m: &String| deltas.iter().any(|d| &d.metric == m && d.best_so_far);
        let mut order: Vec<(&(String, f64), &String)> = known.iter().zip(&short).collect();
        order.sort_by_key(|((m, _), _)| !is_best(m));
        let parts: Vec<String> = order
            .into_iter()
            .take(6)
            .map(|((m, v), s)| match deltas.iter().find(|d| &d.metric == m) {
                Some(d) => format!(
                    "{s} {} ({before_label} {}, {}{})",
                    fmt_num(*v),
                    fmt_num(d.before),
                    fmt_signed(d.after - d.before),
                    if d.best_so_far { ", best" } else { "" }
                ),
                None => format!("{s} {}", fmt_num(*v)),
            })
            .collect();
        let bests: Vec<String> = deltas.iter().filter(|d| d.best_so_far).filter_map(|d| names.iter().position(|n| *n == d.metric).map(|i| short[i].clone())).collect();
        let (sev, title) = if bests.is_empty() {
            (Severity::Info, format!("{stem}: new results"))
        } else if bests.len() > 3 {
            // A title names at most three; the detail carries the numbers.
            (Severity::Good, format!("{stem}: best so far on {} metrics", bests.len()))
        } else {
            (Severity::Good, format!("{stem}: best {} so far", bests.join(", ")))
        };
        raise(&self.store, &self.sink, NoticeDraft::new(NoticeSource::Metric, sev, "result", title, parts.join(" · "), key));
        true
    }

    fn update_best(&self, fam_key: &str, metric: &str, v: f64) {
        let Some(d) = direction(metric) else { return };
        match self.store.best(fam_key, metric) {
            Ok(Some(b)) if !better(d, v, b) => {}
            Ok(_) => {
                let _ = self.store.set_best(fam_key, metric, v);
            }
            Err(_) => {}
        }
    }

}

fn parse_record(line: &str, header: Option<&str>, csv: bool) -> Option<Record> {
    if csv {
        trainer::parse_csv_row(header?, line)
    } else {
        trainer::parse_jsonl_line(line)
    }
}
