// Copyright 2025 OpenAI (codex), Apache-2.0. Derived from codex-rs/shell-command/src/
// command_safety/powershell_parser.rs and powershell_parser.ps1 (the script is included
// unchanged) at 6c0c6759d0617d7670826b61aca0a931bbb6db1f. Modified for Marlowe: the protocol is
// kept here and the long-lived child is started by `marlowe-agent` through `marlowe_os::child`
// (this crate starts no process); the result cross-checks Marlowe's own lexical parse.
// See http://www.apache.org/licenses/LICENSE-2.0 and THIRD_PARTY.md.

//! PowerShell's own parser as a second opinion. A long-lived `powershell -EncodedCommand
//! <script>` child reads one JSON request per line (`{id, payload}`: base64 UTF-16LE source) and
//! answers `{id, status, commands}`: `ok` with the argv of every command when the line lowers to
//! constant words, `unsupported` when it parses but holds dynamic constructs, `parse_errors` /
//! `parse_failed` when PowerShell itself cannot parse it.
//!
//! [`cross_check`] compares that with [`super::words::powershell_words`]: a line PowerShell
//! cannot parse, or one the two read differently, asks. It runs only where a PowerShell call
//! would otherwise run without asking.

use serde::{Deserialize, Serialize};

/// The parser script (Codex's, unchanged).
pub const PARSER_SCRIPT: &str = include_str!("fixtures/codex_powershell_parser.ps1");

/// What PowerShell's parser made of a line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AstOutcome {
    /// Constant command words, one argv per command.
    Commands(Vec<Vec<String>>),
    /// Parses, but holds constructs that do not lower to words (variables, blocks, ...).
    Unsupported,
    /// PowerShell reports parse errors.
    ParseErrors,
    /// The parser could not be asked (no PowerShell, the child died).
    Failed,
}

/// Something that can ask PowerShell's parser (the agent's long-lived child; a fake in tests).
pub trait PwshAst: Send + Sync {
    fn parse(&self, script: &str) -> AstOutcome;
}

/// Why a PowerShell line must ask after comparing PowerShell's reading with Marlowe's literal
/// lowering, if it must.
pub fn cross_check(ours: Option<&Vec<Vec<String>>>, ast: &AstOutcome) -> Option<&'static str> {
    match (ast, ours) {
        (AstOutcome::Failed, _) => Some("PowerShell's parser could not check this command"),
        (AstOutcome::ParseErrors, _) => Some("PowerShell cannot parse this command"),
        (AstOutcome::Commands(theirs), Some(ours)) if theirs != ours => Some("PowerShell reads this command differently"),
        (AstOutcome::Unsupported, Some(_)) => Some("PowerShell reads this command differently"),
        _ => None,
    }
}

/// One request line.
#[derive(Debug, Serialize)]
pub struct Request {
    pub id: u64,
    pub payload: String,
}

impl Request {
    pub fn new(id: u64, script: &str) -> Self {
        Request { id, payload: encode_utf16_base64(script) }
    }

    /// The JSON line, newline included.
    pub fn line(&self) -> String {
        let mut s = serde_json::to_string(self).unwrap_or_default();
        s.push('\n');
        s
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Response {
    id: Option<u64>,
    status: String,
    commands: Option<Vec<Vec<String>>>,
}

/// Reads one response line for request `id`; `Err` on a malformed line or an id mismatch (the
/// caller drops the child and retries once with a fresh one).
pub fn read_response(line: &str, id: u64) -> Result<AstOutcome, String> {
    let r: Response = serde_json::from_str(line.trim_start_matches('\u{feff}').trim()).map_err(|e| e.to_string())?;
    if r.id != Some(id) {
        return Err(format!("response id {:?} for request {id}", r.id));
    }
    Ok(match r.status.as_str() {
        "ok" => r
            .commands
            .filter(|c| !c.is_empty() && c.iter().all(|cmd| !cmd.is_empty() && cmd.iter().all(|w| !w.is_empty())))
            .map(AstOutcome::Commands)
            .unwrap_or(AstOutcome::Unsupported),
        "unsupported" => AstOutcome::Unsupported,
        "parse_errors" | "parse_failed" => AstOutcome::ParseErrors,
        _ => AstOutcome::Failed,
    })
}

/// The `-EncodedCommand` form of the parser script.
pub fn encoded_script() -> String {
    encode_utf16_base64(PARSER_SCRIPT)
}

/// Base64 of UTF-16LE text, as `-EncodedCommand` and the request payload take it.
pub fn encode_utf16_base64(s: &str) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes: Vec<u8> = s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for k in 0..4 {
            if k <= chunk.len() {
                out.push(A[(n >> (18 - 6 * k) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_and_responses_round_trip() {
        let r = Request::new(3, "ls");
        assert_eq!(r.payload, "bABzAA==");
        assert!(r.line().ends_with('\n'));
        assert_eq!(
            read_response(r#"{"id":3,"status":"ok","commands":[["Get-Content","foo bar"]]}"#, 3).unwrap(),
            AstOutcome::Commands(vec![vec!["Get-Content".into(), "foo bar".into()]])
        );
        assert_eq!(read_response(r#"{"id":4,"status":"unsupported"}"#, 4).unwrap(), AstOutcome::Unsupported);
        assert_eq!(read_response(r#"{"id":4,"status":"parse_errors"}"#, 4).unwrap(), AstOutcome::ParseErrors);
        assert!(read_response(r#"{"id":5,"status":"ok"}"#, 4).is_err(), "an id mismatch is a protocol desync");
        assert!(read_response("not json", 4).is_err());
        // An `ok` with an empty word is not trusted.
        assert_eq!(read_response(r#"{"id":1,"status":"ok","commands":[["x",""]]}"#, 1).unwrap(), AstOutcome::Unsupported);
    }

    #[test]
    fn cross_check_asks_on_disagreement_only() {
        let lit = vec![vec!["git".to_string(), "status".to_string()]];
        assert_eq!(cross_check(Some(&lit), &AstOutcome::Commands(lit.clone())), None);
        assert!(cross_check(Some(&lit), &AstOutcome::Commands(vec![vec!["git".into()]])).is_some());
        assert!(cross_check(Some(&lit), &AstOutcome::Unsupported).is_some());
        assert_eq!(cross_check(None, &AstOutcome::Unsupported), None, "both see something dynamic");
        assert_eq!(cross_check(None, &AstOutcome::Commands(lit.clone())), None, "Marlowe is the stricter reader");
        assert!(cross_check(None, &AstOutcome::ParseErrors).is_some());
        assert!(cross_check(Some(&lit), &AstOutcome::Failed).is_some());
    }
}
