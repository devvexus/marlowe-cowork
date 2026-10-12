//! Review through lenses: one reviewer per lens (correctness, security, performance), each in a
//! clean context with the diff, each answering with structured findings. Used by build_v2's
//! stage 6 and on its own for a pending diff (`/security-review` is the security lens alone).

use super::is_stop;
use super::parse::parse_findings;
use super::types::{Finding, Lens, Severity};
use crate::playbook::{StepRunner, StepSpec};
use crate::CrewError;

/// Characters of diff a reviewer is shown inline; the rest it can read in the worktree.
pub const DIFF_CHARS: usize = 60_000;

/// What a reviewer may use: read the tree and run the tests, nothing else.
pub const REVIEW_TOOLS: [&str; 5] = ["read", "glob", "grep", "bash", "powershell"];

/// What each lens looks for.
pub fn checklist(lens: Lens) -> &'static str {
    match lens {
        Lens::Correctness => {
            "wrong results, unhandled inputs and errors, off-by-one, races, broken contracts with callers, behaviour the \
             goal did not ask for, tests that do not test what they claim"
        }
        // One list with the conductor's `/security-review`.
        Lens::Security => marlowe_agent::git_prompts::SECURITY_CHECKLIST,
        Lens::Performance => {
            "work that grows worse than the input needs (quadratic loops, repeated parsing, queries in loops), blocking \
             calls on hot paths, unbounded memory or output, needless copies of large data"
        }
    }
}

/// The reviewer's prompt for `lens` over `diff`.
pub fn prompt(lens: Lens, goal: &str, diff: &str) -> String {
    let cut: String = diff.chars().take(DIFF_CHARS).collect();
    let more = if cut.len() < diff.len() { "\n[… diff cut; read the rest in the worktree]" } else { "" };
    format!(
        "## Review lens: {name}\nGoal of the change: {goal}\n\nThe diff:\n```diff\n{cut}\n```{more}\n\n\
         Review this change through the {name} lens only: {list}. Report only what you can point at in the diff or \
         the code it touches, with the input or state that triggers it. Style is not a finding.\n\n\
         Reply with one ```json block holding an array of findings, each \
         {{\"severity\": \"blocker|major|minor|nit\", \"file\": \"path\", \"line\": 12, \"claim\": \"what is wrong\", \
         \"evidence\": \"why you are sure\", \"fix\": \"the smallest fix\"}}. Blocker: wrong, unsafe or broken as \
         shipped. Major: must be fixed before merging. Reply with `[]` when this lens finds nothing.",
        name = lens.name(),
        list = checklist(lens),
    )
}

/// The non-blocking note a lens leaves when its review cannot be used.
fn unreadable(lens: Lens, claim: String, reply: String) -> Finding {
    Finding {
        lens,
        severity: Severity::Minor,
        file: String::new(),
        line: 0,
        claim,
        evidence: reply.chars().take(400).collect(),
        fix: "review this lens by hand".into(),
    }
}

/// Reviews `diff` through each of `lenses` in parallel, step keys `<key>:<lens>`. A reply that
/// does not parse goes back to its reviewer once with the reason; if it still does not, the
/// result is one non-blocking note saying so (a review that cannot be read never blocks or
/// passes silently). A reviewer that fails outright leaves the same note; only a cancel or a
/// stop is an `Err`.
pub fn review_diff(
    runner: &dyn StepRunner,
    goal: &str,
    diff: &str,
    lenses: &[Lens],
    key: &str,
) -> Result<Vec<(Lens, Vec<Finding>)>, CrewError> {
    let spec = |lens: Lens, key: String, prompt: String| StepSpec {
        key,
        role: lens.role().into(),
        label: format!("review: {}", lens.name()),
        prompt,
        tools: Some(REVIEW_TOOLS.iter().map(|t| t.to_string()).collect()),
        effort: None,
        max_output: None,
        sandbox: None,
    };
    let specs: Vec<StepSpec> = lenses.iter().map(|l| spec(*l, format!("{key}:{}", l.name()), prompt(*l, goal, diff))).collect();
    let results = runner.parallel(specs);
    let mut out = Vec::new();
    for (lens, r) in lenses.iter().zip(results) {
        // A reviewer that ends without an answer (step limit, provider error) costs its lens, not
        // the build: it becomes a non-blocking note. Only a cancel or stop ends the run.
        let text = match r {
            Ok(r) => r.text,
            Err(e) if is_stop(&e) => return Err(e),
            Err(e) => {
                out.push((*lens, vec![unreadable(*lens, format!("the {} review did not complete ({e})", lens.name()), String::new())]));
                continue;
            }
        };
        let findings = match parse_findings(&text, *lens) {
            Ok(f) => f,
            Err(why) => {
                let again = runner.step(spec(
                    *lens,
                    format!("{key}:{}:repair", lens.name()),
                    format!(
                        "{}\n\nYour previous reply could not be read ({why}). It was:\n{}\n\nReply again with only the ```json \
                         array of findings.",
                        prompt(*lens, goal, diff),
                        text.chars().take(4_000).collect::<String>()
                    ),
                ));
                match again {
                    Err(e) if is_stop(&e) => return Err(e),
                    Err(e) => vec![unreadable(*lens, format!("the {} review could not be read ({why}; repair: {e})", lens.name()), text)],
                    Ok(again) => parse_findings(&again.text, *lens).unwrap_or_else(|why| {
                        vec![unreadable(*lens, format!("the {} review could not be read ({why})", lens.name()), again.text)]
                    }),
                }
            }
        };
        out.push((*lens, findings));
    }
    Ok(out)
}
