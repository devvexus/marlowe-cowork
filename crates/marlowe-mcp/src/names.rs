//! Tool names the model sees: `mcp__<server>__<tool>`, at most 64 characters (the Anthropic
//! limit, and the strictest of the providers), `[A-Za-z0-9_-]` only.

use crate::util::sha256_hex;

pub const PREFIX: &str = "mcp__";
pub const MAX_NAME: usize = 64;

/// Tools the MCP host serves besides `mcp__<server>__<tool>`. The first three are model-visible;
/// the prompt tools are for the conductor's own commands and are callable but not listed.
pub const TOOL_SEARCH: &str = "tool_search";
pub const LIST_RESOURCES: &str = "list_mcp_resources";
pub const READ_RESOURCE: &str = "read_mcp_resource";
pub const LIST_PROMPTS: &str = "mcp_list_prompts";
pub const GET_PROMPT: &str = "mcp_get_prompt";

pub fn is_mcp(name: &str) -> bool {
    name.starts_with(PREFIX)
}

/// Replaces anything outside `[A-Za-z0-9_-]` with `_`.
pub fn sanitize(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect()
}

/// The model-visible name for `tool` of `server`. A name over the limit is cut and given a short
/// hash of the full name so two long names stay distinct.
pub fn exposed(server: &str, tool: &str) -> String {
    let full = format!("{PREFIX}{}__{}", sanitize(server), sanitize(tool));
    if full.len() <= MAX_NAME {
        return full;
    }
    let hash = &sha256_hex(full.as_bytes())[..6];
    format!("{}_{hash}", &full[..MAX_NAME - 7])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_namespaced_sanitised_and_bounded() {
        assert_eq!(exposed("github", "create_issue"), "mcp__github__create_issue");
        assert_eq!(exposed("my server", "do.it"), "mcp__my_server__do_it");
        let long = exposed("srv", &"t".repeat(100));
        assert_eq!(long.len(), MAX_NAME);
        assert_ne!(long, exposed("srv", &format!("{}u", "t".repeat(99))), "the hash keeps long names apart");
        assert!(is_mcp(&long) && !is_mcp("read"));
    }
}
