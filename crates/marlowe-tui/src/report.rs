//! Bench report cards and coding-eval reports on disk (M4.6 track L writes them, H3 shows them).
//!
//! `marlowe bench` writes `<home>\bench\<model>-<time>.json`; `marlowe eval run` writes a
//! `report.md` in its run folder, by default under `%LOCALAPPDATA%\marlowe\eval\<time>\`. Both are
//! read here as plain data (no dependency on the binary): the card is JSON, the eval report is
//! markdown. [`lines`] turns either into styled-by-tone rows for the `/bench` panel.

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde_json::Value;

use crate::bg::{ReportFile, ReportKind};

/// How a report row is inked: the renderer maps these to the palette's weights and states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// A heading.
    Head,
    Body,
    Dim,
    Good,
    Warn,
    Bad,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportLine {
    pub tone: Tone,
    pub text: String,
}

fn line(tone: Tone, text: impl Into<String>) -> ReportLine {
    ReportLine { tone, text: text.into() }
}

/// Where reports live: the bench folder in the profile home, and the eval folders.
pub fn report_dirs() -> Vec<(ReportKind, PathBuf)> {
    let home = marlowe_proto::paths::cowork_home();
    let mut dirs = vec![(ReportKind::Bench, home.join("bench")), (ReportKind::Eval, home.join("eval"))];
    if let Some(l) = std::env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty()) {
        dirs.push((ReportKind::Eval, PathBuf::from(l).join("marlowe").join("eval")));
    }
    dirs
}

fn modified_ms(p: &Path) -> i64 {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as i64)
}

/// The reports under `dirs`, unsorted.
pub fn find_reports(dirs: &[(ReportKind, PathBuf)]) -> Vec<ReportFile> {
    let mut out = Vec::new();
    for (kind, dir) in dirs {
        let Ok(rd) = std::fs::read_dir(dir) else { continue };
        for e in rd.flatten() {
            let path = e.path();
            match kind {
                ReportKind::Bench => {
                    if path.extension().is_some_and(|x| x == "json") && path.is_file() {
                        let name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                        out.push(ReportFile { kind: *kind, path: path.display().to_string(), name, modified_ms: modified_ms(&path) });
                    }
                }
                ReportKind::Eval => {
                    let report = path.join("report.md");
                    if report.is_file() {
                        let name = path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                        out.push(ReportFile { kind: *kind, path: report.display().to_string(), name, modified_ms: modified_ms(&report) });
                    }
                }
            }
        }
    }
    out
}

fn pct(x: f64) -> String {
    format!("{:.1}%", 100.0 * x)
}

fn f(v: &Value, k: &str) -> f64 {
    v.get(k).and_then(Value::as_f64).unwrap_or(0.0)
}

fn n(v: &Value, k: &str) -> u64 {
    v.get(k).and_then(Value::as_u64).unwrap_or(0)
}

fn ratio(a: u64, b: u64) -> f64 {
    if b == 0 {
        0.0
    } else {
        a as f64 / b as f64
    }
}

