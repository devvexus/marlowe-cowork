//! Pure helpers that read a [`ProjectState`] for the screen. None of them names a project kind, a
//! phase or a step: they read the item tree, the phases and steps, the agents, the gates, the forms
//! and the kind template's rollups, so a new kind renders with no new code here.
//!
//! The terminal client draws a project from them: rollups that count only settled items, the
//! activity spark, the problems of a run, the parts of a deliverable that failed and the phase
//! output drawn through its own form.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::registry::Registry;
use crate::state::*;
use crate::view::{self, Group, Row};

/// The six-cell activity spark, from none running to six or more.
pub const SPARK: [char; 7] = ['⣀', '⣄', '⣤', '⣦', '⣶', '⣷', '⣿'];

/// The item has reached a state it will not leave by itself and counts in a rollup.
pub fn is_settled(s: ItemStatus) -> bool {
    matches!(s, ItemStatus::Done | ItemStatus::Failed | ItemStatus::Skipped | ItemStatus::Cancelled)
}

/// A phase's name with its round (`DataCollection r2`).
pub fn phase_name(ph: &Phase) -> String {
    if ph.round > 1 {
        format!("{} r{}", ph.ty, ph.round)
    } else {
        ph.ty.clone()
    }
}

/// Capital letters of a CamelCase name (`DataCollection` is `DC`); a name with one capital stays whole.
pub fn initials(name: &str) -> String {
    let caps: String = name.chars().filter(|c| c.is_ascii_uppercase()).collect();
    if caps.chars().count() > 1 {
        caps
    } else {
        name.to_string()
    }
}

/// How long the project has run: to `finished_ms`, else to `now_ms`.
pub fn elapsed_ms(state: &ProjectState, now_ms: u64) -> u64 {
    state.finished_ms.unwrap_or(now_ms).saturating_sub(state.created_ms)
}

/// A phase's span: start to end, or to `now_ms` while it runs.
pub fn phase_span_ms(ph: &Phase, now_ms: u64) -> Option<u64> {
    ph.started_ms.map(|s| ph.ended_ms.unwrap_or(now_ms).saturating_sub(s))
}

/// The gate a phase raised that still waits for the user.
pub fn open_gate_of<'a>(state: &'a ProjectState, phase_id: &str) -> Option<&'a Gate> {
    state.gates.iter().find(|g| g.status == GateStatus::Open && g.phase == phase_id)
}

// ---- rollups ------------------------------------------------------------------------------------

/// One overview row counted over settled items, with what is still in flight beside it.
#[derive(Debug, Clone, PartialEq)]
pub struct Split {
    /// The row title (`Sources`).
    pub title: String,
    /// Every item (or agent) counted, in flight included.
    pub total: u32,
    /// The settled items grouped by the rollup's field.
    pub groups: Vec<Group>,
    /// Items running now.
    pub running: u32,
    /// Items waiting.
    pub queued: u32,
}

/// The kind template's rollups over settled items only (done, failed, skipped, cancelled), plus
/// how many are running and queued. A half-finished item never changes a group, and the numbers
/// are true while the project runs. Agents roll up by status.
pub fn rollups_split(state: &ProjectState, registry: &Registry, now_ms: u64) -> Vec<Split> {
    let declared = registry.kind(&state.kind).map(|k| k.rollups.clone()).unwrap_or_default();
    let mut out = Vec::new();
    if declared.is_empty() {
        for kind in state.order.keys() {
            let items = state.items_of_kind(kind);
            let mut counts = BTreeMap::new();
            for i in items.iter().filter(|i| is_settled(i.status)) {
                *counts.entry(i.status.as_str().to_string()).or_insert(0u32) += 1;
            }
            out.push(Split {
                title: kind.clone(),
                total: items.len() as u32,
                groups: view::make_groups(counts, None, true),
                running: items.iter().filter(|i| i.status == ItemStatus::Running).count() as u32,
                queued: items.iter().filter(|i| matches!(i.status, ItemStatus::Queued | ItemStatus::Blocked)).count() as u32,
            });
        }
        return out;
    }
    for r in declared {
        if r.over == "agents" {
            let v = view::agents_rollup(&r.title, state, &r.by);
            out.push(Split { title: v.title, total: v.total, groups: v.groups, running: 0, queued: 0 });
            continue;
        }
        let items = state.items_of_kind(&r.over);
        let mut counts = BTreeMap::new();
        for i in items.iter().filter(|i| is_settled(i.status)) {
            let v = view::item_value(i, &r.by, now_ms).unwrap_or_else(|| "none".into());
            *counts.entry(v).or_insert(0u32) += 1;
        }
        let field = view::enum_field(state, &r.over, &r.by);
        out.push(Split {
            title: r.title.clone(),
            total: items.len() as u32,
            groups: view::make_groups(counts, field, r.by == "status" || r.by.ends_with("verdict")),
            running: items.iter().filter(|i| i.status == ItemStatus::Running).count() as u32,
            queued: items.iter().filter(|i| matches!(i.status, ItemStatus::Queued | ItemStatus::Blocked)).count() as u32,
        });
    }
    out
}
// ---- activity -----------------------------------------------------------------------------------

