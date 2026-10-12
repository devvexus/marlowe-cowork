//! The prompts behind `/recap` and `/goal`, Claude Code 2.1.296 commands the 2.14 inventory found
//! after the design was written (integ). Both are plain prompt expansions over machinery that
//! already exists: `/recap` reads the session's own history, and `/goal` puts the condition on
//! the shared task list (T34), which keeps the conductor working until the items are done (B3).

/// The built-in session command `name`, if it is one: its expansion of the arguments.
pub fn builtin(name: &str) -> Option<fn(&str) -> String> {
    match name {
        "recap" => Some(recap),
        "goal" => Some(goal),
        _ => None,
    }
}

/// `/recap [focus]`: where this session stands, for someone coming back to it.
pub fn recap(arg: &str) -> String {
    let focus = match arg.trim() {
        "" => String::new(),
        f => format!(" Pay most attention to: {f}."),
    };
    format!(
        "Recap this session for me as if I were coming back to it after a break, from the conversation itself (do not run \
         tools unless a fact is missing).{focus}\n\
         Give, briefly: the goal; what was done (files changed, commands run, decisions and why); what was found or measured; \
         what is still open or failing; and the single next step you would take. Plain prose and a short list, no headings."
    )
}

/// `/goal <condition>`: keep working until the condition holds, tracked on the task list.
pub fn goal(arg: &str) -> String {
    match arg.trim() {
        "" => "There is no goal yet. Ask me what the goal of this session is, in one question.".to_string(),
        condition => format!(
            "Goal for this session: {condition}\n\
             Work toward it until it is met, without stopping to report progress. First break it into concrete items on the \
             task list with `todo` (each one checkable: a test that passes, a command that succeeds, a file that exists), \
             then work through them, marking each done with `todo` only when you have checked it. When every item is \
             done, verify the goal itself once more and finish with a line that starts `Goal met:` and says how you checked. \
             If it cannot be met, stop and say exactly what blocks it."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recap_and_goal_expand_and_other_names_do_not() {
        assert!(builtin("recap").unwrap()("the tests").contains("Pay most attention to: the tests."));
        let g = builtin("goal").unwrap()("cargo test passes");
        assert!(g.contains("Goal for this session: cargo test passes") && g.contains("`todo`") && g.contains("Goal met:"));
        assert!(builtin("goal").unwrap()("  ").contains("Ask me what the goal"));
        assert!(builtin("commit").is_none());
    }
}
