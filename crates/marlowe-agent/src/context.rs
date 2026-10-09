//! Context assembly: tiered system prompt, tool-result masking, compaction.
//!
//! Tiers: stable (persona core + blocks + user, cached) -> context (compaction summary, memory
//! blocks) -> volatile (environment, history). Only the last [`KEEP_TOOL_RESULTS`] tool
//! results keep their bodies; older ones are masked. At [`COMPACTION_TRIGGER`] of the window a
//! summariser call condenses older history.

use marlowe_model::{
    CancelToken, ChatRequest, Effort, Message, ModelInfo, Part, Provider, ProviderError, Role, SystemBlock, ToolSpec,
};

use crate::convo::Conversation;
use crate::system::SystemPrompt;

/// Fraction of the effective window at which history is compacted.
pub const COMPACTION_TRIGGER: f32 = 0.70;

/// Tool results that keep their full body; older ones are masked.
pub const KEEP_TOOL_RESULTS: usize = 8;

/// Window assumed when the model does not report one.
pub const DEFAULT_WINDOW: u32 = 32_768;

/// Replaces the body of a masked tool result.
pub const MASKED_RESULT: &str = "[older tool result elided by the harness; call the tool again if needed]";

/// Preface of the harness-written summary that replaces compacted history.
pub const SUMMARY_PREFACE: &str = "[The conversation so far, condensed by the harness]";

/// Instruction given to the summariser call.
pub const SUMMARY_PROMPT: &str = "Summarise the conversation so far for your own later use: the \
user's goals, decisions made and why, facts established, files touched, and open work. Be \
complete and terse. Do not address the user.";

/// Fixed per-message overhead (role, separators) in the estimate.
const MESSAGE_OVERHEAD: u32 = 4;

/// Longest tool result body shown to the summariser, in characters.
const SUMMARY_RESULT_CHARS: usize = 1_500;

/// Context sizing for one model.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ContextConfig {
    pub window_tokens: u32,
    /// Tokens reserved for the reply.
    pub reserve_output: u32,
    pub keep_tool_results: usize,
    pub compaction_trigger: f32,
}

impl ContextConfig {
    /// Sizing from catalog info, with defaults for anything unknown.
    pub fn for_model(info: &ModelInfo) -> Self {
        let window = info.context_window.unwrap_or(DEFAULT_WINDOW);
        let reserve = info.max_output.unwrap_or(4_096).min(window / 4);
        Self {
            window_tokens: window,
            reserve_output: reserve,
            keep_tool_results: KEEP_TOOL_RESULTS,
            compaction_trigger: COMPACTION_TRIGGER,
        }
    }

    /// Window minus the output reserve.
    pub fn effective_window(&self) -> u32 {
        self.window_tokens.saturating_sub(self.reserve_output)
    }
}

/// The request-ready context for one model call.
#[derive(Debug, Clone, PartialEq)]
pub struct Assembled {
    pub system: Vec<SystemBlock>,
    pub messages: Vec<Message>,
    /// Estimated tokens of `system` + `messages`.
    pub tokens: u32,
}

/// Rough token estimate (about three characters per token).
pub fn estimate_tokens(text: &str) -> u32 {
    (text.chars().count() as u32).div_ceil(3)
}

/// Estimated tokens of one message, all parts included.
pub fn estimate_message_tokens(m: &Message) -> u32 {
    let parts: u32 = m
        .content
        .iter()
        .map(|p| match p {
            Part::Text { text } => estimate_tokens(text),
            Part::Thinking(t) => estimate_tokens(&t.text),
            Part::ToolCall(c) => estimate_tokens(&c.name) + estimate_tokens(&c.args.to_string()) + 2,
            Part::ToolResult(r) => estimate_tokens(&r.content) + 2,
        })
        .sum();
    parts + MESSAGE_OVERHEAD
}

/// Estimated tokens of a whole request.
pub fn estimate_request_tokens(req: &ChatRequest) -> u32 {
    system_tokens(&req.system)
        + req.messages.iter().map(estimate_message_tokens).sum::<u32>()
        + tool_spec_tokens(&req.tools)
}