/// The agents running now, oldest first, each with the item it works on and how long it has run.
pub fn running_agents(state: &ProjectState, now_ms: u64) -> Vec<(&Agent, Option<&Item>, u64)> {
    let mut v: Vec<_> = state
        .agents
        .values()
        .filter(|a| a.status == AgentStatus::Running)
        .map(|a| (a, a.item.as_deref().and_then(|i| state.items.get(i)), now_ms.saturating_sub(a.started_ms)))
        .collect();
    v.sort_by_key(|(a, _, _)| (a.started_ms, a.id.clone()));
    v
}

/// The agents held back from calling their model (the GPU is busy, Windows is short of memory,
/// no slot is free), oldest first: each says what it waits for in its note.
pub fn waiting_agents(state: &ProjectState) -> Vec<&Agent> {
    let mut v: Vec<&Agent> = state.agents.values().filter(|a| a.status == AgentStatus::Waiting).collect();
    v.sort_by_key(|a| (a.started_ms, a.id.clone()));
    v
}

/// Six cells, each the most agents running at once in one 10 s slice of the last 60 s, read from
/// the agents' start and end times. Level `n` is `SPARK[min(n, 6)]`.
pub fn activity(state: &ProjectState, now_ms: u64) -> [u8; 6] {
    const SPAN: u64 = 60_000;
    const CELLS: u64 = 6;
    let slice = SPAN / CELLS;
    let mut out = [0u8; 6];
    for (i, cell) in out.iter_mut().enumerate() {
        let lo = now_ms as i64 - SPAN as i64 + i as i64 * slice as i64;
        let hi = lo + slice as i64;
        let mut t = lo;
        let mut best = 0usize;
        while t < hi {
            let n = state
                .agents
                .values()
                .filter(|a| (a.started_ms as i64) <= t && t < a.ended_ms.map_or(now_ms as i64 + 1, |e| e as i64))
                .count();
            best = best.max(n);
            t += 250;
        }
        *cell = best.min(SPARK.len() - 1) as u8;
    }
    out
}

/// The spark as glyphs; `still` (waiting on the user) draws the lowest cell six times.
pub fn spark(levels: [u8; 6], still: bool) -> String {
    if still {
        return SPARK[0].to_string().repeat(6);
    }
    levels.iter().map(|l| SPARK[(*l as usize).min(SPARK.len() - 1)]).collect()
}

// ---- problems -----------------------------------------------------------------------------------

/// Items that did not end done, grouped by phase, step, kind, status and the harness's reason.
#[derive(Debug, Clone, PartialEq)]
pub struct ProblemGroup {
    /// The phase instance.
    pub phase: String,
    /// The step.
    pub step: String,
    /// The item kind.
    pub kind: String,
    /// `failed`, `skipped`, `blocked` or `cancelled`.
    pub status: ItemStatus,
    /// The reason, with any address cut out of it.
    pub error: String,
    /// The item ids, oldest first.
    pub items: Vec<String>,
}

/// A reason with the address it names left out (`HTTP 403 on https://x/y` is `HTTP 403`).
pub fn plain_reason(e: &str) -> String {
    match e.find(" on http") {
        Some(i) => {
            let rest = &e[i + 4..];
            let end = rest.find(char::is_whitespace).map_or(rest.len(), |k| k);
            format!("{}{}", &e[..i], &rest[end..])
        }
        None => e.to_string(),
    }
}

