//! The turn rows (design 3.4 "Turn persistence"): a [`SessionRecorder`] writes the conversation
//! to the store as the loop produces it, instead of once when a turn ends.
//!
//! - `on_model_call`: the model's answer, as an assistant row with its provider, model, usage and
//!   latency, together with anything appended since the last write (the user's words, steers,
//!   reminders).
//! - `on_tools_done`: the batch's results, one row.
//! - `after_call`: each tool call's outcome the moment it finishes, so a daemon killed halfway
//!   through a parallel batch loses none of the calls that completed.
//! - `before_compact` / `finish_turn`: whatever is still unwritten.
//!
//! A hard kill therefore loses at most the model call in flight. [`repair_dangling`] closes the
//! one gap the loop leaves: a stored assistant message whose tool calls never got results.

use std::sync::Mutex;

use marlowe_agent::context::{TailReminders, TurnNotes, TurnQuery};
use marlowe_agent::system::PromptSections;
use marlowe_agent::{CallCtx, Conversation, ModelCall, StepInfo, ToolMiddleware, ToolOutcome, TurnFlow, TurnObserver};
use marlowe_model::{Message, ModelRef, Role, ToolCall, ToolResult, Usage};
use marlowe_mind::Mind;
use marlowe_proto::SessionId;

use crate::now_ms;
use crate::store::{CallMeta, CallResultRow, RowIn, Store, NOTE, SECTIONS, SYSTEM, TAIL, TOOLS};

/// What a tool call that was running when the daemon stopped reports on resume.
pub(crate) const STOPPED_MID_CALL: &str =
    "This call did not finish: the daemon stopped while it was running. It may or may not have taken effect; check before repeating it.";

struct Inner {
    /// Messages of `convo.messages` below this are stored.
    cursor: usize,
    /// History was compacted (everything was written first): the cursor is the whole history.
    resync: bool,
    turn: u32,
    provider: String,
    model: String,
    /// When the turn began and the memory journal's seq then, for the turn's first row.
    started_ms: i64,
    mem_seq: Option<u64>,
    first_row_pending: bool,
    /// The user's message as `before_turn` stored it, until the first write checks it against
    /// what the loop pushed.
    provisional: Option<Message>,
    /// The last store failure, for the runner to report; the unwritten rows are retried.
    error: Option<String>,
    /// The tail and note of this turn already saved, and the sections last saved.
    saved_tail: Option<String>,
    saved_tools: Option<String>,
    saved_system: Option<String>,
    saved_note: Option<String>,
    saved_sections: Option<String>,
    /// `convo.messages.len()` when the turn began.
    turn_start: usize,
}

/// Writes one session's conversation to the store as it happens. Shared (an `Arc`) between the
/// loop's middleware and observer lists and the session runner.
pub(crate) struct SessionRecorder {
    session: SessionId,
    store: Store,
    mind: Option<Mind>,
    inner: Mutex<Inner>,
}

