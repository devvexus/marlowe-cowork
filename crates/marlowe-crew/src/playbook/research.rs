//! deep_research: plan (Researcher: 3–6 sub-questions) → parallel scouts (one per sub-question,
//! claims with citations) → verify (Reviewer tries to refute each load-bearing claim; refuted
//! claims are dropped, contested ones flagged) → synthesize (Researcher writes a cited report to
//! `<sandbox>\report.md`) → Dispatch with confidence and citations. The report is copied into
//! `<workspace>\research\<slug>.md` only when the user accepts it (MergeRun).

use std::collections::HashMap;

use marlowe_proto::{Citation, Confidence, Dispatch};

use super::{confidence_line, list_items, markdown_links, PlaybookConfig, StepRunner, StepSpec};
use crate::CrewError;

/// The report's file name inside the sandbox.
pub const REPORT_FILE: &str = "report.md";

/// Sub-questions a plan must yield before the playbook pads with the goal itself.
const MIN_SUBQUESTIONS: usize = 1;

/// Read-only workspace tools for the plan and synthesis steps.
const LOOK: [&str; 3] = ["read", "glob", "grep"];

/// A scout's claim, numbered across the whole run.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Claim {
    n: usize,
    /// Index into the sub-questions.
    sub: usize,
    text: String,
}

/// The reviewer's verdict on one claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Supported,
    Refuted(String),
    Contested(String),
    /// The source could not be reached or no verdict was given.
    Unverified(String),
}

