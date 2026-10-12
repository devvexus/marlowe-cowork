//! What the user is looking at in their editor, put in front of the model at the start of a turn
//! (U14). The note comes from the connected editor's `get_context` ([`marlowe_mcp::ide`]); with
//! no editor, or one that does not answer within a second and a half, the turn starts without it.
//!
//! The note joins the turn as pending context, the way a hook's added context does, so memory
//! still sees only what the user typed. It is sent when the newest note already in the
//! conversation differs from the current one. Whether it was "already sent" is read from the
//! conversation's own messages rather than remembered here, so a turn that never happened (an
//! earlier middleware blocked it), a new or forked session, `/clear`, a rewind and a compaction
//! that dropped the old note all send it again.

use std::sync::Arc;

use marlowe_agent::{Conversation, ToolMiddleware, TurnFlow};
use marlowe_mcp::hub::Hub;
use marlowe_mcp::ide;
use marlowe_model::{Part, Role};

pub(super) struct IdeContext {
    hub: Arc<Hub>,
}

impl IdeContext {
    pub(super) fn new(hub: Arc<Hub>) -> Self {
        Self { hub }
    }
}

/// The newest editor note in the history, if any.
fn last_note(convo: &Conversation) -> Option<&str> {
    convo.messages.iter().rev().filter(|m| m.role == Role::User).find_map(|m| {
        m.content.iter().rev().find_map(|p| match p {
            Part::Text { text } if text.starts_with(ide::NOTE_PREFIX) => Some(text.as_str()),
            _ => None,
        })
    })
}

impl ToolMiddleware for IdeContext {
    fn name(&self) -> &str {
        "ide_context"
    }

