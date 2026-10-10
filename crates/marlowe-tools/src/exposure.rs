//! CONTRACTS.md §7.2 — registration ≠ exposure.
//!
//! > **INVARIANT: returns <= 12. Enforced by the type's constructor, not by convention.**
//!
//! HP10 makes this one of four budget tests that fail the build. The mechanism is the
//! constructor: [`ExposedSet`] has a private field and no way in except [`ExposedSet::new`],
//! so "≤12" is a property of every value that exists rather than a property of the call sites
//! somebody remembered to check.
//!
//! Serde routes through the same constructor. A deserialization path that built the struct
//! field-wise would be a thirteenth tool arriving from a config file, and the assertion would
//! still be present and still pass.

use serde::{Deserialize, Serialize};

use crate::manifest::ToolId;

/// **2026-10: raised fourteen → sixty-four for the conductor, which exposes builtin + MCP + skill
/// tools to frontier models; the history below is the old small-model rationale.**
///
/// ARCHITECTURE §5, **amended 2026-08-27: twelve → thirteen (ADR-058), thirteen → fourteen
/// (ADR-059, `glob`)**.
///
/// # The number is a floor on what MCP gets, not a ceiling on what Marlowe has
///
/// Twelve was eleven builtins plus one spare. Splitting `write` out of `edit` — because `edit` was
/// two tools wearing one name and a model asked to write a file reached for the shell instead —
/// made the builtins eleven **exposed**, which silently took an MCP server from two tools to one.
///
/// That is the wrong thing to have paid with. A fix to Marlowe's own surface should not shrink
/// what a user's server may offer, and one tool is not a usable budget for a server: `mcp.json`
/// is not hypothetical. So the cap moved rather than the MCP allowance.
///
/// **The number is arithmetic, not a new judgement: the exposed builtins plus the two MCP slots
/// the budget has always meant.** It carries no spare, and that is deliberate — the next builtin
/// has to raise this again, in the open, with a reason. A cap that quietly absorbed each new tool
/// would be the permissive default this project keeps deleting.
pub const MAX_EXPOSED_TOOLS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExposureError {
    #[error(
        "a capability profile exposed {got} tools; the budget is {MAX_EXPOSED_TOOLS} \
         (ARCHITECTURE §5, HP10). Registration is unlimited — expose fewer and let the model \
         reach the rest through `use`"
    )]
    TooMany { got: usize },

    #[error(
        "tool `{tool}` is exposed twice. Two slots spent on one tool is a budget that counts \
         wrong, and the duplicate would be invisible in the model's tool list"
    )]
    Duplicate { tool: ToolId },
}

/// The model-visible tool set for one run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct ExposedSet {
    tools: Vec<ToolId>,
}

impl ExposedSet {
    /// The only constructor. Rejects an over-budget set and a set with duplicates.
    pub fn new(tools: Vec<ToolId>) -> Result<Self, ExposureError> {
        if tools.len() > MAX_EXPOSED_TOOLS {
            return Err(ExposureError::TooMany { got: tools.len() });
        }
        for (i, t) in tools.iter().enumerate() {
            if tools[..i].contains(t) {
                return Err(ExposureError::Duplicate { tool: t.clone() });
            }
        }
        Ok(Self { tools })
    }

    /// The quarantined reader's set (§8.2). Named rather than written as `new(vec![])` at each
    /// call site, because "empty" is the security property and it should be greppable.
    pub fn empty() -> Self {
        Self { tools: Vec::new() }
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    pub fn contains(&self, tool: &ToolId) -> bool {
        self.tools.contains(tool)
    }

    pub fn iter(&self) -> impl Iterator<Item = &ToolId> {
        self.tools.iter()
    }

    pub fn as_slice(&self) -> &[ToolId] {
        &self.tools
    }
}

impl<'de> Deserialize<'de> for ExposedSet {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let tools = Vec::<ToolId>::deserialize(d)?;
        Self::new(tools).map_err(serde::de::Error::custom)
    }
}

/// The unit of tool exposure. Every entry in `marlowe_exec`'s registration table names its group;
/// a session exposes whole groups (design 3: frontier models get every group, local models the
/// core groups plus the ones detected for the workspace, fixed for the session so the prompt
/// cache holds).
///
/// | Group | Tools | Owner |
/// |---|---|---|
/// | `files` | read, write, edit, multi_edit, edit_lines, notebook_edit | A |
/// | `search` | glob, grep | B |
/// | `shell` | bash, powershell, python, job_output, job_watch, job_kill | B |
/// | `web` | web_fetch, web_search, arxiv, paper | seam |
/// | `code` | symbols, definition, references, hover, diagnostics, rename_symbol | C |
/// | `verify` | project, service, http_check, browser | D |
/// | `agent` | todo, ask_user, skill, agent, send_message, exit_plan, enter/exit_worktree | F |
/// | `git` | git, gh, ci_status | J |
/// | `ml` | train_launch, experiments | J2 |
/// | `mcp` | mcp__server__tool, tool_search | K |
/// | `memory` | remember, recall, forget, correct (daemon) | - |
/// | `crew` | dispatch (daemon) | - |
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolGroup {
    Files,
    Search,
    Shell,
    Web,
    Code,
    Verify,
    Agent,
    Git,
    Ml,
    Mcp,
    Memory,
    Crew,
}

impl ToolGroup {
    pub const ALL: [ToolGroup; 12] = [
        ToolGroup::Files,
        ToolGroup::Search,
        ToolGroup::Shell,
        ToolGroup::Web,
        ToolGroup::Code,
        ToolGroup::Verify,
        ToolGroup::Agent,
        ToolGroup::Git,
        ToolGroup::Ml,
        ToolGroup::Mcp,
        ToolGroup::Memory,
        ToolGroup::Crew,
    ];

