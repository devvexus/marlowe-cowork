//! `marlowe crew plan <run>`: a deep research run's proposed plan and cost quote, and the answer
//! to it (approve, approve narrowed, edit from a file, reject).

use std::path::Path;
use std::time::{Duration, Instant};

use marlowe_proto::{CostQuote, Event, PlanDecision, Request, ResearchPlan, RunId};

use crate::crew_cmd::{connect, reply, run_line, run_updated, send, CREW_USAGE};

const OP_TIMEOUT: Duration = Duration::from_secs(30);

type CmdResult = Result<i32, String>;

/// What to do with the plan.
#[derive(Debug, Clone, PartialEq, Default)]
struct Answer {
    action: Option<String>,
    breadth: Option<u32>,
    depth: Option<u32>,
    sources: Option<u32>,
    json: bool,
    words: Vec<String>,
}

fn parse(args: &[String]) -> Result<Answer, String> {
    let mut a = Answer::default();
    let mut it = args.iter();
    while let Some(w) = it.next() {
        let mut num = |flag: &str| -> Result<u32, String> {
            let v = it.next().ok_or(format!("{flag} needs a number"))?;
            v.parse().map_err(|_| format!("{flag} needs a number, got {v:?}"))
        };
        match w.as_str() {
            "--breadth" => a.breadth = Some(num("--breadth")?),
            "--depth" => a.depth = Some(num("--depth")?),
            "--sources" => a.sources = Some(num("--sources")?),
            "--json" => a.json = true,
            "approve" | "edit" | "reject" if a.action.is_none() => a.action = Some(w.clone()),
            _ => a.words.push(w.clone()),
        }
    }
    Ok(a)
}

/// The plan with the narrowing flags applied; `None` when nothing changes.
fn narrowed(plan: &ResearchPlan, a: &Answer) -> Option<ResearchPlan> {
    let mut p = plan.clone();
    if let Some(b) = a.breadth {
        p.breadth = b;
    }
    if let Some(d) = a.depth {
        p.depth = d;
    }
    if let Some(s) = a.sources {
        p.source_budget = s;
    }
    (p != *plan).then_some(p)
}

/// The decision `a` describes for `plan`. Edits are validated here so a typo fails before the
/// daemon (which would keep waiting) sees it.
fn decision(plan: &ResearchPlan, a: &Answer) -> Result<PlanDecision, String> {
    match a.action.as_deref() {
        Some("approve") => match narrowed(plan, a) {
            Some(p) => {
                p.validate().map_err(|e| format!("the narrowed plan is not runnable: {e}"))?;
                Ok(PlanDecision::Edit { plan: p })
            }
            None => Ok(PlanDecision::Approve),
        },
        Some("reject") => {
            let reason = a.words.join(" ");
            Ok(PlanDecision::Reject { reason: (!reason.trim().is_empty()).then(|| reason.trim().to_string()) })
        }
        Some("edit") => {
            let file = a.words.first().ok_or("plan <run> edit <plan.json>")?;
            let text = std::fs::read_to_string(file).map_err(|e| format!("cannot read {file}: {e}"))?;
            let p: ResearchPlan = serde_json::from_str(&text).map_err(|e| format!("{file} is not a plan: {e}"))?;
            p.validate().map_err(|e| format!("the edited plan is not runnable: {e}"))?;
            Ok(PlanDecision::Edit { plan: p })
        }
        _ => Err(CREW_USAGE.into()),
    }
}

pub fn run(home: &Path, run: &RunId, args: &[String]) -> CmdResult {
    let a = parse(args)?;
    let mut c = connect(home)?;
    send(&mut c, &Request::WatchRun { run: run.clone() })?;
    // The pending plan, if any, arrives just before the transcript.
    let mut pending: Option<(RunId, ResearchPlan, CostQuote)> = None;
    let end = Instant::now() + OP_TIMEOUT;
    let info = loop {
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err("timed out waiting for the daemon".into());
        }
        let Some(env) = c.next_event(left.min(Duration::from_secs(5))).map_err(|e| e.to_string())? else { continue };
        if env.seq != 0 {
            continue;
        }
        match env.event {
            Event::Error { message, .. } => return Err(message),
            Event::PlanProposed { run, plan, quote } => pending = Some((run, plan, quote)),
            Event::RunTranscript { run, .. } => break run,
            _ => {}
        }
    };
    let Some((id, plan, quote)) = pending else {
        return Err(format!("run {} has no plan waiting for approval ({})", info.id.short(), info.status.as_str()));
    };
    if a.action.is_none() {
        if a.json {
            println!("{}", serde_json::to_string_pretty(&plan).map_err(|e| e.to_string())?);
        } else {
            print!("{}", render(&id, &plan, &quote));
            println!(
                "answer with `marlowe crew plan {} approve [--breadth N] [--depth N] [--sources N]`, `edit <plan.json>` \
                 (from --json) or `reject [reason]`",
                id.short()
            );
        }
        return Ok(0);
    }
    let d = decision(&plan, &a)?;
    let what = match &d {
        PlanDecision::Approve => "approved",
        PlanDecision::Edit { .. } => "approved as edited",
        PlanDecision::Reject { .. } => "rejected",
    };
    send(&mut c, &Request::ApprovePlan { run: id, decision: d })?;
    let info = reply(&mut c, OP_TIMEOUT, run_updated)?;
    println!("plan {what}: {}", run_line(&info));
    Ok(0)
}