    fn before_turn(&self, convo: &mut Conversation, _text: &mut String) -> TurnFlow {
        let Some(note) = ide::context_note(&self.hub, ide::CONTEXT_WAIT) else { return TurnFlow::Continue };
        if last_note(convo) != Some(note.as_str()) && !convo.pending_context.contains(&note) {
            convo.pending_context.push(note);
        }
        TurnFlow::Continue
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use marlowe_agent::{Conversation, ToolMiddleware};
    use marlowe_mcp::client::ConnectOpts;
    use marlowe_mcp::config::{self, Paths};
    use marlowe_mcp::fake::{FakeHttp, FakeOpts, HttpOpts};
    use marlowe_mcp::hub::{Hub, HubOpts};
    use marlowe_mcp::oauth::MemoryTokens;
    use marlowe_model::{Message, Part};
    use marlowe_proto::SessionId;
    use serde_json::json;

    use super::IdeContext;

    fn answer(path: &str, line: u32) -> String {
        json!({"selection": {"path": path, "language": "python", "empty": false, "text": "x = 1", "startLine": line, "endLine": line},
               "openEditors": [{"path": path, "language": "python", "dirty": false, "active": true}]})
        .to_string()
    }

    /// A hub whose `ide` server is the fake, answering `get_context` with the cell's text.
    fn editor(cell: &Arc<Mutex<String>>) -> (Arc<Hub>, FakeHttp) {
        let server = FakeHttp::start(HttpOpts { mcp: FakeOpts { context: Some(cell.clone()), ..Default::default() }, ..Default::default() });
        let home = std::env::temp_dir().join(format!("marlowe-ide-ctx-{}-{}", std::process::id(), marlowe_mcp::util::random_token(5)));
        std::fs::create_dir_all(&home).unwrap();
        let mut opts = HubOpts::new(Paths::new(&home, None), ConnectOpts::new(Arc::new(MemoryTokens::default())));
        opts.session = config::parse_session(&[json!({"mcpServers": {"ide": {"type": "http", "url": server.url(), "consequence": "inert"}}}).to_string()]).unwrap();
        let hub = Hub::start(opts);
        assert!(hub.wait_settled(Duration::from_secs(20)));
        (hub, server)
    }

    fn convo() -> Conversation {
        Conversation::new(SessionId("s".into()), None)
    }

    /// One turn as the loop runs it: the middleware, then the user's message is appended.
    fn turn(mw: &IdeContext, c: &mut Conversation, said: &str) -> Vec<String> {
        let mut text = said.to_string();
        mw.before_turn(c, &mut text);
        let pending = c.pending_context.clone();
        c.push(Message::user(text));
        pending
    }

    fn cell(v: String) -> Arc<Mutex<String>> {
        Arc::new(Mutex::new(v))
    }

    #[test]
    fn parity_u14_the_note_comes_on_turn_one_not_again_while_unchanged_and_again_when_it_changes() {
        let c = cell(answer("C:\\w\\a.py", 5));
        let (hub, _server) = editor(&c);
        let mw = IdeContext::new(hub);
        let mut convo = convo();
        let first = turn(&mw, &mut convo, "fix this");
        assert_eq!(first.len(), 1);
        assert!(first[0].contains("a.py") && first[0].contains("x = 1") && first[0].starts_with(marlowe_mcp::ide::NOTE_PREFIX), "{first:?}");
        assert!(convo.messages[0].content.iter().any(|p| matches!(p, Part::Text { text } if text == &first[0])), "the note is in the message, after what they typed");
        assert!(turn(&mw, &mut convo, "and again").is_empty(), "same selection, nothing new to say");
        *c.lock().unwrap() = answer("C:\\w\\a.py", 9);
        let changed = turn(&mw, &mut convo, "now this");
        assert_eq!(changed.len(), 1);
        assert!(changed[0].contains("line 9"), "{changed:?}");
        assert!(turn(&mw, &mut convo, "same").is_empty());
    }

    #[test]
    fn a_turn_that_never_ran_does_not_use_up_the_note() {
        let c = cell(answer("C:\\w\\a.py", 5));
        let (hub, _server) = editor(&c);
        let mw = IdeContext::new(hub);
        let mut blocked = convo();
        // An earlier middleware blocked the turn: before_turn ran, the message was never appended.
        let mut text = "blocked".to_string();
        mw.before_turn(&mut blocked, &mut text);
        blocked.pending_context.clear();
        let next = turn(&mw, &mut blocked, "retry");
        assert_eq!(next.len(), 1, "the note was not in the history, so it is sent now");
        // Called twice before a message lands: still one copy.
        let mut twice = convo();
        let mut t = String::new();
        mw.before_turn(&mut twice, &mut t);
        mw.before_turn(&mut twice, &mut t);
        assert_eq!(twice.pending_context.len(), 1);
    }

    #[test]
    fn a_new_conversation_and_a_compaction_each_get_the_note_again() {
        let c = cell(answer("C:\\w\\a.py", 5));
        let (hub, _server) = editor(&c);
        let mw = IdeContext::new(hub);
        let mut one = convo();
        assert_eq!(turn(&mw, &mut one, "a").len(), 1);
        assert!(turn(&mw, &mut one, "b").is_empty());
        // The same middleware serves a second conversation (a fork, or /clear's new session).
        let mut two = convo();
        assert_eq!(turn(&mw, &mut two, "a").len(), 1);
        // Compaction replaced the history that held the note.
        one.messages.clear();
        one.summary = Some("earlier".into());
        assert_eq!(turn(&mw, &mut one, "c").len(), 1);
    }

    #[test]
    fn no_editor_means_no_note_and_no_wait() {
        let home = std::env::temp_dir().join(format!("marlowe-ide-none-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        let hub = Hub::start(HubOpts::new(Paths::new(&home, None), ConnectOpts::new(Arc::new(MemoryTokens::default()))));
        assert!(hub.wait_settled(Duration::from_secs(5)));
        let mw = IdeContext::new(hub);
        let mut convo = convo();
        let t0 = std::time::Instant::now();
        assert!(turn(&mw, &mut convo, "hello").is_empty());
        assert!(t0.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn an_editor_with_nothing_to_report_adds_nothing() {
        let c = cell(json!({"selection": null, "openEditors": []}).to_string());
        let (hub, _server) = editor(&c);
        let mw = IdeContext::new(hub);
        assert!(turn(&mw, &mut convo(), "hello").is_empty());
    }
}