impl SessionRecorder {
    pub fn new(session: SessionId, store: Store, mind: Option<Mind>, stored_len: usize) -> Self {
        let inner = Inner {
            cursor: stored_len,
            resync: false,
            turn: 0,
            provider: String::new(),
            model: String::new(),
            started_ms: now_ms(),
            mem_seq: None,
            first_row_pending: false,
            provisional: None,
            error: None,
            saved_tail: None,
            saved_tools: None,
            saved_system: None,
            saved_note: None,
            saved_sections: None,
            turn_start: stored_len,
        };
        Self { session, store, mind, inner: Mutex::new(inner) }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// A turn is about to run on `convo`: everything in it is stored.
    pub fn begin_turn(&self, convo: &Conversation, model: &ModelRef) {
        let mem_seq = self.mind.as_ref().and_then(|m| m.journal_seq().ok());
        let mut g = self.lock();
        g.cursor = convo.messages.len();
        g.turn_start = convo.messages.len();
        g.resync = false;
        g.turn = convo.next_turn;
        g.provider = model.provider.as_str().to_string();
        g.model = model.model_id.clone();
        g.started_ms = now_ms();
        g.mem_seq = mem_seq;
        g.first_row_pending = true;
        g.provisional = None;
        g.error = None;
        g.saved_tail = None;
        g.saved_note = None;
    }

    /// History was replaced from the store (a rewind): everything in `convo` is stored.
    pub fn reset(&self, convo: &Conversation) {
        let mut g = self.lock();
        g.cursor = convo.messages.len();
        g.resync = false;
        g.first_row_pending = false;
        g.provisional = None;
    }

    /// The turn was refused before it began (a hook blocked the prompt): its stored prompt goes.
    pub fn discard_turn(&self, turn: u32) {
        let mut g = self.lock();
        g.provisional = None;
        g.first_row_pending = false;
        if let Err(e) = self.store.drop_from(&self.session, turn, now_ms()) {
            g.error = Some(format!("blocked prompt not removed: {e}"));
        }
    }

    /// The last write failure since the turn began, once.
    pub fn take_error(&self) -> Option<String> {
        self.lock().error.take()
    }

    /// Writes what the turn left unwritten. `user_text` and the lengths before the turn find the
    /// turn's messages when history was compacted without the loop's `before_compact` hook (a
    /// path that should not exist, kept so a turn is never lost).
    pub fn finish_turn(&self, convo: &Conversation, user_text: &str, before_len: usize) {
        let mut g = self.lock();
        if convo.messages.len() < g.cursor && !g.resync {
            let user = Message::user(user_text);
            g.cursor = convo.messages.iter().rposition(|m| *m == user).unwrap_or(before_len.min(convo.messages.len()));
        }
        self.write(&mut g, convo, None);
        if g.error.is_none() {
            // A turn that stored nothing (it failed before the first call) still moves the session.
            let _ = self.store.append_rows(&self.session, Vec::new(), convo.next_turn, convo.exposure.tainted, now_ms());
        }
    }

    /// Keeps what this turn's request carried beyond its words (the per-turn tail, the teams note) and
    /// the session's prompt sections, so a session loaded again sends the same bytes: a request that
    /// no longer starts with the last one costs the model a full re-read.
    fn save_context(&self, g: &mut Inner, convo: &Conversation) {
        let live = convo.ext.get::<TurnQuery>().map(|q| q.0);
        let at = |entries: Option<&Vec<(usize, String)>>| {
            entries.and_then(|v| v.iter().rev().find(|(i, _)| Some(*i) == live).map(|(_, t)| t.clone()))
        };
        let tail = at(convo.ext.get::<TailReminders>().map(|t| &t.0));
        let note = at(convo.ext.get::<TurnNotes>().map(|t| &t.0));
        let sections = convo.ext.get::<PromptSections>().and_then(|s| serde_json::to_string(s).ok());
        let put = |g: &mut Inner, turn: u32, kind: &str, text: &str, offset: u32| {
            if let Err(e) = self.store.put_context(&self.session, turn, kind, text, offset) {
                g.error = Some(format!("turn context not saved: {e}"));
            }
        };
        let turn = g.turn;
        // Hooks may put messages ahead of the prompt: the text is placed by its row in the turn.
        let offset = live.and_then(|l| l.checked_sub(g.turn_start)).unwrap_or(0) as u32;
        if let Some(t) = tail.filter(|t| g.saved_tail.as_ref() != Some(t)) {
            put(g, turn, TAIL, &t, offset);
            g.saved_tail = Some(t);
        }
        if let Some(t) = note.filter(|t| g.saved_note.as_ref() != Some(t)) {
            put(g, turn, NOTE, &t, offset);
            g.saved_note = Some(t);
        }
        let tools = convo
            .ext
            .get::<marlowe_agent::toolset::FrozenTools>()
            .and_then(|f| serde_json::to_string(&serde_json::json!({"specs": f.specs, "told": f.told})).ok());
        let system =
            convo.ext.get::<marlowe_agent::context::FrozenSystem>().and_then(|f| serde_json::to_string(&f.blocks).ok());
        if let Some(t) = system.filter(|t| g.saved_system.as_ref() != Some(t)) {
            put(g, 0, SYSTEM, &t, 0);
            g.saved_system = Some(t);
        }
        if let Some(t) = tools.filter(|t| g.saved_tools.as_ref() != Some(t)) {
            put(g, 0, TOOLS, &t, 0);
            g.saved_tools = Some(t);
        }
        if let Some(t) = sections.filter(|t| g.saved_sections.as_ref() != Some(t)) {
            put(g, 0, SECTIONS, &t, 0);
            g.saved_sections = Some(t);
        }
    }

    /// Writes `convo.messages[cursor..]`, then `extra` (a message the loop is about to append),
    /// in one transaction. On success the cursor passes both.
    fn write(&self, g: &mut Inner, convo: &Conversation, extra: Option<(Message, Option<CallMeta>, Option<u32>)>) {
        if let Some(stored) = g.provisional.take() {
            // The loop may have pushed something other than what `before_turn` saw (a later hook
            // rewrote the prompt, attachments joined it): the stored row follows the loop.
            if let Some(actual) = g.cursor.checked_sub(1).and_then(|i| convo.messages.get(i)) {
                if *actual != stored {
                    if let Err(e) = self.store.replace_last(&self.session, actual.clone()) {
                        g.error = Some(format!("turn not saved: {e}"));
                    }
                }
            }
        }
        if g.resync || convo.messages.len() < g.cursor {
            g.cursor = convo.messages.len();
            g.resync = false;
        }
        let at = now_ms();
        let mut rows: Vec<RowIn> = Vec::new();
        for m in &convo.messages[g.cursor..] {
            rows.push(self.row(g, m.clone(), None, None, at));
        }
        let pushed = extra.is_some();
        if let Some((message, meta, step)) = extra {
            rows.push(self.row(g, message, meta, step, at));
        }
        if rows.is_empty() {
            return;
        }
        match self.store.append_rows(&self.session, rows, convo.next_turn, convo.exposure.tainted, at) {
            Ok(()) => g.cursor = convo.messages.len() + usize::from(pushed),
            Err(e) => g.error = Some(format!("turn not saved: {e}")),
        }
    }

    fn row(&self, g: &mut Inner, message: Message, meta: Option<CallMeta>, step: Option<u32>, at: i64) -> RowIn {
        let first = std::mem::take(&mut g.first_row_pending) && message.role == Role::User;
        RowIn {
            message,
            turn: g.turn,
            step,
            at: if first { g.started_ms } else { at },
            meta,
            mem_seq: if first { g.mem_seq } else { None },
        }
    }
}

impl TurnObserver for SessionRecorder {
    fn on_request(&self, convo: &Conversation, _req: &marlowe_model::ChatRequest) {
        let mut g = self.lock();
        self.save_context(&mut g, convo);
    }

