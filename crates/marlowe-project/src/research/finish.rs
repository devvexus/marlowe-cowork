//! Finish: the delivered copy, workspace disposal, and what the project remembers.
//!
//! `deliver` copies `report.md` and `research.json` to `<home>\output\<slug>-<id>\` and leaves a
//! `deliverable` item for the relay to read. `archive` (generic) writes the final state, so a
//! finished project can be reopened from a stage. `dispose` applies the plan's `workspace` choice,
//! staying inside the project directory. `remember` hands extractive memories to a
//! [`MemorySink`]: no model is involved.

use std::sync::Arc;

use serde_json::json;

use super::report::verified_claims;
use super::{object, s, ResearchDeps};
use crate::engine::{Finished, TaskInput, Toolbox};
use crate::ids::derived_id;
use crate::state::ItemStatus;

/// Most findings a project remembers.
pub const FINDINGS_STORED: usize = 5;

/// One thing a finished project asks to be remembered.
///
/// There is deliberately no trust and no maturation field. The sink stores an entry with web and
/// agent trust, untrusted lineage (`sources` are its parents) and the normal maturation delay,
/// because everything here came from pages the run read. "Established" is a label on the memory's
/// text, not a bypass of maturation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryEntry {
    /// Stable for the same project and text, so storing twice stores once.
    pub key: String,
    /// The project id.
    pub project: String,
    /// `finding` (a verified claim) or `result` (the project's own summary).
    pub kind: String,
    /// What to remember.
    pub text: String,
    /// The addresses the text rests on (the lineage).
    pub sources: Vec<String>,
    /// Upheld with two independent roots and not contested.
    pub established: bool,
    /// The project read network content (always true for research).
    pub tainted: bool,
}

/// Where memories go. The binary implements it over the Mind.
pub trait MemorySink: Send + Sync {
    /// Stores one entry. An error is reported, never fatal to the project.
    fn remember(&self, entry: &MemoryEntry) -> Result<(), String>;
}

/// A sink that stores nothing.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoMemory;

impl MemorySink for NoMemory {
    fn remember(&self, _entry: &MemoryEntry) -> Result<(), String> {
        Ok(())
    }
}

/// A folder-safe name from a title.
fn slug(title: &str) -> String {
    let mut out = String::new();
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let out = out.trim_matches('-');
    let mut cut: String = out.chars().take(40).collect();
    // A word cut in half reads badly in a folder name: go back to the last hyphen.
    if out.chars().count() > 40 && out.chars().nth(40) != Some('-') {
        if let Some(i) = cut.rfind('-') {
            cut.truncate(i);
        }
    }
    let cut = cut.trim_end_matches('-').to_string();
    if cut.is_empty() { "project".into() } else { cut }
}

fn deliver(deps: &ResearchDeps, input: &TaskInput) -> Result<Finished, String> {
    let store = deps.store()?;
    let dir = store.dir(&input.project).map_err(|e| e.to_string())?;
    let report = dir.join("report.md");
    if !report.is_file() {
        return Err("there is no report to deliver".into());
    }
    let target = deps.output.join(format!("{}-{}", slug(&input.plan.title), input.project));
    std::fs::create_dir_all(&target).map_err(|e| format!("the delivery folder could not be made: {e}"))?;
    let mut files = Vec::new();
    for name in ["report.md", "research.json"] {
        let from = dir.join(name);
        if from.is_file() {
            std::fs::copy(&from, target.join(name)).map_err(|e| format!("{name} could not be copied: {e}"))?;
            files.push(name.to_string());
        }
    }
    let mut it = input.new_item("deliverable", "deliverable@1", &["deliverable"]);
    it.label = format!("Delivered to {}", target.display());
    it.data = object(json!({
        "dir": target.display().to_string(),
        "path": target.join("report.md").display().to_string(),
        "files": files,
        "send": input.plan.send,
    }));
    Ok(Finished { emits: vec![it], ..Default::default() })
}

fn dispose(deps: &ResearchDeps, input: &TaskInput) -> Result<Finished, String> {
    deps.store()?.dispose(&input.project, input.plan.workspace).map_err(|e| format!("the workspace could not be cleaned up: {e}"))?;
    Ok(Finished::default())
}

fn remember(deps: &ResearchDeps, input: &TaskInput) -> Result<Finished, String> {
    let (claims, urls, tainted, report) = input
        .project_handle
        .read(|st| {
            let claims = verified_claims(st);
            let urls: std::collections::HashMap<String, String> = st.items_of_kind("source").into_iter().map(|i| (i.id.clone(), s(&i.data, "source").to_string())).collect();
            let report = st.items_of_kind("report").into_iter().rev().find(|r| r.status == ItemStatus::Done).map(|r| r.data.clone());
            (claims, urls, st.tainted, report)
        })
        .map_err(|e| e.to_string())?;
    let mut entries: Vec<MemoryEntry> = Vec::new();
    for c in claims.iter().take(FINDINGS_STORED) {
        entries.push(MemoryEntry {
            key: derived_id("m", &[&input.project, "finding", &c.text]),
            project: input.project.clone(),
            kind: "finding".into(),
            text: c.text.clone(),
            sources: c.sources.iter().take(3).filter_map(|id| urls.get(id).cloned()).collect(),
            established: c.verdict == "upheld" && c.roots >= 2 && !c.contested,
            tainted,
        });
    }
    if let Some(r) = report {
        let text = format!("Research project \"{}\" finished with {} confidence. {}", input.plan.title, s(&r, "confidence"), s(&r, "first_paragraph"));
        entries.push(MemoryEntry { key: derived_id("m", &[&input.project, "result", &text]), project: input.project.clone(), kind: "result".into(), text, sources: Vec::new(), established: false, tainted });
    }
    let mut failed = 0;
    for e in &entries {
        if deps.memory.remember(e).is_err() {
            failed += 1;
        }
    }
    Ok(Finished { rejected: (failed > 0).then(|| json!({"memories_not_stored": failed})), ..Default::default() })
}

/// Registers `deliver`, `dispose` and `remember`.
pub fn register(t: &mut Toolbox, deps: &Arc<ResearchDeps>) {
    let d = deps.clone();
    t.code("deliver", move |input, _| deliver(&d, input));
    let d = deps.clone();
    t.code("dispose", move |input, _| dispose(&d, input));
    let d = deps.clone();
    t.code("remember", move |input, _| remember(&d, input));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_are_folder_safe_and_short() {
        assert_eq!(slug("Find the newest Kimi, DeepSeek & Qwen4 models!"), "find-the-newest-kimi-deepseek-qwen4", "cut at a word, not in the middle of one");
        assert_eq!(slug("  ../../etc/passwd  "), "etc-passwd");
        assert_eq!(slug("???"), "project");
        assert!(slug(&"word ".repeat(40)).chars().count() <= 40);
    }

    #[test]
    fn a_memory_entry_has_no_way_to_ask_for_trust_or_to_skip_maturation() {
        // The struct is the whole contract: every field is text, lineage or a label.
        let e = MemoryEntry { key: "m_1".into(), project: "p_1".into(), kind: "finding".into(), text: "t".into(), sources: vec![], established: true, tainted: true };
        let MemoryEntry { key: _, project: _, kind: _, text: _, sources: _, established: _, tainted: _ } = e;
    }
}