/// Runs the deep_research playbook for `goal`.
pub fn deep_research(runner: &dyn StepRunner, goal: &str, cfg: &PlaybookConfig) -> Result<Dispatch, CrewError> {
    // Plan.
    let plan = runner.step(StepSpec {
        key: "plan".into(),
        role: "researcher".into(),
        label: "plan".into(),
        prompt: plan_prompt(goal),
        tools: Some(LOOK.iter().map(|t| t.to_string()).collect()),
    })?;
    let mut subs: Vec<String> = list_items(&plan.text);
    subs.truncate(cfg.max_scouts.max(1));
    if subs.len() < MIN_SUBQUESTIONS {
        subs = vec![goal.trim().to_string()];
    }

    // Scouts, one per sub-question, concurrently.
    let n = subs.len();
    let specs = subs
        .iter()
        .enumerate()
        .map(|(i, q)| StepSpec {
            key: format!("scout:{}", i + 1),
            role: "scout".into(),
            label: format!("scout {}/{n}", i + 1),
            prompt: scout_prompt(goal, q),
            tools: None,
        })
        .collect();
    let mut claims: Vec<Claim> = Vec::new();
    let mut empty: Vec<usize> = Vec::new();
    let mut failed = 0;
    for (i, r) in runner.parallel(specs).into_iter().enumerate() {
        match r {
            Ok(r) => {
                let items = list_items(&r.text);
                if items.is_empty() {
                    empty.push(i);
                }
                for text in items {
                    claims.push(Claim { n: claims.len() + 1, sub: i, text });
                }
            }
            Err(CrewError::Cancelled) => return Err(CrewError::Cancelled),
            Err(CrewError::Stopped) => return Err(CrewError::Stopped),
            Err(_) => {
                failed += 1;
                empty.push(i);
            }
        }
    }
    if failed == n {
        return Err(CrewError::Step { step: "scout".into(), message: format!("all {n} scouts failed") });
    }

    // Verify.
    let verdicts = if claims.is_empty() {
        HashMap::new()
    } else {
        let review = runner.step(StepSpec {
            key: "verify".into(),
            role: "reviewer".into(),
            label: "verify".into(),
            prompt: verify_prompt(goal, &claims),
            tools: None,
        })?;
        parse_verdicts(&review.text)
    };
    let verdict_of = |c: &Claim| {
        verdicts.get(&c.n).cloned().unwrap_or_else(|| Verdict::Unverified("the reviewer gave no verdict".into()))
    };
    let (kept, dropped): (Vec<&Claim>, Vec<&Claim>) =
        claims.iter().partition(|c| !matches!(verdict_of(c), Verdict::Refuted(_)));
    let supported = kept.iter().filter(|c| verdict_of(c) == Verdict::Supported).count();

    // Synthesize.
    runner.progress("synthesize");
    let synth = runner.step(StepSpec {
        key: "synthesize".into(),
        role: "researcher".into(),
        label: "synthesize".into(),
        prompt: synth_prompt(goal, &subs, &kept, &dropped, &empty, &verdict_of),
        tools: Some(LOOK.iter().map(|t| t.to_string()).collect()),
    })?;
    let report = synth.text.trim().to_string();
    let root = &runner.sandbox().root;
    std::fs::create_dir_all(root)?;
    let path = root.join(REPORT_FILE);
    std::fs::write(&path, format!("{report}\n"))?;

    // Citations come from the report, minus sources that only refuted claims cited.
    let refuted_only: Vec<String> = dropped
        .iter()
        .flat_map(|c| markdown_links(&c.text))
        .map(|c| c.url_or_path)
        .filter(|t| !kept.iter().any(|k| k.text.contains(t.as_str())))
        .collect();
    let mut citations: Vec<Citation> =
        markdown_links(&report).into_iter().filter(|c| !refuted_only.contains(&c.url_or_path)).collect();
    if citations.is_empty() {
        for c in kept.iter().flat_map(|c| markdown_links(&c.text)) {
            if !citations.iter().any(|x| x.url_or_path == c.url_or_path) {
                citations.push(c);
            }
        }
    }

    let stated = confidence_line(&report).unwrap_or(Confidence::Medium);
    let cap = if supported == 0 {
        Confidence::Low
    } else if citations.len() < 2 {
        Confidence::Medium
    } else {
        Confidence::High
    };
    let contested = kept.len() - supported;
    let mut summary = first_paragraph(&report, 700);
    summary.push_str(&format!(
        "\n\n{} of {} claims supported, {} contested or unverified, {} refuted and dropped; {} scout{}.",
        supported,
        claims.len(),
        contested,
        dropped.len(),
        n,
        if n == 1 { "" } else { "s" }
    ));
    if !empty.is_empty() {
        summary.push_str(&format!(" {} sub-question(s) came back empty.", empty.len()));
    }
    Ok(Dispatch {
        summary,
        artifacts: vec![path.display().to_string()],
        confidence: min_confidence(stated, cap),
        citations,
        diff: None,
    })
}

fn plan_prompt(goal: &str) -> String {
    format!(
        "Goal: {goal}\n\n\
         Plan the investigation. Reply with 3 to 6 sub-questions as a numbered list, one per line, \
         the most decisive first. Each one goes to its own scout, so each must stand on its own."
    )
}

fn scout_prompt(goal: &str, question: &str) -> String {
    format!(
        "The overall goal, for context only: {goal}\n\n\
         Your sub-question: {question}\n\n\
         Reply with your claims as a numbered list, one per line, each with its citation as a \
         markdown link. Mark the claims the answer depends on `(load-bearing)`."
    )
}

fn verify_prompt(goal: &str, claims: &[Claim]) -> String {
    let mut p = format!(
        "Goal: {goal}\n\n\
         Scouts returned the claims below. Try to refute each load-bearing claim against its cited \
         source, and check the others where it is cheap. Give one verdict per claim, on its own \
         line, by the claim's number: `N. supported`, `N. refuted: <why>`, `N. contested: <why>` \
         or `N. unverified: <why>`.\n\nClaims:\n"
    );
    for c in claims {
        p.push_str(&format!("{}. {}\n", c.n, c.text));
    }
    p
}

