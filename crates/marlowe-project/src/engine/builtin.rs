//! The generic functions every project kind uses: they know plans, rounds and gates, nothing
//! about sources or experiments.
//!
//! | Name | Kind | Does |
//! |---|---|---|
//! | `plan_expand` | code | makes a `question` item for each question of the plan that has none (a continuation's new ones) |
//! | `plan_card` | gate card | the plan as the user sees it: objective, questions, fields, limits, and the network grant in plain words |
//! | `plan_answer` | gate answer | approval records the decision and installs the run-scoped network grant |
//! | `checkin_card` | gate card | counts of what exists, why the gate opened, the options |
//! | `checkin_answer` | gate answer | approve writes the report; keep researching or steer starts a continuation round (or just goes on, for a checkin the user forced mid-run); budget and failure checkins continue, report or cancel |
//! | `archive` | code | writes the `final` snapshot |
//!
//! Research-specific cards (flagged additional sources, the lead's note) replace `checkin_card`
//! under the same name in a toolbox that registers its own.

use std::collections::BTreeMap;

use marlowe_proto::ProjectGateOption;
use serde_json::{json, Value};

use super::functions::{Effect, Finished, GateCard, GateCtx, TaskInput, Toolbox};
use crate::plan::{continue_plan, Continuation};
use crate::state::{Gate, GateAnswer, GateKind, Item, ItemStatus, Phase, PlanRecord, ProjectState, StageStatus};

/// Registers the generic functions.
pub fn register(t: &mut Toolbox) {
    t.code("plan_expand", plan_expand);
    t.code("archive", |input, _| {
        input.project_handle.archive("final").map_err(|e| format!("could not write the final snapshot: {e}"))?;
        Ok(Finished::default())
    });
    t.payload("plan_card", plan_card);
    t.answer("plan_answer", |st, gate, answer| plan_answer(st, gate, answer));
    t.payload("checkin_card", checkin_card);
    t.answer("checkin_answer", |st, gate, answer| checkin_answer(st, gate, answer));
}

fn plan_expand(input: &TaskInput, _cancel: &marlowe_model::CancelToken) -> Result<Finished, String> {
    let plan = input.plan.clone();
    let phase = input.phase.clone();
    let project = input.project.clone();
    let have = input.project_handle.read(move |st| st.items_of_kind("question").iter().map(|i| i.id.clone()).collect::<Vec<_>>()).map_err(|e| e.to_string())?;
    let mut out = Finished::default();
    for q in &plan.questions {
        let id = crate::ids::item_id(&project, "question", &q.id);
        if have.contains(&id) {
            continue;
        }
        let mut it = Item::new(id, "question", "question@1");
        it.phase = phase.clone();
        it.step = input.step.id.clone();
        it.status = ItemStatus::Done;
        it.label = q.text.clone();
        it.data.insert("id".into(), Value::String(q.id.clone()));
        it.data.insert("text".into(), Value::String(q.text.clone()));
        out.emits.push(it);
    }
    Ok(out)
}

/// The plan card: the plan as the user sees it, with the network grant in plain words.
pub fn plan_card(ctx: &GateCtx<'_>) -> GateCard {
    let p = &ctx.state.plan;
    let fields: Vec<Value> = p
        .fields
        .iter()
        .map(|f| {
            let mut o = json!({"key": f.key, "label": f.label});
            if !f.values.is_empty() {
                o["values"] = json!(f.values);
            }
            o
        })
        .collect();
    let payload = json!({
        "kind": "plan",
        "plan_no": p.plan_no,
        "title": p.title,
        "objective": p.objective,
        "questions": p.questions.iter().map(|q| json!({"id": q.id, "text": q.text})).collect::<Vec<_>>(),
        "fields": fields,
        "limits": p.limits,
        "deliverable": p.deliverable,
        "workspace": p.workspace,
        "send": p.send,
        "steer": p.steer,
        "grant": format!(
            "Approving lets this project read public https pages it finds, up to {}, honouring robots.txt. Anything else asks.",
            p.limits.max_fetches
        ),
    });
    GateCard { payload, options: GateKind::Plan.options(0) }
}

