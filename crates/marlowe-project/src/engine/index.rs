//! A light view of the state for the scheduler, built on the state actor's thread.
//!
//! The scheduler asks "which items of this kind are done, in which round, under which question"
//! after every task, so it reads a small index instead of cloning the document.

use std::collections::HashMap;

use serde_json::Value;

use crate::state::{GateStatus, Item, ItemStatus, ProjectState, StageStatus, StepState};

/// One item, as much as scheduling needs.
#[derive(Debug, Clone)]
pub(super) struct ItemInfo {
    pub id: String,
    pub status: ItemStatus,
    /// The step that made it.
    pub step: String,
    /// The round of the phase that made it.
    pub round: u32,
    /// The question it belongs to (empty for none).
    pub sub: String,
}

/// One phase instance.
#[derive(Debug, Clone)]
pub(super) struct PhaseInfo {
    pub id: String,
    pub ty: String,
    pub round: u32,
    pub status: StageStatus,
    pub steps: Vec<StepState>,
}

/// One gate.
#[derive(Debug, Clone)]
pub(super) struct GateInfo {
    pub id: String,
    pub phase: String,
    pub step: String,
    pub status: GateStatus,
    /// `payload.reason`, else the gate kind.
    pub reason: String,
    /// `payload.phase`, for a gate raised by code (failures).
    pub about: String,
}

/// The index.
#[derive(Debug, Clone)]
pub(super) struct Index {
    pub phases: Vec<PhaseInfo>,
    pub by_kind: HashMap<String, Vec<ItemInfo>>,
    pub gates: Vec<GateInfo>,
    /// The highest gate number in use.
    pub max_gate: u32,
}

/// The question an item belongs to: its own id for a question, else the nearest ancestor question's.
pub(super) fn sub_of(st: &ProjectState, item: &Item) -> String {
    let mut cur = item;
    for _ in 0..8 {
        if cur.kind == "question" {
            return cur.data.get("id").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| cur.id.clone());
        }
        match cur.parent.as_deref().and_then(|p| st.items.get(p)) {
            Some(p) => cur = p,
            None => break,
        }
    }
    String::new()
}

fn number_after(prefix: &str, id: &str) -> u32 {
    id.strip_prefix(prefix).and_then(|n| n.parse().ok()).unwrap_or(0)
}

impl Index {
    /// Builds the index; run it inside `ProjectHandle::read`.
    pub fn build(st: &ProjectState) -> Index {
        let rounds: HashMap<&str, u32> = st.phases.iter().map(|p| (p.id.as_str(), p.round)).collect();
        let mut by_kind: HashMap<String, Vec<ItemInfo>> = HashMap::new();
        for (kind, ids) in &st.order {
            let list = by_kind.entry(kind.clone()).or_default();
            for id in ids {
                if let Some(it) = st.items.get(id) {
                    list.push(ItemInfo {
                        id: it.id.clone(),
                        status: it.status,
                        step: it.step.clone(),
                        round: rounds.get(it.phase.as_str()).copied().unwrap_or(1),
                        sub: sub_of(st, it),
                    });
                }
            }
        }
        Index {
            phases: st
                .phases
                .iter()
                .map(|p| PhaseInfo { id: p.id.clone(), ty: p.ty.clone(), round: p.round, status: p.status, steps: p.steps.clone() })
                .collect(),
            by_kind,
            gates: st
                .gates
                .iter()
                .map(|g| GateInfo {
                    id: g.id.clone(),
                    phase: g.phase.clone(),
                    step: g.step.clone(),
                    status: g.status,
                    reason: g.payload.get("reason").and_then(Value::as_str).unwrap_or(g.kind.as_str()).to_string(),
                    about: g.payload.get("phase").and_then(Value::as_str).unwrap_or("").to_string(),
                })
                .collect(),
            max_gate: st.gates.iter().map(|g| number_after("g_", &g.id)).max().unwrap_or(0),
        }
    }

    /// The items of a kind, in display order.
    pub fn items(&self, kind: &str) -> &[ItemInfo] {
        self.by_kind.get(kind).map_or(&[], Vec::as_slice)
    }
}