/// The plan as the user reads it.
pub fn render(run: &RunId, plan: &ResearchPlan, quote: &CostQuote) -> String {
    let mut out = format!("research plan for run {} · {}\n", run.short(), plan.goal);
    for q in &plan.sub_questions {
        let ch: Vec<&str> = q.channels.iter().map(|c| c.as_str()).collect();
        out.push_str(&format!("  {}. {}  [{}]\n", q.id, q.question, ch.join(", ")));
        if let Some(r) = q.rationale.as_deref().filter(|r| !r.trim().is_empty()) {
            out.push_str(&format!("     why: {}\n", r.trim()));
        }
    }
    out.push_str(&format!(
        "  breadth {} · depth {} · source budget {}{}\n",
        plan.breadth,
        plan.depth,
        plan.source_budget,
        if plan.recalled.is_empty() { String::new() } else { format!(" · {} memories recalled", plan.recalled.len()) }
    ));
    if let Some(n) = plan.notes.as_deref().filter(|n| !n.trim().is_empty()) {
        out.push_str(&format!("  notes: {}\n", n.trim()));
    }
    let usd = quote.usd.map(|u| format!(" · ${u:.2}")).unwrap_or_default();
    out.push_str(&format!(
        "quote: about {} model tokens · {} fetches · {} min{usd}\n  basis: {}\n",
        quote.model_tokens, quote.fetches, quote.minutes, quote.basis
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use marlowe_proto::{ChannelId, SubQuestion};

    fn plan() -> ResearchPlan {
        ResearchPlan {
            goal: "LoRA ranks".into(),
            sub_questions: vec![SubQuestion {
                id: 1,
                question: "Which ranks work for 7B?".into(),
                channels: vec![ChannelId::Arxiv],
                rationale: Some("papers".into()),
                queries: Vec::new(),
            }],
            breadth: 4,
            depth: 1,
            source_budget: 40,
            recalled: vec![],
            notes: None,
        }
    }

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn approve_is_plain_unless_narrowed_and_narrowing_is_validated() {
        let p = plan();
        assert_eq!(decision(&p, &parse(&args("approve")).unwrap()).unwrap(), PlanDecision::Approve);
        let d = decision(&p, &parse(&args("approve --breadth 2 --sources 12")).unwrap()).unwrap();
        let PlanDecision::Edit { plan: e } = d else { panic!("{d:?}") };
        assert_eq!((e.breadth, e.depth, e.source_budget), (2, 1, 12));
        assert!(decision(&p, &parse(&args("approve --breadth 0")).unwrap()).is_err(), "breadth 0 is refused here");
    }

    #[test]
    fn reject_carries_its_reason_and_no_action_is_usage() {
        let p = plan();
        let d = decision(&p, &parse(&args("reject too broad")).unwrap()).unwrap();
        assert_eq!(d, PlanDecision::Reject { reason: Some("too broad".into()) });
        assert!(decision(&p, &parse(&args("")).unwrap()).is_err());
    }

    #[test]
    fn the_rendered_plan_names_questions_channels_and_the_quote() {
        let q = CostQuote { model_tokens: 120_000, fetches: 40, minutes: 22, usd: None, basis: "local".into() };
        let s = render(&RunId("abcdef0123".into()), &plan(), &q);
        assert!(s.contains("1. Which ranks work for 7B?  [arxiv]"), "{s}");
        assert!(s.contains("about 120000 model tokens · 40 fetches · 22 min"), "{s}");
    }
}