/// Estimated tokens of the tool specs sent with every request.
pub fn tool_spec_tokens(tools: &[ToolSpec]) -> u32 {
    tools
        .iter()
        .map(|t| {
            estimate_tokens(&t.name) + estimate_tokens(&t.description) + estimate_tokens(&t.input_schema.to_string())
        })
        .sum()
}

fn system_tokens(blocks: &[SystemBlock]) -> u32 {
    blocks.iter().map(|b| estimate_tokens(&b.text)).sum()
}

/// The system block carrying a compaction summary. Cached: it changes only on compaction.
pub fn summary_block(summary: &str) -> SystemBlock {
    SystemBlock { text: format!("{SUMMARY_PREFACE}\n\n{}", summary.trim()), cache: true }
}

/// Builds the system blocks and message list for the next model call.
///
/// System: stable tier (core, blocks, user), the compaction summary, memory blocks, then the
/// environment. Messages: the history with thinking dropped from assistant messages before the
/// last user message, and all but the last `keep_tool_results` tool results masked.
pub fn assemble(system: &SystemPrompt, memory: &[String], convo: &Conversation, cfg: &ContextConfig) -> Assembled {
    let mut blocks = system.render_stable();
    if let Some(s) = convo.summary.as_deref().filter(|s| !s.trim().is_empty()) {
        blocks.push(summary_block(s));
    }
    blocks.extend(
        memory
            .iter()
            .filter(|m| !m.trim().is_empty())
            .map(|m| SystemBlock { text: m.trim().to_string(), cache: false }),
    );
    blocks.extend(system.render_environment());

    let mut messages = convo.messages.clone();
    if let Some(last_user) = messages.iter().rposition(|m| m.role == Role::User) {
        for m in &mut messages[..last_user] {
            if m.role == Role::Assistant {
                m.content.retain(|p| !matches!(p, Part::Thinking(_)));
            }
        }
        // An earlier assistant message that was only thinking is now empty; drop it.
        let mut i = 0;
        messages.retain(|m| {
            let keep = i >= last_user || !m.content.is_empty();
            i += 1;
            keep
        });
    }
    mask_tool_results(&mut messages, cfg.keep_tool_results);

    let tokens = system_tokens(&blocks) + messages.iter().map(estimate_message_tokens).sum::<u32>();
    Assembled { system: blocks, messages, tokens }
}

/// Masks the bodies of all but the last `keep` tool results; returns how many were masked.
/// Results already masked are not counted again.
pub fn mask_tool_results(messages: &mut [Message], keep: usize) -> usize {
    let mut seen = 0usize;
    let mut masked = 0usize;
    for m in messages.iter_mut().rev() {
        for p in m.content.iter_mut().rev() {
            if let Part::ToolResult(r) = p {
                seen += 1;
                if seen > keep && r.content != MASKED_RESULT {
                    r.content = MASKED_RESULT.to_string();
                    masked += 1;
                }
            }
        }
    }
    masked
}

/// True when `tokens` crosses the compaction trigger.
pub fn needs_compaction(tokens: u32, cfg: &ContextConfig) -> bool {
    tokens as f32 >= cfg.effective_window() as f32 * cfg.compaction_trigger
}

/// Estimated tokens of what compaction acts on: the summary plus the messages.
fn history_tokens(convo: &Conversation) -> u32 {
    convo.summary.as_deref().map_or(0, |s| estimate_tokens(&summary_block(s).text))
        + convo.messages.iter().map(estimate_message_tokens).sum::<u32>()
}

/// Which messages compaction keeps: the last user message, and the last assistant message
/// onward when it comes after that (so a tool call stays with its results).
fn kept_indices(messages: &[Message]) -> Vec<usize> {
    let live = messages.iter().rposition(|m| m.role == Role::User);
    let tail = messages
        .iter()
        .rposition(|m| m.role == Role::Assistant)
        .filter(|&a| live.is_none_or(|l| a > l));
    let mut keep: Vec<usize> = live.into_iter().collect();
    if let Some(t) = tail {
        keep.extend(t..messages.len());
    }
    keep
}