fn synth_prompt(
    goal: &str,
    subs: &[String],
    kept: &[&Claim],
    dropped: &[&Claim],
    empty: &[usize],
    verdict_of: &dyn Fn(&Claim) -> Verdict,
) -> String {
    let mut p = format!(
        "Goal: {goal}\n\n\
         Write the report from the checked claims below and nothing else. Contested and unverified \
         claims stay, marked as such with the reason. Refuted claims are listed only so you do not \
         reuse them. Reply with the full report in markdown; it is saved as `report.md`. Answer the \
         goal in the first paragraph and end with the `Confidence:` line.\n"
    );
    for (i, q) in subs.iter().enumerate() {
        p.push_str(&format!("\n### {}. {q}\n", i + 1));
        let mine: Vec<&&Claim> = kept.iter().filter(|c| c.sub == i).collect();
        if mine.is_empty() {
            p.push_str(if empty.contains(&i) {
                "- (the scout found nothing usable)\n"
            } else {
                "- (no claim survived review)\n"
            });
        }
        for c in mine {
            match verdict_of(c) {
                Verdict::Contested(why) => p.push_str(&format!("- {} [contested: {why}]\n", c.text)),
                Verdict::Unverified(why) => p.push_str(&format!("- {} [unverified: {why}]\n", c.text)),
                _ => p.push_str(&format!("- {}\n", c.text)),
            }
        }
    }
    if !dropped.is_empty() {
        p.push_str("\nRefuted and dropped (do not use):\n");
        for c in dropped {
            if let Verdict::Refuted(why) = verdict_of(c) {
                p.push_str(&format!("- {} — {why}\n", c.text));
            }
        }
    }
    p
}

/// Verdict lines `N. supported`, `N. refuted: why`, `N) contested — why`, `**N.** unverified`.
/// The first verdict for a number wins.
pub fn parse_verdicts(text: &str) -> HashMap<usize, Verdict> {
    let mut out = HashMap::new();
    for line in text.lines() {
        let t = line.trim().trim_start_matches(['-', '*', '#', ' ']);
        let digits = t.chars().take_while(char::is_ascii_digit).count();
        let Ok(n) = t[..digits].parse::<usize>() else { continue };
        let rest = t[digits..].trim_start_matches(['.', ')', ':', '*', ' ']);
        let lower = rest.to_ascii_lowercase();
        let reason = || {
            let after = rest
                .find([':', '—', '-', '('])
                .map(|i| &rest[i..])
                .unwrap_or("")
                .trim_start_matches([':', '—', '-', '(', ' ']);
            after.trim().trim_end_matches(')').trim_matches('*').trim().to_string()
        };
        let v = if lower.starts_with("supported") {
            Verdict::Supported
        } else if lower.starts_with("refuted") {
            Verdict::Refuted(reason())
        } else if lower.starts_with("contested") {
            Verdict::Contested(reason())
        } else if lower.starts_with("unverified") {
            Verdict::Unverified(reason())
        } else {
            continue;
        };
        out.entry(n).or_insert(v);
    }
    out
}

/// The first paragraph that is not a heading, cut at a word boundary to about `max` chars.
pub fn first_paragraph(text: &str, max: usize) -> String {
    let para = text
        .split("\n\n")
        .map(str::trim)
        .find(|p| !p.is_empty() && !p.starts_with('#') && !p.to_ascii_lowercase().starts_with("confidence:"))
        .unwrap_or("")
        .replace('\n', " ");
    if para.chars().count() <= max {
        return para;
    }
    let cut: String = para.chars().take(max).collect();
    let at_boundary = para.chars().nth(max).is_some_and(char::is_whitespace);
    let cut = if at_boundary { cut.as_str() } else { cut.rfind(' ').map(|i| &cut[..i]).unwrap_or(&cut) };
    format!("{}…", cut.trim_end())
}

fn rank(c: Confidence) -> u8 {
    match c {
        Confidence::Low => 0,
        Confidence::Medium => 1,
        Confidence::High => 2,
    }
}

/// The lower of two confidences.
pub fn min_confidence(a: Confidence, b: Confidence) -> Confidence {
    if rank(a) <= rank(b) {
        a
    } else {
        b
    }
}
