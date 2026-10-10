//! `/export`: a session as Markdown (to read) or JSON (to keep). Faithful to what was said and
//! done; the scrubber that makes an export safe to share is track M's, and runs on top.

use std::collections::HashMap;
use std::fmt::Write as _;

use marlowe_model::{Part, Role, ToolCall};
use marlowe_proto::{CostReport, ExportFormat, SessionSummary};
use serde_json::{json, Value};

use crate::store::RowOut;

/// Longest tool result Markdown quotes; the JSON export has it whole.
const RESULT_CHARS: usize = 1_200;

/// Longest tool-call arguments Markdown shows on one line.
const ARGS_CHARS: usize = 240;

/// Renders `rows` of `session` in `format`.
pub(crate) fn export(session: &SessionSummary, rows: &[RowOut], cost: &CostReport, format: ExportFormat) -> String {
    match format {
        ExportFormat::Markdown => markdown(session, rows, cost),
        ExportFormat::Json => json_text(session, rows, cost),
    }
}

fn when(ms: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms)
        .map(|t| t.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| format!("{ms} ms"))
}

fn json_text(session: &SessionSummary, rows: &[RowOut], cost: &CostReport) -> String {
    let messages: Vec<Value> = rows
        .iter()
        .map(|r| {
            let mut m = json!({
                "idx": r.idx,
                "turn": r.turn,
                "role": r.role,
                "at_ms": r.at_ms,
                "content": r.content,
            });
            if let Some(step) = r.step {
                m["step"] = json!(step);
            }
            if let Some(meta) = &r.meta {
                m["provider"] = json!(meta.provider);
                m["model"] = json!(meta.model);
                m["usage"] = json!(meta.usage);
                m["latency_ms"] = json!(meta.latency_ms);
            }
            m
        })
        .collect();
    let doc = json!({
        "format": "marlowe-session/1",
        "session": session,
        "cost": cost,
        "messages": messages,
    });
    serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "{}".into())
}

fn markdown(session: &SessionSummary, rows: &[RowOut], cost: &CostReport) -> String {
    let mut out = String::new();
    let title = if session.title.trim().is_empty() { "Untitled session" } else { session.title.trim() };
    let _ = writeln!(out, "# {title}\n");
    let _ = writeln!(out, "- Session `{}`", session.id);
    if let Some(ws) = &session.workspace {
        let _ = writeln!(out, "- Workspace `{ws}`");
    }
    let _ = writeln!(out, "- Started {} · {} turns · {} model calls", when(session.created_ms), session.turns, cost.calls);
    for m in &cost.by_model {
        let _ = writeln!(out, "- {} · {} calls · {} in, {} out{}", m.model, m.calls, m.usage.input, m.usage.output, money(m.usage.cost_usd));
    }
    let _ = writeln!(out, "- Tokens {} in, {} out{}", cost.usage.input, cost.usage.output, money(cost.usage.cost_usd));

    // Results by call id, so each sits under the call that asked for it.
    let mut results: HashMap<&str, (&str, bool)> = HashMap::new();
    for r in rows {
        for p in &r.content {
            if let Part::ToolResult(t) = p {
                results.insert(t.call_id.as_str(), (t.content.as_str(), t.is_error));
            }
        }
    }

    let mut current: Option<u32> = None;
    for r in rows {
        if r.role == Role::Tool {
            continue;
        }
        if current != Some(r.turn) {
            current = Some(r.turn);
            blank(&mut out);
            let _ = writeln!(out, "## Turn {} · {}", r.turn, when(r.at_ms));
        }
        match r.role {
            Role::User => {
                blank(&mut out);
                let _ = writeln!(out, "**You**\n");
                out.push_str(&body(r));
            }
            Role::Assistant => {
                let who = match &r.meta {
                    Some(m) => format!(
                        "**Marlowe** ({}/{} · {} in, {} out · {} ms)",
                        m.provider, m.model, m.usage.input, m.usage.output, m.latency_ms
                    ),
                    None => "**Marlowe**".to_string(),
                };
                blank(&mut out);
                let _ = writeln!(out, "{who}\n");
                out.push_str(&body(r));
                for call in r.content.iter().filter_map(|p| if let Part::ToolCall(c) = p { Some(c) } else { None }) {
                    out.push_str(&tool_call(call, results.get(call.id.as_str()).copied()));
                }
            }
            Role::System => {
                blank(&mut out);
                let _ = writeln!(out, "**System**\n");
                out.push_str(&body(r));
            }
            Role::Tool => {}
        }
    }
    out
}

/// Ends `out` with exactly one blank line.
fn blank(out: &mut String) {
    while !out.ends_with("\n\n") {
        out.push('\n');
    }
}

fn money(c: Option<f64>) -> String {
    match c {
        Some(c) if c > 0.0 => format!(" · ${c:.4}"),
        Some(_) => " · free".to_string(),
        None => String::new(),
    }
}

/// A message's text and image notes. Reasoning is left out of the Markdown.
fn body(r: &RowOut) -> String {
    let mut out = String::new();
    for p in &r.content {
        match p {
            Part::Text { text } if !text.trim().is_empty() => {
                out.push_str(text.trim_end());
                out.push_str("\n\n");
            }
            Part::Image(i) => {
                let _ = writeln!(out, "[image {} {}x{}]\n", i.mime, i.w, i.h);
            }
            _ => {}
        }
    }
    out
}

