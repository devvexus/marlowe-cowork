//! Findings (G13): what `report_findings` and the review workflows produce, in the 3.7 schema
//! `{lens, severity, file, line, claim, evidence, fix}`. Checked here (an invalid finding is
//! refused with a message the model can act on) and kept per session or run under
//! `<home>\findings\<key>.jsonl`.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use marlowe_proto::{Finding, FindingSeverity};
use serde_json::{json, Value};

/// The JSON Schema of one finding (for `agent(schema=...)` and the tool's spec).
pub fn finding_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "lens": {"type": "string", "description": "the angle that found it: correctness, security, performance, reuse, tests, ..."},
            "severity": {"enum": ["info", "low", "medium", "high", "critical"]},
            "file": {"type": "string", "description": "path relative to the repository root"},
            "line": {"type": "integer", "minimum": 1},
            "claim": {"type": "string", "description": "one sentence: what is wrong"},
            "evidence": {"type": "string", "description": "the code or behaviour that shows it"},
            "fix": {"type": "string", "description": "the change that fixes it"}
        },
        "required": ["lens", "severity", "file", "claim", "evidence"]
    })
}

/// `{"findings": [finding, ...]}`.
pub fn findings_schema() -> Value {
    json!({
        "type": "object",
        "properties": {"findings": {"type": "array", "items": finding_schema()}},
        "required": ["findings"]
    })
}

fn severity(s: &str) -> Option<FindingSeverity> {
    Some(match s.trim().to_ascii_lowercase().as_str() {
        "info" | "nit" | "note" => FindingSeverity::Info,
        "low" | "minor" => FindingSeverity::Low,
        "medium" | "moderate" => FindingSeverity::Medium,
        "high" | "major" => FindingSeverity::High,
        "critical" | "blocker" => FindingSeverity::Critical,
        _ => return None,
    })
}

/// Reads one finding; the error names the field and what it must be.
pub fn parse_finding(v: &Value, at: usize) -> Result<Finding, String> {
    let o = v.as_object().ok_or_else(|| format!("findings[{at}] must be an object"))?;
    let s = |k: &str| o.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    let lens = s("lens").ok_or_else(|| format!("findings[{at}].lens is required (correctness, security, performance, reuse, ...)"))?;
    let file = s("file").ok_or_else(|| format!("findings[{at}].file is required (a path)"))?;
    let claim = s("claim").ok_or_else(|| format!("findings[{at}].claim is required: one sentence on what is wrong"))?;
    let sev = o.get("severity").and_then(Value::as_str).ok_or_else(|| format!("findings[{at}].severity is required"))?;
    let severity = severity(sev).ok_or_else(|| format!("findings[{at}].severity `{sev}` must be info, low, medium, high or critical"))?;
    let line = match o.get("line") {
        None | Some(Value::Null) => None,
        Some(l) => match l.as_u64() {
            Some(n) if n >= 1 && n <= u32::MAX as u64 => Some(n as u32),
            _ => return Err(format!("findings[{at}].line must be a positive integer")),
        },
    };
    Ok(Finding { lens, severity, file, line, claim, evidence: s("evidence").unwrap_or_default(), fix: s("fix") })
}

/// Reads a list (`[...]` or `{"findings": [...]}`); every bad entry is reported.
pub fn parse_findings(v: &Value) -> Result<Vec<Finding>, String> {
    let list = match v {
        Value::Array(a) => a,
        Value::Object(o) => o.get("findings").and_then(Value::as_array).ok_or("`findings` must be a list of findings")?,
        _ => return Err("`findings` must be a list of findings".into()),
    };
    let mut out = Vec::new();
    let mut errors = Vec::new();
    for (i, f) in list.iter().enumerate() {
        match parse_finding(f, i) {
            Ok(f) => out.push(f),
            Err(e) => errors.push(e),
        }
    }
    if errors.is_empty() {
        Ok(out)
    } else {
        Err(errors.join("; "))
    }
}

/// `<home>\findings\<key>.jsonl` (the key a session id or `run-<id>`, made file-safe).
pub fn store_path(home: &Path, key: &str) -> PathBuf {
    let safe: String = key.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' }).collect();
    home.join("findings").join(format!("{safe}.jsonl"))
}

/// Appends findings to the store.
pub fn append(home: &Path, key: &str, findings: &[Finding]) -> std::io::Result<()> {
    let path = store_path(home, key);
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    for x in findings {
        let mut line = serde_json::to_string(x).map_err(std::io::Error::other)?;
        line.push('\n');
        f.write_all(line.as_bytes())?;
    }
    Ok(())
}

/// Everything stored under `key`.
pub fn load(home: &Path, key: &str) -> Vec<Finding> {
    let Ok(f) = std::fs::File::open(store_path(home, key)) else { return Vec::new() };
    std::io::BufReader::new(f).lines().map_while(Result::ok).filter_map(|l| serde_json::from_str(&l).ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn findings_are_checked_field_by_field() {
        let ok = json!([{"lens": "correctness", "severity": "major", "file": "a.rs", "line": 3, "claim": "off by one", "evidence": "i <= n"}]);
        let f = parse_findings(&ok).unwrap();
        assert_eq!(f[0].severity, FindingSeverity::High);
        let bad = json!({"findings": [{"lens": "x", "severity": "urgent", "file": "a.rs", "claim": "c"}, {"severity": "low", "file": "b", "claim": "c", "line": 0, "lens": "y"}]});
        let e = parse_findings(&bad).unwrap_err();
        assert!(e.contains("findings[0].severity `urgent`") && e.contains("findings[1].line"), "{e}");
    }
}
