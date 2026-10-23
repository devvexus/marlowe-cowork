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
    format!(
        "<system-reminder>Teams is on: form a team task for every substantive request (several in sequence for \
         multi-phase work: understand, design, build, review, staying in the loop between them); solo only for \
         conversation and trivial edits. When the user names a saved team task, or one in the team tool's list \
         fits, run it by name with its args (team{{name, args}}) and write no script; write one only when none \
         fits. Keep a team under {size} specialists. Load the team-task-authoring skill before writing a script.</system-reminder>"
    )
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