/// Every item that did not end done, grouped. Only the harness's words are used as reasons.
pub fn problems(state: &ProjectState) -> Vec<ProblemGroup> {
    let mut groups: Vec<ProblemGroup> = Vec::new();
    for ph in &state.phases {
        let mut ids: Vec<&Item> = state
            .items
            .values()
            .filter(|i| i.phase == ph.id && matches!(i.status, ItemStatus::Failed | ItemStatus::Skipped | ItemStatus::Blocked | ItemStatus::Cancelled))
            .collect();
        ids.sort_by_key(|i| (i.live.started_ms.unwrap_or(u64::MAX), i.id.clone()));
        for it in ids {
            let err = plain_reason(it.error.as_deref().filter(|e| !e.is_empty()).unwrap_or(it.status.as_str()));
            match groups.iter_mut().find(|g| g.phase == ph.id && g.step == it.step && g.kind == it.kind && g.status == it.status && g.error == err) {
                Some(g) => g.items.push(it.id.clone()),
                None => groups.push(ProblemGroup {
                    phase: ph.id.clone(),
                    step: it.step.clone(),
                    kind: it.kind.clone(),
                    status: it.status,
                    error: err,
                    items: vec![it.id.clone()],
                }),
            }
        }
    }
    groups
}

// ---- the deliverable ----------------------------------------------------------------------------

fn employs_model(s: &StepState) -> bool {
    matches!(s.kind, StepKind::Fanout | StepKind::Merge)
}

/// The last phase that had employees working and made items: where the deliverable is written.
pub fn content_phase(state: &ProjectState) -> Option<&Phase> {
    state
        .phases
        .iter()
        .rev()
        .find(|p| p.steps.iter().any(|s| employs_model(s) && state.items.values().any(|i| i.phase == p.id && i.step == s.id)))
}

/// The item the last step of the content phase made: the project's result.
pub fn result_item(state: &ProjectState) -> Option<&Item> {
    let ph = content_phase(state)?;
    ph.steps
        .iter()
        .rev()
        .find_map(|s| state.order.values().flatten().rev().filter_map(|id| state.items.get(id)).find(|i| i.phase == ph.id && i.step == s.id))
}

/// The parts of the deliverable that failed: failed items of the content phase.
pub fn unwritten(state: &ProjectState) -> Vec<&Item> {
    let Some(ph) = content_phase(state) else { return Vec::new() };
    let mut v: Vec<&Item> = state.items.values().filter(|i| i.phase == ph.id && i.status == ItemStatus::Failed).collect();
    v.sort_by_key(|i| (i.live.started_ms.unwrap_or(u64::MAX), i.id.clone()));
    v
}

/// A phase's output drawn through the form its declaration names. The records table is the cell
/// whose field takes its columns from the plan.
#[derive(Debug, Clone, PartialEq)]
pub struct Output {
    /// The phase instance.
    pub phase: String,
    /// The output as a row (its table cells are the claims, sources, records).
    pub row: Row,
    /// The key of the table cell that takes its columns from the plan.
    pub records_key: Option<String>,
}

/// The latest phase output with a declared form, as a [`Row`].
pub fn output_row(state: &ProjectState, registry: &Registry, now_ms: u64) -> Option<Output> {
    for ph in state.phases.iter().rev() {
        let Some(Value::Object(data)) = &ph.output else { continue };
        let Some(form_id) = registry.phase_spec(&ph.ty).and_then(|s| s.output.clone()) else { continue };
        let Some(form) = state.forms.get(&form_id) else { continue };
        let mut item = Item::new(format!("out_{}", ph.id), "output", &form_id);
        item.status = ItemStatus::Done;
        item.phase = ph.id.clone();
        item.data = data.clone();
        let row = view::view(&item, state, now_ms);
        let records_key = form.fields.iter().find(|f| f.columns_from.is_some()).map(|f| f.key.clone());
        return Some(Output { phase: ph.id.clone(), row, records_key });
    }
    None
}

// ---- cards --------------------------------------------------------------------------------------

/// A payload scalar as words: `sources_read: 35` is `35 sources read`, `elapsed_ms` a duration.
pub fn summary_words(summary: &Map<String, Value>, skip_durations: bool) -> String {
    let mut parts = Vec::new();
    for (k, v) in summary {
        if let Some(stem) = k.strip_suffix("_ms") {
            if skip_durations {
                continue;
            }
            parts.push(format!("{} {}", stem.replace('_', " "), view::format_duration(v.as_u64().unwrap_or(0))));
        } else {
            parts.push(format!("{} {}", view::scalar_text(v), k.replace('_', " ")));
        }
    }
    parts.join(" · ")
}