fn plan_answer(st: &ProjectState, gate: &Gate, answer: &GateAnswer) -> Result<Vec<Effect>, String> {
    if answer.choice != "approve" {
        return Err(format!("`{}` is not an answer to a plan card", answer.choice));
    }
    let plan = &st.plan;
    // A continuation's approval adds its own fetches to what the earlier rounds used.
    let used = st.grants.net.as_ref().map_or(0, |n| n.used);
    Ok(vec![
        Effect::PlanRecord { record: PlanRecord { plan_no: plan.plan_no, approved_ms: Some(gate.answered_ms.unwrap_or(gate.asked_ms)), decision: Some("approve".into()) } },
        Effect::NetGrant { quota: used + plan.limits.max_fetches },
    ])
}

fn counts_by_kind(st: &ProjectState) -> Value {
    let mut by: BTreeMap<&str, BTreeMap<&str, u32>> = BTreeMap::new();
    for it in st.items.values() {
        *by.entry(it.kind.as_str()).or_default().entry(it.status.as_str()).or_default() += 1;
    }
    json!(by)
}

/// The options of a checkin that is not the lead's: a budget or failure checkin offers continue,
/// report and cancel.
fn options_for(reason: &str) -> Vec<ProjectGateOption> {
    let opt = |id: &str, label: &str, needs_text: bool| ProjectGateOption { id: id.into(), label: label.into(), needs_text };
    match reason {
        "budget" => vec![opt("continue", "Continue with a larger budget", false), opt("report", "Write the report with what exists", false), opt("cancel", "Cancel", false)],
        "failures" => vec![opt("continue", "Go on with what worked", false), opt("report", "Write the report with what exists", false), opt("cancel", "Cancel", false)],
        _ => GateKind::Checkin.options(0),
    }
}

/// The generic checkin card: why it opened, counts of what exists, the options. A kind with
/// more to show (flagged sources, the lead's note) builds on it and registers its own.
pub fn checkin_card(ctx: &GateCtx<'_>) -> GateCard {
    let st = ctx.state;
    let payload = json!({
        "kind": "checkin",
        "reason": ctx.reason,
        "round": ctx.phase.round,
        "summary": {
            "items": counts_by_kind(st),
            "tokens": {"in": st.metrics.tokens.input, "out": st.metrics.tokens.out},
            "elapsed_ms": st.metrics.active_ms,
        },
        "options": options_for(ctx.reason).iter().map(|o| o.id.clone()).collect::<Vec<_>>(),
    });
    GateCard { payload, options: options_for(ctx.reason) }
}

fn checkin_answer(st: &ProjectState, gate: &Gate, answer: &GateAnswer) -> Result<Vec<Effect>, String> {
    let reason = gate.payload.get("reason").and_then(Value::as_str).unwrap_or("user_forced");
    match (reason, answer.choice.as_str()) {
        (_, "approve" | "report") => Ok(vec![Effect::JumpToReport]),
        ("budget", "continue") => Ok(vec![Effect::RaiseBudget { to: st.metrics.budget_raises + 1 }]),
        ("failures", "continue") => Ok(Vec::new()),
        ("lead_decision", "keep_researching" | "steer") => {
            let proposed: Vec<String> = gate
                .payload
                .get("proposed_questions")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|q| q.get("q").and_then(Value::as_str).or_else(|| q.as_str()).map(str::to_string)).collect())
                .unwrap_or_default();
            let questions = if !proposed.is_empty() {
                proposed
            } else if let Some(t) = answer.text.as_ref().filter(|t| t.chars().count() >= 10) {
                vec![t.chars().take(200).collect()]
            } else {
                return Err("there are no questions to continue with; steer with a sentence of at least ten characters".into());
            };
            // Sources a kind's card offered to read first (the generic card offers none).
            let seeds: Vec<String> = gate
                .payload
                .get("seed_sources")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).take(40).collect())
                .unwrap_or_default();
            let spec = Continuation { questions, inherit: Vec::new(), seed_sources: seeds, steer: answer.text.clone() };
            continuation_effects(st, &spec, gate.answered_ms.unwrap_or(gate.asked_ms))
        }
        ("budget" | "failures", other) => Err(format!("`{other}` is not an answer to a {reason} checkin")),
        // A checkin the user forced in the middle of the work: keep going (a steer is kept on the
        // plan for the next employee prompts to read as data).
        (_, "keep_researching" | "steer") => {
            let mut plan = st.plan.clone();
            if let Some(t) = answer.text.as_ref() {
                plan.steer = Some(t.chars().take(1000).collect());
                Ok(vec![Effect::SetPlan { plan: Box::new(plan) }])
            } else {
                Ok(Vec::new())
            }
        }
        (_, other) => Err(format!("`{other}` is not an answer to this checkin")),
    }
}