fn tool_call(call: &ToolCall, result: Option<(&str, bool)>) -> String {
    let mut args = call.args.to_string();
    if args.chars().count() > ARGS_CHARS {
        args = args.chars().take(ARGS_CHARS).collect::<String>() + "…";
    }
    let mut out = format!("- `{}` `{}`\n", call.name, args.replace('`', "'"));
    match result {
        Some((text, is_error)) => {
            let shown: String = text.chars().take(RESULT_CHARS).collect();
            let cut = text.chars().count() > RESULT_CHARS;
            let fence = "`".repeat(3.max(longest_run(&shown) + 1));
            let _ = writeln!(out, "  - {}", if is_error { "failed" } else { "result" });
            let _ = writeln!(out, "\n{fence}");
            out.push_str(shown.trim_end());
            let _ = writeln!(out, "{}\n{fence}\n", if cut { "\n… (cut; the JSON export has it whole)" } else { "" });
        }
        None => out.push_str("  - no result recorded\n"),
    }
    out
}

fn longest_run(s: &str) -> usize {
    let (mut best, mut run) = (0, 0);
    for c in s.chars() {
        run = if c == '`' { run + 1 } else { 0 };
        best = best.max(run);
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_ops::cost;
    use crate::store::CallMeta;
    use marlowe_model::{ModelRef, ToolResult, Usage};
    use marlowe_proto::SessionId;

    fn row(idx: u32, role: Role, turn: u32, at_ms: i64, content: Vec<Part>, meta: Option<CallMeta>) -> RowOut {
        RowOut { idx, role, content, at_ms, turn, step: meta.as_ref().map(|_| 1), meta, mem_seq: None }
    }

    fn text(s: &str) -> Part {
        Part::Text { text: s.into() }
    }

    /// A small fixed session: a question answered with a tool call and its result, then a
    /// follow-up with a failing call and an awkward result.
    fn fixture() -> (SessionSummary, Vec<RowOut>) {
        let meta = |input, output, ms| CallMeta {
            provider: "ollama".into(),
            model: "marlowe-dusk".into(),
            usage: Usage { input, output, ..Usage::default() },
            latency_ms: ms,
        };
        let rows = vec![
            row(0, Role::User, 0, 1_760_000_000_000, vec![text("what does main.rs do?")], None),
            row(
                1,
                Role::Assistant,
                0,
                1_760_000_001_000,
                vec![
                    Part::Thinking(marlowe_model::Thinking { text: "private reasoning".into(), ..Default::default() }),
                    text("Let me read it."),
                    Part::ToolCall(ToolCall { id: "c1".into(), name: "read".into(), args: json!({"path": "src/main.rs"}) }),
                ],
                Some(meta(1200, 40, 900)),
            ),
            row(2, Role::Tool, 0, 1_760_000_002_000, vec![Part::ToolResult(ToolResult::text("c1", "fn main() {}\n", false))], None),
            row(3, Role::Assistant, 0, 1_760_000_003_000, vec![text("It is an empty entry point.")], Some(meta(1300, 12, 400))),
            row(4, Role::User, 1, 1_760_000_100_000, vec![text("run the tests")], None),
            row(
                5,
                Role::Assistant,
                1,
                1_760_000_101_000,
                vec![Part::ToolCall(ToolCall { id: "c2".into(), name: "bash".into(), args: json!({"command": "cargo test"}) })],
                Some(meta(1500, 20, 600)),
            ),
            row(
                6,
                Role::Tool,
                1,
                1_760_000_102_000,
                vec![Part::ToolResult(ToolResult::text("c2", "error: no `Cargo.toml` here\n```\nnested fence\n```\n", true))],
                None,
            ),
        ];
        let session = SessionSummary {
            id: SessionId("0123456789abcdef".into()),
            title: "main.rs".into(),
            workspace: Some("C:/ws/demo".into()),
            created_ms: 1_760_000_000_000,
            updated_ms: 1_760_000_102_000,
            turns: 2,
        };
        (session, rows)
    }

    fn fixture_cost(rows: &[RowOut]) -> CostReport {
        cost::report(rows, &|_: &ModelRef| cost::Price { pricing: None, local: true })
    }

    #[test]
    fn markdown_matches_the_golden_file() {
        let (session, rows) = fixture();
        let md = export(&session, &rows, &fixture_cost(&rows), ExportFormat::Markdown);
        assert_eq!(md, include_str!("../../tests/golden/f2_export.md").replace("\r\n", "\n"));
    }

    #[test]
    fn json_matches_the_golden_file_and_is_complete() {
        let (session, rows) = fixture();
        let text = export(&session, &rows, &fixture_cost(&rows), ExportFormat::Json);
        assert_eq!(text, include_str!("../../tests/golden/f2_export.json").replace("\r\n", "\n").trim_end());
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["messages"].as_array().unwrap().len(), 7);
        assert!(text.contains("private reasoning"), "the JSON keeps the reasoning the Markdown leaves out");
    }
}