    /// Groups a local model always gets. The rest are exposed when detected for the workspace
    /// (a project profile, a git repo, an LSP, a training setup) or, for `mcp`, behind
    /// `tool_search` over its threshold.
    pub const CORE: [ToolGroup; 7] = [
        ToolGroup::Files,
        ToolGroup::Search,
        ToolGroup::Shell,
        ToolGroup::Web,
        ToolGroup::Agent,
        ToolGroup::Memory,
        ToolGroup::Crew,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ToolGroup::Files => "files",
            ToolGroup::Search => "search",
            ToolGroup::Shell => "shell",
            ToolGroup::Web => "web",
            ToolGroup::Code => "code",
            ToolGroup::Verify => "verify",
            ToolGroup::Agent => "agent",
            ToolGroup::Git => "git",
            ToolGroup::Ml => "ml",
            ToolGroup::Mcp => "mcp",
            ToolGroup::Memory => "memory",
            ToolGroup::Crew => "crew",
        }
    }
}

/// Which model classes a tool is offered to inside its group (e.g. `edit_lines` is a local-model
/// bench arm; a GPT-only `apply_patch` would be frontier-only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Profiles {
    pub frontier: bool,
    pub local: bool,
}

impl Profiles {
    pub const ALL: Profiles = Profiles { frontier: true, local: true };
    pub const FRONTIER: Profiles = Profiles { frontier: true, local: false };
    pub const LOCAL: Profiles = Profiles { frontier: false, local: true };

    pub fn admits(self, profile: ExposureProfile) -> bool {
        match profile {
            ExposureProfile::Frontier => self.frontier,
            ExposureProfile::Local => self.local,
        }
    }
}

impl Default for Profiles {
    fn default() -> Self {
        Profiles::ALL
    }
}

/// The model class a session's tool set is chosen for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExposureProfile {
    Frontier,
    Local,
}

/// The groups a session exposes: every group for a frontier model; the core groups plus
/// `detected` for a local one. Sorted, no duplicates. Chosen once per session (prompt cache).
pub fn select_groups(profile: ExposureProfile, detected: &[ToolGroup]) -> Vec<ToolGroup> {
    let mut groups: Vec<ToolGroup> = match profile {
        ExposureProfile::Frontier => ToolGroup::ALL.to_vec(),
        ExposureProfile::Local => ToolGroup::CORE.iter().chain(detected).copied().collect(),
    };
    groups.sort();
    groups.dedup();
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(n: usize) -> Vec<ToolId> {
        (0..n).map(|i| ToolId::new(format!("t{i}"))).collect()
    }

    /// **The name and every number are derived, and that is the point.**
    ///
    /// This was `twelve_is_allowed_and_thirteen_is_not`, with `got: 13` typed in. ADR-058 moved
    /// the cap to thirteen and the literal stayed — so the assertion started demanding that a set
    /// of FOURTEEN report `got: 13`, which is simply false, and the test name described a rule
    /// the code no longer had.
    ///
    /// A constant that appears as a word in a test name is a constant that will go stale in a
    /// place no compiler looks.
    #[test]
    fn the_budget_is_allowed_and_one_past_it_is_not() {
        assert!(ExposedSet::new(ids(MAX_EXPOSED_TOOLS)).is_ok(), "the budget itself must fit");
        assert_eq!(
            ExposedSet::new(ids(MAX_EXPOSED_TOOLS + 1)),
            Err(ExposureError::TooMany { got: MAX_EXPOSED_TOOLS + 1 }),
            "the refusal carries the count the user has to act on"
        );
    }

    #[test]
    fn frontier_gets_every_group_and_local_gets_core_plus_detected() {
        assert_eq!(select_groups(ExposureProfile::Frontier, &[]), ToolGroup::ALL.to_vec());
        let local = select_groups(ExposureProfile::Local, &[ToolGroup::Git, ToolGroup::Files]);
        assert!(local.contains(&ToolGroup::Git));
        assert!(!local.contains(&ToolGroup::Code));
        assert_eq!(local.len(), ToolGroup::CORE.len() + 1, "detected core groups are not doubled");
        assert!(Profiles::LOCAL.admits(ExposureProfile::Local) && !Profiles::LOCAL.admits(ExposureProfile::Frontier));
        for g in ToolGroup::ALL {
            assert_eq!(serde_json::to_string(&g).unwrap(), format!("\"{}\"", g.as_str()));
        }
    }

    #[test]
    fn duplicates_are_refused() {
        let dup = vec![ToolId::new("read"), ToolId::new("read")];
        assert_eq!(
            ExposedSet::new(dup),
            Err(ExposureError::Duplicate { tool: ToolId::new("read") })
        );
    }

    #[test]
    fn deserialization_runs_the_same_constructor() {
        // A tool past the budget arriving from a config file must be refused by the same code the
        // constructor uses. Without this the invariant would hold everywhere except the one path
        // that reads user input.
        //
        // Derived from `MAX_EXPOSED_TOOLS`, not typed: at the old cap of twelve these literals
        // meant "one past" and "exactly at". ADR-058 moved the cap and they silently became
        // "exactly at" and "one under" — the same bytes asserting a different property.
        let past = serde_json::to_string(&ids(MAX_EXPOSED_TOOLS + 1)).unwrap();
        assert!(serde_json::from_str::<ExposedSet>(&past).is_err());

        let at = serde_json::to_string(&ids(MAX_EXPOSED_TOOLS)).unwrap();
        assert_eq!(
            serde_json::from_str::<ExposedSet>(&at).unwrap().len(),
            MAX_EXPOSED_TOOLS
        );
    }
}
