//! `journal.jsonl` (WF5, BG3): one line per finished agent, in completion order, then one line
//! for how the run ended. Resume reads it back: a call whose id and key match a recorded `done`
//! call is served from it instead of running.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::host::{AgentResult, AgentStatus};

/// One finished agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentRecord {
    /// Issue sequence number in the run that recorded it.
    pub seq: u32,
    /// Position in the script (the resume identity, [`crate::AgentCall::id`]).
    pub id: String,
    pub key: String,
    /// Completion order in that run.
    pub completion: u32,
    #[serde(default)]
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    #[serde(default)]
    pub cached: bool,
    pub result: AgentResult,
}

/// A journal line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Line {
    Agent(AgentRecord),
    /// The script returned.
    Result { value: Value },
    /// The script failed or was stopped.
    Ended { message: String },
}

/// Appends one line (creating the file and its directory).
pub fn append(path: &Path, line: &Line) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    let mut text = serde_json::to_string(line).map_err(std::io::Error::other)?;
    text.push('\n');
    f.write_all(text.as_bytes())
}

/// Every readable line; a torn last line (a crash mid-write) is skipped.
pub fn load(path: &Path) -> std::io::Result<Vec<Line>> {
    let f = std::fs::File::open(path)?;
    Ok(std::io::BufReader::new(f).lines().map_while(Result::ok).filter_map(|l| serde_json::from_str(&l).ok()).collect())
}

/// The finished agents of a journal.
pub fn agents(lines: &[Line]) -> Vec<AgentRecord> {
    lines
        .iter()
        .filter_map(|l| match l {
            Line::Agent(a) => Some(a.clone()),
            _ => None,
        })
        .collect()
}

/// How a run seeds itself from an earlier journal.
#[derive(Debug, Clone, Default)]
pub struct Resume {
    /// Usable results by call id (only calls that finished `done`).
    pub entries: HashMap<String, (String, AgentResult)>,
    /// `resume_from` (another run, possibly an edited script): the first call that differs, and
    /// every call after it in script order, runs live. `false` = the same run resumed in place
    /// after a restart: every matching call is served.
    pub prefix_rule: bool,
}

impl Resume {
    pub fn from_records(records: &[AgentRecord], prefix_rule: bool) -> Resume {
        let entries = records
            .iter()
            .filter(|r| r.result.status == AgentStatus::Done)
            .map(|r| (r.id.clone(), (r.key.clone(), r.result.clone())))
            .collect();
        Resume { entries, prefix_rule }
    }

    pub fn from_journal(path: &Path, prefix_rule: bool) -> std::io::Result<Resume> {
        Ok(Self::from_records(&agents(&load(path)?), prefix_rule))
    }
}