/// A bench card (JSON) as rows. Text that is not a card is shown as it is.
pub fn bench_lines(json: &str) -> Vec<ReportLine> {
    let Ok(card) = serde_json::from_str::<Value>(json) else {
        return json.lines().map(|l| line(Tone::Dim, l)).collect();
    };
    let model = card.get("model").and_then(Value::as_str).unwrap_or("?");
    let local = card.get("local").and_then(Value::as_bool).unwrap_or(false);
    let mut out = vec![line(Tone::Head, format!("{model} ({})", if local { "local" } else { "frontier" }))];
    if let Some(t) = card.get("tools").filter(|t| !t.is_null()) {
        let prompts = n(t, "prompts");
        out.push(line(
            Tone::Body,
            format!(
                "tools     {prompts} prompts: right tool {}, right values {}, schema-valid {}; valid after repair {}, needing repair {}; {:.0} ms/call",
                pct(ratio(n(t, "selected"), prompts)),
                pct(ratio(n(t, "args_ok"), prompts)),
                pct(ratio(n(t, "schema_valid"), prompts)),
                pct(f(t, "valid_rate")),
                pct(f(t, "repair_rate")),
                f(t, "mean_ms")
            ),
        ));
        if let Some(m) = t.get("misses").and_then(Value::as_array) {
            for miss in m.iter().filter_map(Value::as_str).take(3) {
                out.push(line(Tone::Dim, format!("            miss: {}", miss.chars().take(140).collect::<String>())));
            }
        }
    }
    if let Some(e) = card.get("edits").filter(|e| !e.is_null()) {
        let retries = n(e, "retries");
        if let Some(formats) = e.get("formats").and_then(Value::as_object) {
            for (name, r) in formats {
                out.push(line(
                    Tone::Body,
                    format!(
                        "edits     {name:<10} first attempt {} ({}/{}), within {retries} retries {} ({}/{}), {:.2} calls/edit",
                        pct(f(r, "first_rate")),
                        n(r, "first_applied"),
                        n(r, "cases"),
                        pct(f(r, "success_rate")),
                        n(r, "succeeded"),
                        n(r, "cases"),
                        f(r, "mean_calls")
                    ),
                ));
            }
        }
        let rec: Vec<&str> = e.get("recommended").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
        out.push(line(Tone::Dim, format!("            recommended: {}", if rec.is_empty() { "none".to_string() } else { rec.join(" > ") })));
    }
    if let Some(m) = card.get("memory").filter(|m| !m.is_null()) {
        out.push(line(
            Tone::Body,
            format!("memory    recalled {}/{} after a restart ({}), stored {}/{}", n(m, "recalled"), n(m, "facts"), pct(f(m, "recall_rate")), n(m, "stored"), n(m, "facts")),
        ));
    }
    if let Some(r) = card.get("research").filter(|r| !r.is_null()) {
        out.push(line(
            Tone::Body,
            format!(
                "research  answers {}/{} ({}), citations verified {}/{} ({})",
                n(r, "answers_correct"),
                n(r, "questions"),
                pct(f(r, "answer_rate")),
                n(r, "citations_verified"),
                n(r, "citations"),
                pct(f(r, "citation_accuracy"))
            ),
        ));
    }
    if let Some(c) = card.get("coding").filter(|c| !c.is_null()) {
        match c.get("error").and_then(Value::as_str) {
            Some(e) => out.push(line(Tone::Warn, format!("coding    not run: {e}"))),
            None => out.push(line(Tone::Body, format!("coding    micro tasks passed {}/{} ({})", n(c, "passed"), n(c, "runs"), pct(f(c, "pass_rate"))))),
        }
    }
    if let Some(s) = card.get("skipped").and_then(Value::as_array) {
        for k in s.iter().filter_map(Value::as_str) {
            out.push(line(Tone::Warn, format!("skipped   {k}")));
        }
    }
    if let Some(g) = card.get("gates").and_then(Value::as_array) {
        out.push(line(Tone::Head, "gates"));
        for row in g {
            let pass = row.get("pass").and_then(Value::as_bool).unwrap_or(false);
            let id = row.get("id").and_then(Value::as_str).unwrap_or("?");
            let detail = row.get("detail").and_then(Value::as_str).unwrap_or("");
            out.push(line(if pass { Tone::Good } else { Tone::Bad }, format!("{id:<4} {}  {detail}", if pass { "PASS" } else { "FAIL" })));
        }
    }
    out
}

/// An eval run's `report.md` as rows: headings, the tables as they are, and the gates inked.
pub fn eval_lines(md: &str) -> Vec<ReportLine> {
    md.lines()
        .map(|l| {
            let t = l.trim_end();
            let tone = if t.starts_with('#') {
                Tone::Head
            } else if t.contains("FAIL") {
                Tone::Bad
            } else if t.contains("PASS") {
                Tone::Good
            } else if t.contains("not graded") || t.contains("unavailable") || t.contains("not wired") {
                Tone::Warn
            } else if t.starts_with('|') {
                Tone::Body
            } else {
                Tone::Dim
            };
            line(tone, t.trim_start_matches('#').trim_start())
        })
        .collect()
}

