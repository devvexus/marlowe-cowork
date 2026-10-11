//! The prompts behind the git slash commands. They live in `marlowe-agent` (the conductor's
//! command table expands the built-in names to them); re-exported here next to the tools they
//! drive.

pub use marlowe_agent::git_prompts::{commit, open_pr, pr_comments, review};
