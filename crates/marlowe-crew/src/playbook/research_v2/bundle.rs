//! `bundle.json` beside the report: the research eval's input (schema `marlowe.research-bundle/1`,
//! read by `tools/research_eval.py`). Each numbered reference carries the passages the report
//! cited from it and the stored document text, so citation accuracy is judged offline against
//! exactly what the run read.

use std::path::PathBuf;

use marlowe_proto::ResearchPlan;
use serde_json::{json, Value};

use super::compress::Compressed;
use super::explore::Explored;
use super::synth::Synthesis;
use super::Ctx;
use crate::CrewError;

pub const BUNDLE_FILE: &str = "bundle.json";
pub const BUNDLE_SCHEMA: &str = "marlowe.research-bundle/1";
/// Stored text kept per reference (the judge reads the best-overlapping chunks of it).
const DOC_CHARS: usize = 200_000;

fn clip_chars(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

pub(super) fn bundle(ctx: &Ctx<'_>, plan: &ResearchPlan, syn: &Synthesis, ex: &Explored, comp: &Compressed) -> Value {
    let references: Vec<Value> = syn
        .report
        .references
        .iter()
        .map(|r| {
            let passages: Vec<Value> = comp
                .claims
                .iter()
                .filter(|c| c.source == r.source && syn.cited_claims.contains(&c.id))
                .map(|c| {
                    let l = &c.quote.locator;
                    let section = l.section.as_deref().map(|s| format!(" · {s}")).unwrap_or_default();
                    json!({"text": c.quote.text, "locator": format!("{} bytes {}-{}{section}", l.doc.short(), l.start, l.end)})
                })
                .collect();
            let doc = ex.fetched(r.source).and_then(|f| f.doc.as_ref()).and_then(|d| ctx.read_doc(d));
            json!({
                "n": r.n,
                "title": r.title,
                "url": r.url,
                "identity": r.identity,
                "passages": passages,
                "document_text": doc.as_deref().map(|d| clip_chars(d, DOC_CHARS)),
            })
        })
        .collect();
    json!({
        "schema": BUNDLE_SCHEMA,
        "question_id": Value::Null,
        "question": ctx.goal,
        "report_markdown": syn.markdown,
        "references": references,
        "channels_used": plan.channels().iter().map(|c| c.as_str()).collect::<Vec<_>>(),
        "stats": {
            "counts": ctx.counts(),
            "effective_citations": syn.cited_claims.len(),
            "run_citation_accuracy": syn.report_checks.citation_accuracy(),
            "run_citation_accuracy_strict": syn.report_checks.strict_accuracy(),
            "coverage": syn.coverage,
            "removed_sentences": syn.removed.len(),
        },
    })
}

pub(super) fn write_bundle(
    ctx: &Ctx<'_>,
    plan: &ResearchPlan,
    syn: &Synthesis,
    ex: &Explored,
    comp: &Compressed,
) -> Result<PathBuf, CrewError> {
    let path = ctx.root().join(BUNDLE_FILE);
    let v = bundle(ctx, plan, syn, ex, comp);
    std::fs::write(&path, serde_json::to_string_pretty(&v).unwrap_or_default())?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    #[test]
    fn clipping_respects_char_boundaries() {
        assert_eq!(super::clip_chars("ééé", 2), "éé");
        assert_eq!(super::clip_chars("ab", 5), "ab");
    }
}