/// Condenses older history with a summariser call; returns (before, after) token estimates
/// of the summary plus messages. Everything except the live user message and the last tool
/// step is folded, together with any earlier summary, into `convo.summary`. When nothing can be
/// condensed it makes no call and returns equal numbers. On error the conversation is unchanged.
pub fn compact(
    convo: &mut Conversation,
    provider: &dyn Provider,
    model: &str,
    cfg: &ContextConfig,
    cancel: &CancelToken,
) -> Result<(u32, u32), ProviderError> {
    let before = history_tokens(convo);
    let keep = kept_indices(&convo.messages);
    if keep.is_empty() || keep.len() == convo.messages.len() {
        return Ok((before, before));
    }
    let dropped: Vec<&Message> =
        convo.messages.iter().enumerate().filter(|(i, _)| !keep.contains(i)).map(|(_, m)| m).collect();

    // Leave room for the instructions and the reply; about three characters per token.
    let budget_chars = (cfg.effective_window() as usize * 3 * 6 / 10).max(4_000);
    let mut prompt = String::new();
    if let Some(s) = convo.summary.as_deref() {
        prompt.push_str("<earlier_summary>\n");
        prompt.push_str(s.trim());
        prompt.push_str("\n</earlier_summary>\n\n");
    }
    prompt.push_str("<transcript>\n");
    prompt.push_str(&transcript(&dropped, budget_chars));
    prompt.push_str("\n</transcript>\n\n");
    prompt.push_str(SUMMARY_PROMPT);

    let mut req = ChatRequest::new(model, vec![Message::user(prompt)]);
    req.effort = Effort::Off;
    req.max_output_tokens = Some(cfg.reserve_output.clamp(256, 4_096));
    req.system =
        vec![SystemBlock { text: "You condense conversations into faithful working notes.".to_string(), cache: false }];
    let completion = provider.stream(&req, cancel, &mut |_| {})?;
    if cancel.is_cancelled() {
        return Err(ProviderError::cancelled());
    }
    let summary = completion.text.trim();
    if summary.is_empty() {
        return Err(ProviderError::server("the summariser returned nothing"));
    }

    convo.messages = keep.iter().map(|&i| convo.messages[i].clone()).collect();
    convo.summary = Some(summary.to_string());
    Ok((before, history_tokens(convo)))
}

