//! Teams mode: a per-session standing opt-in to team tasks (Claude Code's second state).
//!
//! The daemon keeps the flag; each user turn asks [`TeamsNote`] for its reminder. The text rides
//! on the turn's user message (the conversation tail, [`crate::context::TurnNotes`]), never in
//! the system prompt, so the prefix stays byte-identical whether Teams is on or off.

use std::sync::Arc;

/// The reminder the first turn after Teams is switched off carries.
pub const OFF_NOTICE: &str =
    "<system-reminder>Teams is off: the standard opt-in rule applies again.</system-reminder>";

/// The reminder every user turn carries while Teams is on. `size` is the team-size guideline.
pub fn on_reminder(size: u32) -> String {
    on_reminder_naming(size, None)
}

/// [`on_reminder`], plus (when the user's message names a saved team task) a line saying which one
/// and how to run it: the model that was told the list may still reach for `path` or `script`.
pub fn on_reminder_naming(size: u32, named: Option<&str>) -> String {
    let named = named.map(|n| format!(" The user named a saved team task: {n}. Run that one now: team{{name, args}}, taking the args from their words; write no script and pass no path.")).unwrap_or_default();
    format!(
        "<system-reminder>Teams is on: form a team task for every substantive request (several in sequence for \
         multi-phase work: understand, design, build, review, staying in the loop between them); solo only for \
         conversation and trivial edits. When the user names a saved team task, or one in the team tool's list \
         fits, run it by name with its args (team{{name, args}}) and write no script; write one only when none \
         fits. Keep a team under {size} specialists. Load the team-task-authoring skill before writing a script.{named}</system-reminder>"
    )
}

/// [`on_reminder_naming`] for a daemon that runs projects: a substantive request starts a project
/// (`manage_project`), not a team task. `named` is a saved team task the user's message names, and
/// `superseded` whether a project kind replaces it (then the note points at a project instead).
pub fn on_reminder_projects(size: u32, named: Option<&str>, superseded: bool) -> String {
    let named = match (named, superseded) {
        (Some(n), true) => format!(" The user named a saved team task: {n}. A project replaces it: start one with manage_project{{action:\"create\"}} and write no team script."),
        (Some(n), false) => format!(" The user named a saved team task: {n}. Run that one now: team{{name, args}}, taking the args from their words; write no script and pass no path."),
        (None, _) => String::new(),
    };
    format!(
        "<system-reminder>Teams is on: for every substantive request start a project with manage_project{{action:\"create\"}} without asking; \
         any request for research or deep research is one. Write the plan yourself (objective, questions, fields); the user approves the \
         plan card before anything runs. Stay solo for conversation and trivial edits. Use a team task (team) only when the user names a \
         saved one or asks for a script, and keep a team under {size} specialists.{named}</system-reminder>"
    )
}

/// Teams is off and the user asks for research: a research project is the answer in both states
/// (the plan card is the ask, and nothing is read before the user approves it).
pub const RESEARCH_REMINDER: &str = "<system-reminder>The user asks for research. Start a research project with manage_project{action:\"create\"} (objective, questions, fields): its plan card is the question to the user, and nothing is read until they approve it, so do not ask first.</system-reminder>";

/// Teams is off and the request looks substantial: offer a project, do not start one.
pub const SUGGEST_REMINDER: &str = "<system-reminder>If this request is substantial (several steps, a lot to read or build), say in one sentence that a project could do it and offer. Call manage_project only after the user agrees.</system-reminder>";

/// Whether the message asks for research or deep research. Whole words only: "research",
/// "researching", "literature review", "state of the art"; not "researcher" or "re-search".
/// Also a written answer backed by sources: a summary, report or overview, or to look into or
/// find out about something, "with sources" or citations. Found live: "look into what is known
/// about intermittent fasting and write me a summary with sources" went to the web by hand.
pub fn is_research_request(text: &str) -> bool {
    let lower = text.to_lowercase();
    let words: Vec<&str> = lower.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect();
    let has = |set: &[&str]| words.iter().any(|w| set.contains(w));
    let named = has(&["research", "researching", "researched"])
        || lower.contains("literature review")
        || lower.contains("state of the art")
        || lower.contains("state-of-the-art");
    let sourced = has(&["sources", "citations", "cited"]) || lower.contains("with references");
    let written = has(&["summary", "summarize", "summarise", "report", "overview", "investigate", "compile"])
        || ["look into", "dig into", "find out", "what is known", "what's known", "write up", "write-up"].iter().any(|p| lower.contains(p));
    named || (sourced && written)
}

/// Whether a message looks like work too big for one reply: a long message, or a shorter one
/// laid out as a list. Only used to offer a project when Teams is off.
pub fn looks_substantial(text: &str) -> bool {
    let words = text.split_whitespace().count();
    let lines = text.lines().filter(|l| !l.trim().is_empty()).count();
    words >= 40 || (words >= 20 && lines >= 4)
}

/// A service: the reminder for the turn now starting (given the user's text), if any. Asked once
/// per turn (the off notice is handed out once), so it may change what it remembers.
#[derive(Clone)]
pub struct TeamsNote(pub Arc<dyn Fn(&str) -> Option<String> + Send + Sync>);

impl TeamsNote {
    pub fn note(&self, user_text: &str) -> Option<String> {
        (self.0)(user_text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn research_is_a_whole_word_and_a_few_phrases() {
        for yes in [
            "Do a deep research on the newest open models",
            "can you research kimi k3?",
            "I want a literature review of pruning",
            "what is the state of the art in speculative decoding",
            "Researching this by hand is slow",
            "Can you look into what is actually known about the health effects of intermittent fasting and write me a summary with sources?",
            "What are the current best practices for storing lithium-ion batteries long term? Put together a short report with sources.",
            "give me an overview of RISC-V vector extensions with citations",
        ] {
            assert!(is_research_request(yes), "{yes}");
        }
        for no in [
            "fix the failing test",
            "the researcher notebook",
            "a re-search of the file tree",
            "search the web for kimi",
            "summarize this file",
            "look into why the build fails",
            "list the sources of this crate",
        ] {
            assert!(!is_research_request(no), "{no}");
        }
    }

    #[test]
    fn substantial_is_length_or_a_list() {
        assert!(!looks_substantial("fix the typo in the readme"));
        assert!(looks_substantial(&"word ".repeat(45)));
        assert!(looks_substantial("Please do these things for me:\n- one two three four\n- five six seven eight\n- nine ten eleven twelve\n- and thirteen fourteen fifteen"));
    }

    #[test]
    fn the_project_reminder_points_at_projects_and_keeps_the_team_for_named_tasks() {
        let r = on_reminder_projects(5, None, false);
        assert!(r.starts_with("<system-reminder>Teams is on: for every substantive request start a project"), "{r}");
        assert!(r.contains("manage_project") && r.contains("under 5 specialists") && r.ends_with("</system-reminder>"));
        assert!(on_reminder_projects(5, Some("code-review (args: path)"), false).contains("Run that one now: team{name, args}"));
        let sup = on_reminder_projects(5, Some("old-lookup"), true);
        assert!(sup.contains("A project replaces it") && !sup.contains("Run that one now"), "{sup}");
    }
}