/// The effects of a continuation round: the next plan (inheriting the last), its plan record, and
/// the round's phases inserted ahead of the Report that waits. Each phase of the plan's list up
/// to (not including) its first Report or Finish is repeated for the new round; the plan phase of
/// a continuation needs no approval (the user just chose to continue).
pub fn continuation_effects(st: &ProjectState, spec: &Continuation, now_ms: u64) -> Result<Vec<Effect>, String> {
    let next = continue_plan(&st.plan, spec, now_ms).map_err(|e| e.to_string())?;
    let round = next.plan_no;
    let before = st.phases.iter().find(|p| p.status != StageStatus::Done && matches!(p.ty.as_str(), "Report" | "Finish")).map(|p| p.id.clone());
    let mut out = vec![
        Effect::PlanRecord { record: PlanRecord { plan_no: next.plan_no, approved_ms: Some(now_ms), decision: Some("continue".into()) } },
        Effect::SetPlan { plan: Box::new(next.clone()) },
    ];
    for ty in next.phases.iter().take_while(|t| !matches!(t.as_str(), "Report" | "Finish")) {
        out.push(Effect::InsertPhase { before: before.clone(), phase: Phase::pending(ty, round) });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::tests::sample;

    fn gate(reason: &str, payload_extra: Value) -> Gate {
        let mut payload = json!({"kind": "checkin", "reason": reason});
        if let (Some(p), Some(e)) = (payload.as_object_mut(), payload_extra.as_object()) {
            p.extend(e.clone());
        }
        Gate {
            id: "g_1".into(),
            kind: GateKind::Checkin,
            status: crate::state::GateStatus::Answered,
            payload,
            options: Vec::new(),
            answer: None,
            asked_ms: 10,
            answered_ms: Some(20),
            phase: "ph_check".into(),
            step: "checkin_card".into(),
        }
    }

    fn ans(choice: &str, text: Option<&str>) -> GateAnswer {
        GateAnswer { choice: choice.into(), text: text.map(str::to_string) }
    }

    #[test]
    fn approving_a_plan_installs_a_run_scoped_grant_sized_by_the_plan() {
        let st = sample();
        let g = gate("plan", json!({}));
        let fx = plan_answer(&st, &g, &ans("approve", None)).unwrap();
        assert_eq!(fx[1], Effect::NetGrant { quota: 60 });
        assert!(matches!(&fx[0], Effect::PlanRecord { record } if record.plan_no == 1 && record.decision.as_deref() == Some("approve")));
        assert!(plan_answer(&st, &g, &ans("keep_researching", None)).is_err());
    }

    #[test]
    fn the_plan_card_says_what_approval_grants_in_plain_words() {
        let st = sample();
        let phase = st.phases[0].clone();
        let step = crate::registry::Registry::builtin().phase_spec("Plan").unwrap().step("plan_card").unwrap().clone();
        let card = plan_card(&GateCtx { state: &st, phase: &phase, step: &step, reason: "plan" });
        assert_eq!(card.payload["questions"].as_array().unwrap().len(), 3);
        assert!(card.payload["grant"].as_str().unwrap().contains("public https pages it finds, up to 60, honouring robots.txt"));
        assert_eq!(card.options.iter().map(|o| o.id.as_str()).collect::<Vec<_>>(), ["approve", "cancel"]);
    }

    #[test]
    fn a_continuation_inserts_the_rounds_phases_before_the_waiting_report() {
        let mut st = sample();
        for p in st.phases.iter_mut().filter(|p| p.ty != "Report" && p.ty != "Finish") {
            p.status = StageStatus::Done;
        }
        let spec = Continuation { questions: vec!["What did the Kimi K2.6 release change for long context?".into()], ..Default::default() };
        let fx = continuation_effects(&st, &spec, 99).unwrap();
        let inserted: Vec<(String, u32)> = fx
            .iter()
            .filter_map(|e| match e {
                Effect::InsertPhase { before, phase } => {
                    assert_eq!(before.as_deref(), Some("ph_report"));
                    Some((phase.id.clone(), phase.round))
                }
                _ => None,
            })
            .collect();
        assert_eq!(inserted, [("ph_plan_2".to_string(), 2), ("ph_dc_2".into(), 2), ("ph_dv_2".into(), 2), ("ph_think_2".into(), 2)]);
        let Effect::SetPlan { plan } = fx.iter().find(|e| matches!(e, Effect::SetPlan { .. })).unwrap() else { unreachable!() };
        assert_eq!((plan.plan_no, plan.questions[0].id.as_str(), plan.parent_plan), (2, "q4", Some(1)));
    }

    #[test]
    fn checkin_answers_by_reason() {
        let st = sample();
        assert_eq!(checkin_answer(&st, &gate("user_forced", json!({})), &ans("approve", None)).unwrap(), [Effect::JumpToReport]);
        assert_eq!(checkin_answer(&st, &gate("budget", json!({})), &ans("continue", None)).unwrap(), [Effect::RaiseBudget { to: 1 }]);
        assert_eq!(checkin_answer(&st, &gate("failures", json!({})), &ans("continue", None)).unwrap(), []);
        assert_eq!(checkin_answer(&st, &gate("user_forced", json!({})), &ans("keep_researching", None)).unwrap(), [], "a forced checkin in mid-run just goes on");
        let steered = checkin_answer(&st, &gate("user_forced", json!({})), &ans("steer", Some("focus on benchmarks"))).unwrap();
        assert!(matches!(&steered[0], Effect::SetPlan { plan } if plan.steer.as_deref() == Some("focus on benchmarks")));
        assert!(checkin_answer(&st, &gate("budget", json!({})), &ans("steer", Some("x"))).is_err(), "budget checkins take no steer");
    }

    #[test]
    fn keep_researching_after_the_lead_uses_its_proposed_questions_else_the_steer_text() {
        let st = sample();
        let g = gate("lead_decision", json!({"proposed_questions": [{"q": "Which Kimi models support 1M context?"}]}));
        let fx = checkin_answer(&st, &g, &ans("keep_researching", None)).unwrap();
        assert!(fx.iter().any(|e| matches!(e, Effect::InsertPhase { phase, .. } if phase.id == "ph_dc_2")));
        let none = gate("lead_decision", json!({}));
        assert!(checkin_answer(&st, &none, &ans("keep_researching", None)).unwrap_err().contains("no questions"));
        let fx = checkin_answer(&st, &none, &ans("steer", Some("what changed in the sparse attention design"))).unwrap();
        let Effect::SetPlan { plan } = fx.iter().find(|e| matches!(e, Effect::SetPlan { .. })).unwrap() else { unreachable!() };
        assert_eq!(plan.questions[0].text, "what changed in the sparse attention design");
    }
}