/// Renders messages as a plain transcript, keeping the most recent `max_chars`.
fn transcript(messages: &[&Message], max_chars: usize) -> String {
    let mut lines: Vec<String> = Vec::new();
    for m in messages {
        let who = match m.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        };
        for p in &m.content {
            match p {
                Part::Text { text } if !text.trim().is_empty() => lines.push(format!("[{who}] {}", text.trim())),
                Part::ToolCall(c) => lines.push(format!("[assistant called {}] {}", c.name, c.args)),
                Part::ToolResult(r) => {
                    let body: String = r.content.chars().take(SUMMARY_RESULT_CHARS).collect();
                    let cut = if body.len() < r.content.len() { " [...]" } else { "" };
                    let tag = if r.is_error { "tool error" } else { "tool result" };
                    lines.push(format!("[{tag}] {body}{cut}"));
                }
                _ => {}
            }
        }
    }
    let mut used = 0usize;
    let mut kept: Vec<&str> = Vec::new();
    for line in lines.iter().rev() {
        let n = line.chars().count() + 1;
        if used + n > max_chars && !kept.is_empty() {
            break;
        }
        used += n;
        kept.push(line);
    }
    let mut out = String::new();
    if kept.len() < lines.len() {
        out.push_str("[earlier lines omitted]\n");
    }
    for line in kept.iter().rev() {
        out.push_str(line);
        out.push('\n');
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use marlowe_model::{ProviderKind, Thinking, ToolCall, ToolResult};
    use marlowe_proto::SessionId;
    use serde_json::json;

    fn call(id: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![Part::ToolCall(ToolCall { id: id.into(), name: "read".into(), args: json!({"path": id}) })],
        }
    }

    fn result(id: &str) -> Message {
        Message::tool_results([ToolResult { call_id: id.into(), content: format!("body {id}"), is_error: false }])
    }

    #[test]
    fn masking_keeps_the_last_n_and_is_idempotent() {
        let mut msgs = Vec::new();
        for i in 0..5 {
            msgs.push(call(&i.to_string()));
            msgs.push(result(&i.to_string()));
        }
        assert_eq!(mask_tool_results(&mut msgs, 2), 3);
        assert_eq!(mask_tool_results(&mut msgs, 2), 0, "already masked results are not recounted");
        let bodies: Vec<String> =
            msgs.iter().flat_map(|m| m.tool_results_iter().map(|r| r.content.clone())).collect();
        assert_eq!(bodies[..3], [MASKED_RESULT, MASKED_RESULT, MASKED_RESULT]);
        assert_eq!(bodies[3..], ["body 3", "body 4"]);
    }

    #[test]
    fn assembly_orders_tiers_and_drops_old_thinking() {
        let sys = SystemPrompt::with_core("core").environment(crate::Environment {
            now: "now".into(),
            os: "os".into(),
            workspace: None,
            model: "m".into(),
        });
        let mut convo = Conversation::new(SessionId::from("s"), None);
        convo.summary = Some("earlier stuff".into());
        convo.push(Message::user("first"));
        convo.push(Message {
            role: Role::Assistant,
            content: vec![
                Part::Thinking(Thinking { text: "hmm".into(), ..Default::default() }),
                Part::Text { text: "answer".into() },
            ],
        });
        convo.push(Message::user("second"));
        let cfg = ContextConfig::for_model(&ModelInfo::bare(ProviderKind::Ollama, "m"));
        let a = assemble(&sys, &["<memory>x</memory>".into()], &convo, &cfg);
        let texts: Vec<&str> = a.system.iter().map(|b| b.text.as_str()).collect();
        assert_eq!(texts[0], "core");
        assert!(texts[1].starts_with(SUMMARY_PREFACE) && texts[1].contains("earlier stuff"));
        assert_eq!(texts[2], "<memory>x</memory>");
        assert!(texts[3].starts_with("<environment>"));
        assert!(!a.system[3].cache);
        assert!(a.messages[1].thinking().is_none(), "thinking from earlier turns is not resent");
        assert_eq!(a.messages[1].joined_text(), "answer");
        assert!(convo.messages[1].thinking().is_some(), "the stored history is untouched");
        assert!(a.tokens > 0);
    }

    #[test]
    fn compaction_triggers_at_seventy_percent_not_at_exhaustion() {
        let cfg =
            ContextConfig { window_tokens: 1_100, reserve_output: 100, keep_tool_results: 8, compaction_trigger: 0.7 };
        assert!(!needs_compaction(699, &cfg));
        assert!(needs_compaction(700, &cfg));
    }

    #[test]
    fn kept_messages_are_the_live_turn_and_the_last_tool_step() {
        let msgs = vec![
            Message::user("old"),
            Message::assistant("old answer"),
            Message::user("live"),
            call("a"),
            result("a"),
            call("b"),
            result("b"),
        ];
        assert_eq!(kept_indices(&msgs), vec![2, 5, 6]);
        let msgs = vec![Message::user("old"), Message::assistant("old answer"), Message::user("live")];
        assert_eq!(kept_indices(&msgs), vec![2]);
    }

    #[test]
    fn the_transcript_keeps_the_most_recent_lines() {
        let a = Message::user("a".repeat(100));
        let b = Message::user("b".repeat(100));
        let t = transcript(&[&a, &b], 150);
        assert!(t.starts_with("[earlier lines omitted]"));
        assert!(t.contains("bbb") && !t.contains("aaa"));
    }

    #[test]
    fn request_estimate_counts_tools() {
        let mut req = ChatRequest::new("m", vec![Message::user("hello there")]);
        let base = estimate_request_tokens(&req);
        req.tools.push(ToolSpec { name: "read".into(), description: "Read a file".into(), input_schema: json!({}) });
        assert!(estimate_request_tokens(&req) > base);
    }
}
