//! The tool list a conversation sends, frozen.
//!
//! A local model keeps its prompt cache only while every request starts with the previous one,
//! and the tool list sits ahead of every message. So the list is read from the host once, when
//! the conversation begins (or is compacted), and every request after that sends it unchanged:
//! an MCP server that connects later, a `tool_search` that loads a schema, a group that turns on
//! all wait for the next compaction or a new session. The model is told about them on the newest
//! user message instead (a turn note, so the bytes before it never change) and can call them by
//! name right away: the host runs a call by name whether or not it is listed.

use std::collections::BTreeSet;

use marlowe_model::ToolSpec;

use crate::context::TurnNotes;
use crate::convo::Conversation;

/// The most characters of one new tool's schema the note carries.
const SCHEMA_CHARS: usize = 1_500;

/// The tool list the conversation sends, and what the model has been told is waiting.
#[derive(Debug, Clone, PartialEq)]
pub struct FrozenTools {
    pub specs: Vec<ToolSpec>,
    /// The compaction summary the list was frozen under: a new summary refreshes it.
    pub for_summary: Option<String>,
    /// Tools the model has been told about already.
    pub told: BTreeSet<String>,
}

impl FrozenTools {
    pub fn new(specs: Vec<ToolSpec>, for_summary: Option<String>) -> Self {
        Self { specs, for_summary, told: BTreeSet::new() }
    }
}

/// The list this turn sends, given what the host offers now. Freezes it on the first turn and
/// after a compaction. Otherwise returns the frozen list and, when the host offers tools the
/// list lacks, a note for the turn's user message at `user_index`.
pub fn for_turn(convo: &mut Conversation, live: Vec<ToolSpec>, user_index: usize) -> Vec<ToolSpec> {
    let refresh = match convo.ext.get::<FrozenTools>() {
        None => true,
        Some(f) => f.for_summary != convo.summary,
    };
    if refresh {
        convo.ext.insert(FrozenTools::new(live.clone(), convo.summary.clone()));
        return live;
    }
    let Some(frozen) = convo.ext.get_mut::<FrozenTools>() else { return live };
    let fresh: Vec<&ToolSpec> = live
        .iter()
        .filter(|s| !frozen.told.contains(&s.name) && !frozen.specs.iter().any(|f| f.name == s.name))
        .collect();
    if !fresh.is_empty() {
        let note = note_for(&fresh);
        frozen.told.extend(fresh.iter().map(|s| s.name.clone()));
        // One note per message: the store keeps one, and a reloaded session must send the same bytes.
        let notes = &mut convo.ext.get_or_default::<TurnNotes>().0;
        match notes.iter_mut().find(|(i, _)| *i == user_index) {
            Some((_, text)) => {
                text.push_str("\n\n");
                text.push_str(&note);
            }
            None => notes.push((user_index, note)),
        }
    }
    convo.ext.get::<FrozenTools>().map(|f| f.specs.clone()).unwrap_or(live)
}

fn note_for(fresh: &[&ToolSpec]) -> String {
    let mut out = String::from(
        "<system-reminder>\nThese tools became available after this conversation began. They are not in your tool list \
(it stays fixed until the conversation is compacted), but you can call them by name now with the arguments shown.\n",
    );
    for s in fresh {
        let line = s.description.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
        let mut schema = s.input_schema.to_string();
        if schema.chars().count() > SCHEMA_CHARS {
            schema = schema.chars().take(SCHEMA_CHARS).collect::<String>() + "…";
        }
        out.push_str(&format!("\n- {}: {line}\n  arguments (JSON schema): {schema}", s.name));
    }
    out.push_str("\n</system-reminder>");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use marlowe_proto::SessionId;
    use serde_json::json;

    fn spec(name: &str) -> ToolSpec {
        ToolSpec { name: name.into(), description: format!("{name} does a thing.\nMore."), input_schema: json!({"type": "object"}) }
    }

    fn convo() -> Conversation {
        Conversation::new(SessionId::from("s"), None)
    }

    #[test]
    fn the_list_is_frozen_and_new_tools_are_told_once_on_the_newest_message() {
        let mut c = convo();
        let first = for_turn(&mut c, vec![spec("read"), spec("write")], 0);
        assert_eq!(first.len(), 2);
        // A server connected, a tool was loaded, one group went away: the list is what it was.
        let live = vec![spec("write"), spec("mcp__x__ping"), spec("read")];
        let next = for_turn(&mut c, live.clone(), 4);
        assert_eq!(next, first, "same tools, same order, nothing added");
        let notes = &c.ext.get::<TurnNotes>().unwrap().0;
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].0, 4, "told on the newest user message");
        assert!(notes[0].1.contains("mcp__x__ping") && !notes[0].1.contains("- read"), "{}", notes[0].1);
        // Said once.
        assert_eq!(for_turn(&mut c, live, 6), first);
        assert_eq!(c.ext.get::<TurnNotes>().unwrap().0.len(), 1);
    }

    #[test]
    fn a_compaction_lets_the_new_tools_join() {
        let mut c = convo();
        for_turn(&mut c, vec![spec("read")], 0);
        c.summary = Some("earlier work".into());
        let after = for_turn(&mut c, vec![spec("read"), spec("mcp__x__ping")], 2);
        assert_eq!(after.len(), 2, "the list is read again after a compaction");
        assert!(c.ext.get::<TurnNotes>().is_none(), "nothing to announce: it is in the list now");
    }
}