/// `kind`'s text as rows.
pub fn lines(kind: ReportKind, text: &str) -> Vec<ReportLine> {
    match kind {
        ReportKind::Bench => bench_lines(text),
        ReportKind::Eval => eval_lines(text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CARD: &str = r#"{
      "model": "ollama/marlowe-dusk:27b-super", "local": true, "started_unix": 1,
      "tools": {"prompts": 200, "called": 198, "selected": 190, "args_ok": 186, "schema_valid": 196, "valid_rate": 0.985, "repair_rate": 0.04, "mean_ms": 812.0, "misses": ["read vs grep"]},
      "edits": {"formats": {"edit": {"cases": 40, "first_applied": 38, "succeeded": 40, "first_rate": 0.95, "success_rate": 1.0, "mean_calls": 1.1}}, "recommended": ["edit"], "retries": 2},
      "memory": null, "research": null,
      "coding": {"runs": 10, "passed": 7, "pass_rate": 0.7, "error": null},
      "skipped": ["research: no notes"],
      "gates": [{"id": "A4", "pass": true, "detail": "ok"}, {"id": "A5", "pass": false, "detail": "first attempt 88%"}]
    }"#;

    #[test]
    fn a_bench_card_reads_as_the_cli_prints_it() {
        let rows = bench_lines(CARD);
        let text: Vec<&str> = rows.iter().map(|r| r.text.as_str()).collect();
        assert!(text[0].starts_with("ollama/marlowe-dusk:27b-super (local)"));
        assert!(text.iter().any(|l| l.contains("200 prompts") && l.contains("valid after repair 98.5%")), "{text:?}");
        assert!(text.iter().any(|l| l.contains("edit") && l.contains("first attempt 95.0% (38/40)")));
        assert!(text.iter().any(|l| l.contains("micro tasks passed 7/10 (70.0%)")));
        let pass = rows.iter().find(|r| r.text.starts_with("A4")).unwrap();
        let fail = rows.iter().find(|r| r.text.starts_with("A5")).unwrap();
        assert_eq!((pass.tone, fail.tone), (Tone::Good, Tone::Bad));
    }

    #[test]
    fn eval_reports_ink_their_gates_and_skip_the_hashes() {
        let rows = eval_lines("# Coding eval: micro on m\n\n| arm | pass |\n|---|---|\n| marlowe-full | 7/10 |\nA1 PASS\nA3 FAIL\nGo: unavailable here\n");
        assert_eq!(rows[0], ReportLine { tone: Tone::Head, text: "Coding eval: micro on m".into() });
        assert_eq!(rows.iter().find(|r| r.text == "A1 PASS").unwrap().tone, Tone::Good);
        assert_eq!(rows.iter().find(|r| r.text == "A3 FAIL").unwrap().tone, Tone::Bad);
        assert_eq!(rows.iter().find(|r| r.text.starts_with("Go:")).unwrap().tone, Tone::Warn);
    }

    #[test]
    fn reports_are_found_in_their_folders() {
        let dir = std::env::temp_dir().join(format!("marlowe-tui-reports-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("bench")).unwrap();
        std::fs::create_dir_all(dir.join("eval").join("20261011-0100")).unwrap();
        std::fs::write(dir.join("bench").join("dusk-1.json"), CARD).unwrap();
        std::fs::write(dir.join("bench").join("notes.txt"), "x").unwrap();
        std::fs::write(dir.join("eval").join("20261011-0100").join("report.md"), "# r").unwrap();
        let found = find_reports(&[(ReportKind::Bench, dir.join("bench")), (ReportKind::Eval, dir.join("eval"))]);
        let mut names: Vec<(ReportKind, String)> = found.iter().map(|r| (r.kind, r.name.clone())).collect();
        names.sort_by_key(|(_, n)| n.clone());
        assert_eq!(names, [(ReportKind::Eval, "20261011-0100".to_string()), (ReportKind::Bench, "dusk-1".to_string())]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
