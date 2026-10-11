//! REMEMBER: the answer and the strongest established findings as Result memories, once.
//!
//! Trust is worst case over lineage. A finding is tainted when any source it rests on is a
//! network page or an untrusted recalled memory (and the run's own taint latch is added by the
//! host); the recalled memories it rests on are passed as its lineage, so the store caps its
//! trust by theirs and a correction can find it. A finding is stored as established only when
//! its group is on the goal and backed by two independent roots after page identities were
//! paired (two copies of one paper are one root).

use std::collections::BTreeSet;

use marlowe_proto::{ChannelId, Confidence, ResearchStage, StageState};
use marlowe_research::{Corroboration, SourceId, Standing, TrustBucket};

use super::compress::Compressed;
use super::explore::Explored;
use super::parse::clip;
use super::synth::{confidence_word, Evidence, Synthesis};
use super::types::{checkpoint_key, Finding};
use super::Ctx;
use crate::CrewError;

/// Established findings stored as memories, besides the answer.
const FINDINGS_STORED: usize = 5;

/// Whether a finding resting on `sources` read untrusted content.
pub(super) fn tainted(ex: &Explored, sources: &BTreeSet<SourceId>) -> bool {
    sources.iter().any(|s| {
        let route = ex.dedup.get(*s).and_then(|s| s.routes.first());
        match route {
            Some(r) if r.bucket() == TrustBucket::Memory => ex.state.memory_untrusted.contains(&r.url),
            Some(_) => true,
            None => true,
        }
    })
}

/// The recalled memory ids among `sources`: a finding's lineage in the store.
fn lineage(ex: &Explored, sources: &BTreeSet<SourceId>) -> Vec<String> {
    sources
        .iter()
        .filter_map(|s| ex.dedup.get(*s).and_then(|s| s.routes.first()))
        .filter(|r| r.channel == ChannelId::Memory)
        .filter_map(|r| r.url.strip_prefix("memory:").map(str::to_string))
        .collect()
}

/// Source URLs (or `memory:<id>`) for the record.
fn urls(ex: &Explored, sources: &BTreeSet<SourceId>) -> Vec<String> {
    sources.iter().filter_map(|s| ex.fetched(*s).map(|f| f.url.clone())).collect()
}

pub(super) fn remember(
    ctx: &Ctx<'_>,
    syn: &Synthesis,
    ex: &Explored,
    comp: &Compressed,
    ev: &Evidence,
) -> Result<Vec<String>, CrewError> {
    let key = checkpoint_key(ResearchStage::Remember, None);
    if let Some(ids) = ctx.load::<Vec<String>>(&key) {
        ctx.stage(
            ResearchStage::Remember,
            StageState::Done,
            ids.len() as u32,
            ids.len() as u32,
            Some("stored earlier".into()),
        );
        return Ok(ids);
    }
    let run = ctx.host.run_id();
    // The run's latch first: a run that saw untrusted search results was steered by them, even
    // when it read only memory.
    let latch = ctx.host.tainted();
    let cite = |n: &[u32]| -> String {
        let parts: Vec<String> = syn
            .report
            .references
            .iter()
            .filter(|r| n.contains(&r.n))
            .take(6)
            .map(|r| format!("{} <{}>", clip(&r.title, 80), r.url))
            .collect();
        if parts.is_empty() {
            String::new()
        } else {
            format!(" Sources: {}.", parts.join("; "))
        }
    };
    let refs_in = |text: &str| -> Vec<u32> {
        let mut v: Vec<u32> = Vec::new();
        let mut rest = text;
        while let Some(i) = rest.find('[') {
            let Some(j) = rest[i..].find(']') else { break };
            for n in super::parse::numbers(&rest[i + 1..i + j]) {
                if !v.contains(&n) {
                    v.push(n);
                }
            }
            rest = &rest[i + j + 1..];
        }
        v
    };
    let title = crate::run::title_from_goal(ctx.goal);
    let mut findings: Vec<(Finding, bool)> = Vec::new();
    let answer_refs = refs_in(&syn.report.answer);
    let answer_sources: BTreeSet<SourceId> = syn
        .report
        .references
        .iter()
        .filter(|r| answer_refs.contains(&r.n))
        .map(|r| r.source)
        .chain(syn.cited_claims.iter().filter_map(|c| comp.claim(*c)).map(|c| c.source))
        .collect();
    findings.push((
        Finding {
            run: run.clone(),
            text: format!(
                "Research \"{title}\" (run {}, confidence {}): {}{}",
                run.short(),
                confidence_word(syn.report.confidence),
                clip(&syn.report.answer, 1_000),
                cite(&answer_refs)
            ),
            standing: if syn.report.confidence == Confidence::High { Standing::Established } else { Standing::Suggested },
            references: answer_refs,
            derived_from: lineage(ex, &answer_sources),
            sources: urls(ex, &answer_sources),
        },
        latch || tainted(ex, &answer_sources),
    ));
    let mut strong: Vec<&Corroboration> = ev
        .corroboration
        .iter()
        .filter(|c| c.standing == Standing::Established && c.supporting_roots >= 2 && !ev.off_goal.contains_key(&c.group))
        .collect();
    strong.sort_by_key(|c| usize::MAX - c.supporting_roots);
    for c in strong.into_iter().take(FINDINGS_STORED) {
        let Some(g) = ev.groups.iter().find(|g| g.id == c.group) else { continue };
        let sources: BTreeSet<SourceId> =
            g.members.iter().filter_map(|(id, _)| comp.claim(*id)).map(|cl| cl.source).collect();
        let refs: Vec<u32> =
            syn.report.references.iter().filter(|r| sources.contains(&r.source)).map(|r| r.n).collect();
        findings.push((
            Finding {
                run: run.clone(),
                text: format!(
                    "Research finding (\"{title}\", run {}; established by {} independent sources): {}{}",
                    run.short(),
                    c.supporting_roots,
                    clip(&g.statement, 600),
                    cite(&refs)
                ),
                standing: Standing::Established,
                references: refs,
                derived_from: lineage(ex, &sources),
                sources: urls(ex, &sources),
            },
            latch || tainted(ex, &sources),
        ));
    }
    let mut ids = Vec::new();
    for (f, tainted) in &findings {
        match ctx.host.remember(f, *tainted) {
            Ok(id) => ids.push(id),
            Err(e) => ctx.stage(
                ResearchStage::Remember,
                StageState::Running,
                ids.len() as u32,
                findings.len() as u32,
                Some(format!("not stored: {e}")),
            ),
        }
    }
    ctx.save(&key, &format!("remembered {} findings", ids.len()), &ids);
    ctx.stage(
        ResearchStage::Remember,
        StageState::Done,
        ids.len() as u32,
        findings.len() as u32,
        Some(format!("{} findings stored as memories", ids.len())),
    );
    Ok(ids)
}