/// A plan limit key in words (`max_sources` is `sources`).
pub fn limit_words(key: &str) -> String {
    key.strip_prefix("max_").unwrap_or(key).replace('_', " ")
}

/// The plan the card shows, with the same key-field lookup the plan card uses.
pub fn key_field_label(state: &ProjectState) -> Option<String> {
    let key = state.plan.key_field.as_deref()?;
    state.plan.fields.iter().find(|f| f.key == key).map(|f| f.label.clone())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::state::tests::sample;

    fn source(st: &mut ProjectState, key: &str, status: ItemStatus, relevancy: Option<&str>, step: &str, started: u64) -> String {
        let mut it = Item::new(crate::ids::item_id(&st.id, "source", key), "source", "analyze@1");
        it.parent = Some(st.items_of_kind("question")[0].id.clone());
        it.status = status;
        it.step = step.into();
        it.phase = "ph_dc".into();
        it.live.started_ms = Some(started);
        it.live.ended_ms = (status != ItemStatus::Running).then_some(started + 4_000);
        if let Some(r) = relevancy {
            it.data.insert("relevancy".into(), json!(r));
        }
        if status == ItemStatus::Failed {
            it.error = Some("HTTP 403 on https://example.org/x".into());
        }
        let id = it.id.clone();
        st.order.entry("source".into()).or_default().push(id.clone());
        st.items.insert(id.clone(), it);
        id
    }

    #[test]
    fn rollups_count_only_settled_items_and_report_the_rest_beside_them() {
        let mut st = sample();
        source(&mut st, "a", ItemStatus::Done, Some("High"), "analyze", 1_000);
        source(&mut st, "b", ItemStatus::Done, Some("Low"), "analyze", 2_000);
        source(&mut st, "c", ItemStatus::Running, Some("Critical"), "analyze", 3_000);
        let reg = Registry::builtin();
        let rs = rollups_split(&st, &reg, 10_000);
        let sources = rs.iter().find(|r| r.title == "Sources").expect("the kind's Sources rollup");
        assert_eq!(sources.total, 3);
        assert_eq!(sources.running, 1);
        let counted: u32 = sources.groups.iter().map(|g| g.count).sum();
        assert_eq!(counted, 2, "a running item's early verdict never shows");
        assert!(sources.groups.iter().all(|g| g.value != "Critical"));
    }

    #[test]
    fn problems_group_by_step_and_reason_without_the_address() {
        let mut st = sample();
        source(&mut st, "a", ItemStatus::Failed, None, "fetch", 1_000);
        source(&mut st, "b", ItemStatus::Failed, None, "fetch", 2_000);
        source(&mut st, "c", ItemStatus::Skipped, None, "fetch", 3_000);
        let g = problems(&st);
        let failed = g.iter().find(|g| g.status == ItemStatus::Failed).unwrap();
        assert_eq!((failed.items.len(), failed.error.as_str()), (2, "HTTP 403"));
        assert_eq!(plain_reason("robots.txt on https://x.com disallows it"), "robots.txt disallows it");
    }

    #[test]
    fn the_spark_follows_how_many_agents_ran_at_once() {
        let mut st = sample();
        for (i, (a, b)) in [(0u64, 100_000u64), (50_000, 100_000)].iter().enumerate() {
            st.agents.insert(
                format!("a_{i}"),
                Agent { id: format!("a_{i}"), role: "analyst".into(), model: "m".into(), step: "s".into(), item: None, status: AgentStatus::Done, attempt: 1, tokens: Tokens::default(), started_ms: *a, ended_ms: Some(*b), note: None },
            );
        }
        let lv = activity(&st, 100_000);
        assert_eq!(lv[0], 1);
        assert_eq!(lv[5], 2);
        assert_eq!(spark(lv, true), "⣀⣀⣀⣀⣀⣀", "waiting on the user is still");
    }

    #[test]
    fn a_label_that_filled_empty_falls_back_to_the_items_own_label() {
        let mut st = sample();
        let mut it = Item::new("i_000000aa".into(), "section", "section@1");
        it.label = "What is known about Qwen4?".into();
        it.status = ItemStatus::Failed;
        st.forms = Registry::builtin().forms_for("research");
        assert_eq!(view::item_title(&it, &st, 0), "What is known about Qwen4?");
    }
}