    fn on_model_call(&self, convo: &Conversation, call: &ModelCall<'_>) {
        let mut g = self.lock();
        let c = call.completion;
        let meta = CallMeta {
            provider: g.provider.clone(),
            model: call.request.model.clone(),
            usage: c.usage,
            latency_ms: call.latency_ms,
        };
        // The loop keeps whatever the model produced (reasoning alone included); an empty answer
        // has no row to carry its usage, but it was billed all the same.
        let message = c.kept_message();
        if message.is_none() && meta.usage != Usage::default() {
            if let Err(e) = self.store.put_spend(&self.session, g.turn, Some(call.info.step), meta.clone(), now_ms()) {
                g.error = Some(format!("usage not saved: {e}"));
            }
        }
        let extra = message.map(|m| (m, Some(meta), Some(call.info.step)));
        self.save_context(&mut g, convo);
        self.write(&mut g, convo, extra);
    }

    fn on_tools_done(&self, convo: &Conversation, info: StepInfo, _calls: &[ToolCall], results: &[ToolResult]) {
        let mut g = self.lock();
        self.write(&mut g, convo, Some((Message::tool_results(results.iter().cloned()), None, Some(info.step))));
    }
}

impl ToolMiddleware for SessionRecorder {
    fn name(&self) -> &str {
        "turn-rows"
    }

    /// Stores the prompt the moment the turn begins, so a daemon killed during the first model
    /// call still has what the user asked. Runs last among the hooks that may rewrite it.
    fn before_turn(&self, convo: &mut Conversation, text: &mut String) -> TurnFlow {
        let mut g = self.lock();
        let message = Message::user(text.as_str());
        let at = now_ms();
        // Messages a hook pushed ahead of the prompt (a file-change or job reminder) are part of
        // the turn's request: stored first, or a session loaded again would send a history that
        // no longer starts with the one the model's cache holds.
        let ahead: Vec<Message> = convo.messages.get(g.cursor..).map(<[Message]>::to_vec).unwrap_or_default();
        let mut rows: Vec<RowIn> = ahead.into_iter().map(|m| self.row(&mut g, m, None, None, at)).collect();
        rows.push(self.row(&mut g, message.clone(), None, None, at));
        match self.store.append_rows(&self.session, rows, convo.next_turn + 1, convo.exposure.tainted, at) {
            Ok(()) => {
                g.cursor = convo.messages.len() + 1;
                g.provisional = Some(message);
            }
            Err(e) => g.error = Some(format!("turn not saved: {e}")),
        }
        TurnFlow::Continue
    }

    fn after_call(&self, call: &ToolCall, cx: &CallCtx, outcome: &mut ToolOutcome) {
        let row = CallResultRow { call_id: call.id.clone(), content: outcome.content.clone(), is_error: outcome.is_error };
        // Best effort: the batch row is the record; this only narrows what a kill can lose.
        let _ = self.store.put_call_result(&cx.session, row, now_ms());
    }

    fn before_compact(&self, convo: &mut Conversation, _focus: Option<&str>) {
        let mut g = self.lock();
        self.write(&mut g, convo, None);
        // Compaction replaces a prefix of `messages` with a summary; all of it is stored.
        g.resync = true;
    }
}

/// A session loaded from the store whose last message is an assistant turn with tool calls that
/// never got results (the daemon was killed while they ran) is made well-formed before the model
/// sees it: each call gets the outcome it recorded if it finished, else a note saying it did not.
/// Returns how many calls were closed. Only for a session with no live runner.
pub(crate) fn repair_dangling(store: &Store, convo: &mut Conversation) -> Result<usize, String> {
    let Some(last) = convo.messages.last() else { return Ok(0) };
    if last.role != Role::Assistant {
        return Ok(0);
    }
    let calls: Vec<ToolCall> = last.tool_calls().cloned().collect();
    if calls.is_empty() {
        return Ok(0);
    }
    let kept = store.call_results(&convo.session)?;
    let results = calls.iter().map(|c| match kept.iter().find(|k| k.call_id == c.id) {
        Some(k) => ToolResult::text(c.id.clone(), k.content.clone(), k.is_error),
        None => ToolResult::text(c.id.clone(), STOPPED_MID_CALL, true),
    });
    let message = Message::tool_results(results);
    let row = RowIn {
        message: message.clone(),
        turn: convo.next_turn.saturating_sub(1),
        step: None,
        at: now_ms(),
        meta: None,
        mem_seq: None,
    };
    store.append_rows(&convo.session, vec![row], convo.next_turn, convo.exposure.tainted, now_ms())?;
    convo.push(message);
    Ok(calls.len())
}
