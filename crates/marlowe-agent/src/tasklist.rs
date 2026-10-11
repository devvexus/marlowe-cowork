//! The persisted task list (T34): `todo_add`, `todo_update`, `todo_get`; stable ids, `blocked_by`,
//! shared by the conductor, its subagents and workflow agents.
//!
//! One list per conversation (keyed by the conductor's session id; a subagent's session id is
//! `<conductor>/<agent>` and reaches the same list). The list lives in the [`TaskLists`] service,
//! which writes every change through a [`TaskListStore`] (the daemon's key-value table), so it
//! survives compaction, `--resume` and a daemon restart, and tells the clients through the
//! [`TodoSink`] after each change. Whoever changes an item (the conductor, a subagent, a workflow
//! agent) changes the one list.
//!
//! The conductor's own [`TodoState`] mirrors the list, because that is what the stale-todo
//! reminder, the stop check and the pinned working set read; [`TaskListSync`] refreshes the mirror
//! at the start of a turn and after each step, which is how a subagent's update reaches it. A
//! subagent's `todo{items}` stays its own private list (Claude Code's TodoWrite is per agent); only
//! the `todo_*` tools and the conductor's `todo{items}` touch the shared one.
//!
//! An item is not startable (`in_progress`) while an item it is blocked by is open.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use marlowe_model::Message;
use marlowe_proto::{SessionId, TodoItem, TodoStatus};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::convo::Conversation;
use crate::middleware::{StepInfo, ToolMiddleware, TurnFlow};
use crate::todo::{render_todos, TodoSink, TodoState};
use crate::tools::{CallCtx, ToolOutcome};

/// A list as it is stored.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListState {
    pub items: Vec<TodoItem>,
    /// The id the next new item gets (ids are never reused within a session).
    pub next_id: u64,
}

/// Where lists are kept between runs. Called with the service's lock held: must not block long.
pub trait TaskListStore: Send + Sync {
    fn load(&self, root: &SessionId) -> Option<ListState>;
    fn save(&self, root: &SessionId, state: &ListState);
}

/// A store that keeps nothing across runs (tests, bare hosts).
#[derive(Default)]
pub struct MemoryListStore(Mutex<HashMap<SessionId, ListState>>);

impl TaskListStore for MemoryListStore {
    fn load(&self, root: &SessionId) -> Option<ListState> {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).get(root).cloned()
    }
    fn save(&self, root: &SessionId, state: &ListState) {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).insert(root.clone(), state.clone());
    }
}

/// The conductor's session id for any session of the conversation: a subagent's
/// `<conductor>/<agent>` and a workflow agent's reach the conductor's list.
pub fn root_of(session: &SessionId) -> SessionId {
    match session.as_str().split_once('/') {
        Some((root, _)) => SessionId(root.to_string()),
        None => session.clone(),
    }
}

/// Whether `session` is the conductor itself (not a subagent or workflow agent).
pub fn is_conductor(session: &SessionId) -> bool {
    !session.as_str().contains('/')
}

/// What `todo_update` changes (each `None` leaves the field).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Patch {
    pub subject: Option<String>,
    pub description: Option<String>,
    pub active_form: Option<String>,
    pub status: Option<TodoStatus>,
    pub owner: Option<String>,
    /// Replaces the item's blockers.
    pub blocked_by: Option<Vec<String>>,
    /// Adds to the item's blockers.
    pub add_blocked_by: Vec<String>,
}

struct Inner {
    store: Arc<dyn TaskListStore>,
    lists: Mutex<HashMap<SessionId, ListState>>,
    sink: Option<TodoSink>,
}

/// The shared task lists of the daemon's sessions (a service; cheap to clone).
#[derive(Clone)]
pub struct TaskLists {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for TaskLists {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TaskLists")
    }
}

impl TaskLists {
    pub fn new(store: Arc<dyn TaskListStore>, sink: Option<TodoSink>) -> Self {
        Self { inner: Arc::new(Inner { store, lists: Mutex::new(HashMap::new()), sink }) }
    }

    /// Lists that live only as long as this value.
    pub fn in_memory() -> Self {
        Self::new(Arc::new(MemoryListStore::default()), None)
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<SessionId, ListState>> {
        self.inner.lists.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Runs `f` on the session's list (loaded from the store on first use), saves it and tells
    /// the clients when `f` changed it.
    fn with<R>(&self, session: &SessionId, f: impl FnOnce(&mut ListState) -> Result<R, String>) -> Result<R, String> {
        let root = root_of(session);
        let (result, snapshot) = {
            let mut g = self.lock();
            let state = g.entry(root.clone()).or_insert_with(|| self.inner.store.load(&root).unwrap_or_default());
            let before = state.clone();
            let result = f(state);
            let changed = *state != before;
            if changed {
                self.inner.store.save(&root, state);
            }
            (result, changed.then(|| state.items.clone()))
        };
        if let (Some(items), Some(sink)) = (snapshot, &self.inner.sink) {
            (sink.0)(&root, &items);
        }
        result
    }

    /// The list now.
    pub fn snapshot(&self, session: &SessionId) -> Vec<TodoItem> {
        self.with(session, |s| Ok(s.items.clone())).unwrap_or_default()
    }

    pub fn get(&self, session: &SessionId, id: &str) -> Result<TodoItem, String> {
        self.with(session, |s| find(&s.items, id).cloned().ok_or_else(|| unknown(id, &s.items)))
    }

    /// `todo_add`.
    pub fn add(
        &self,
        session: &SessionId,
        subject: &str,
        description: &str,
        active_form: Option<String>,
        blocked_by: Vec<String>,
    ) -> Result<TodoItem, String> {
        let subject = subject.trim();
        if subject.is_empty() {
            return Err("`subject` is required: what the item is, in the imperative".into());
        }
        self.with(session, |s| {
            for b in &blocked_by {
                if find(&s.items, b).is_none() {
                    return Err(unknown(b, &s.items));
                }
            }
            s.next_id = s.next_id.max(next_after(&s.items));
            let id = s.next_id.to_string();
            s.next_id += 1;
            let item = TodoItem {
                content: subject.to_string(),
                status: TodoStatus::Pending,
                active_form: active_form.filter(|a| !a.trim().is_empty()),
                id: Some(id),
                description: description.trim().to_string(),
                owner: None,
                blocked_by: dedup(blocked_by),
            };
            s.items.push(item.clone());
            Ok(item)
        })
    }

    /// `todo_update`, made by `caller` (the owner when an item is started without one).
    pub fn update(&self, session: &SessionId, id: &str, patch: Patch, caller: &str) -> Result<TodoItem, String> {
        self.with(session, |s| {
            let at = s.items.iter().position(|i| i.id.as_deref() == Some(id.trim())).ok_or_else(|| unknown(id, &s.items))?;
            let mut item = s.items[at].clone();
            if let Some(subject) = patch.subject.as_deref().map(str::trim).filter(|x| !x.is_empty()) {
                item.content = subject.to_string();
            }
            if let Some(d) = &patch.description {
                item.description = d.trim().to_string();
            }
            if let Some(a) = &patch.active_form {
                item.active_form = Some(a.trim().to_string()).filter(|a| !a.is_empty());
            }
            if let Some(owner) = &patch.owner {
                item.owner = Some(owner.trim().to_string()).filter(|o| !o.is_empty());
            }
            let mut blockers = patch.blocked_by.clone().unwrap_or_else(|| item.blocked_by.clone());
            blockers.extend(patch.add_blocked_by.iter().cloned());
            let blockers = dedup(blockers);
            for b in &blockers {
                if b == id.trim() {
                    return Err(format!("item {id} cannot be blocked by itself"));
                }
                if find(&s.items, b).is_none() {
                    return Err(unknown(b, &s.items));
                }
                if blocks_transitively(&s.items, b, id.trim()) {
                    return Err(format!("item {b} already waits for item {id}; blocking {id} on it would be a cycle"));
                }
            }
            item.blocked_by = blockers;
            if let Some(status) = patch.status {
                if status == TodoStatus::InProgress && item.status != TodoStatus::InProgress {
                    let open: Vec<String> = item
                        .blocked_by
                        .iter()
                        .filter(|b| find(&s.items, b).is_some_and(|x| x.status != TodoStatus::Completed))
                        .map(|b| format!("#{b}"))
                        .collect();
                    if !open.is_empty() {
                        return Err(format!(
                            "item {id} is blocked by {} which {} not completed; finish {} first, or work on an item that is not blocked",
                            open.join(", "),
                            if open.len() == 1 { "is" } else { "are" },
                            if open.len() == 1 { "that one" } else { "those" },
                        ));
                    }
                    if item.owner.is_none() {
                        item.owner = Some(caller.to_string());
                    }
                }
                item.status = status;
            }
            s.items[at] = item.clone();
            Ok(item)
        })
    }

    /// The conductor's `todo{items}`: replaces the list. An item keeps its id, owner, description
    /// and blockers when its subject is unchanged; new subjects get new ids; blockers that no
    /// longer exist are dropped.
    pub fn replace(&self, session: &SessionId, incoming: Vec<TodoItem>) -> Vec<TodoItem> {
        self.with(session, |s| {
            let mut old = std::mem::take(&mut s.items);
            s.next_id = s.next_id.max(next_after(&old));
            let mut out: Vec<TodoItem> = Vec::with_capacity(incoming.len());
            for mut it in incoming {
                if let Some(at) = old.iter().position(|o| o.content.trim() == it.content.trim()) {
                    let o = old.remove(at);
                    it.id = o.id;
                    it.owner = o.owner;
                    it.description = o.description;
                    it.blocked_by = o.blocked_by;
                } else {
                    it.id = Some(s.next_id.to_string());
                    s.next_id += 1;
                }
                out.push(it);
            }
            let ids: Vec<String> = out.iter().filter_map(|i| i.id.clone()).collect();
            for it in &mut out {
                it.blocked_by.retain(|b| ids.contains(b));
            }
            s.items = out.clone();
            Ok(out)
        })
        .unwrap_or_default()
    }
}

fn find<'a>(items: &'a [TodoItem], id: &str) -> Option<&'a TodoItem> {
    let id = id.trim().trim_start_matches('#');
    items.iter().find(|i| i.id.as_deref() == Some(id))
}

fn unknown(id: &str, items: &[TodoItem]) -> String {
    let ids: Vec<&str> = items.iter().filter_map(|i| i.id.as_deref()).collect();
    if ids.is_empty() {
        format!("no item {id}: the list is empty")
    } else {
        format!("no item {id}; the list has {}", ids.join(", "))
    }
}

/// One past the highest numeric id in use.
fn next_after(items: &[TodoItem]) -> u64 {
    items.iter().filter_map(|i| i.id.as_deref()?.parse::<u64>().ok()).max().map_or(1, |m| m + 1)
}

fn dedup(ids: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for id in ids {
        let id = id.trim().trim_start_matches('#').to_string();
        if !id.is_empty() && !out.contains(&id) {
            out.push(id);
        }
    }
    out
}

/// Whether `from` waits (directly or through others) for `target`.
fn blocks_transitively(items: &[TodoItem], from: &str, target: &str) -> bool {
    let mut seen: Vec<&str> = Vec::new();
    let mut stack = vec![from];
    while let Some(cur) = stack.pop() {
        if seen.contains(&cur) {
            continue;
        }
        seen.push(cur);
        if let Some(item) = find(items, cur) {
            for b in &item.blocked_by {
                if b == target {
                    return true;
                }
                stack.push(b);
            }
        }
    }
    false
}

// ---------------------------------------------------------------- the tools

fn lists<'a>(cx: &'a CallCtx) -> Result<&'a TaskLists, ToolOutcome> {
    cx.scope.services.get::<TaskLists>().ok_or_else(|| ToolOutcome::error("the shared task list is not available in this session; use `todo`"))
}

/// Who is calling: `conductor`, or the subagent's name (its id when it has none).
pub fn caller_name(cx: &CallCtx) -> String {
    match cx.session.as_str().split_once('/') {
        None => "conductor".to_string(),
        Some((_, agent)) => cx
            .scope
            .services
            .get::<crate::subagent::AgentNames>()
            .and_then(|s| s.name_of(agent))
            .unwrap_or_else(|| agent.to_string()),
    }
}

fn str_arg(args: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|k| args.get(*k).and_then(Value::as_str)).map(str::to_string)
}

fn id_list(args: &Value, keys: &[&str]) -> Option<Vec<String>> {
    keys.iter().find_map(|k| args.get(*k)).map(|v| match v {
        Value::Array(a) => a
            .iter()
            .filter_map(|x| match x {
                Value::String(s) => Some(s.clone()),
                Value::Number(n) => Some(n.to_string()),
                _ => None,
            })
            .collect(),
        Value::String(s) => s.split(',').map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect(),
        Value::Number(n) => vec![n.to_string()],
        _ => Vec::new(),
    })
}

fn parse_status(s: &str) -> Result<TodoStatus, String> {
    match s.trim().to_ascii_lowercase().replace('-', "_").as_str() {
        "pending" | "todo" | "open" => Ok(TodoStatus::Pending),
        "in_progress" | "started" | "doing" => Ok(TodoStatus::InProgress),
        "completed" | "done" | "complete" => Ok(TodoStatus::Completed),
        other => Err(format!("status `{other}` is not pending, in_progress or completed")),
    }
}

/// Updates the conductor's mirror ([`TodoState`]) to the shared list. A subagent's own
/// extensions are left alone: it must not be nagged about the conductor's items.
pub fn mirror(cx: &CallCtx, lists: &TaskLists) {
    if !is_conductor(&cx.session) {
        return;
    }
    let items = lists.snapshot(&cx.session);
    cx.scope.with_ext(|e| {
        let st = e.get_or_default::<TodoState>();
        st.items = items;
        st.steps_since_update = 0;
        st.reminded = false;
    });
}

fn listing(items: &[TodoItem]) -> String {
    if items.is_empty() {
        "The list is empty.".into()
    } else {
        format!("Task list:\n{}", render_todos(items))
    }
}

/// The `todo_add` tool.
pub fn todo_add_call(args: &Value, cx: &CallCtx) -> ToolOutcome {
    let lists = match lists(cx) {
        Ok(l) => l,
        Err(e) => return e,
    };
    let Some(subject) = str_arg(args, &["subject", "content", "title"]) else {
        return ToolOutcome::error("`subject` is required: what the item is, in the imperative (\"Add the retry test\")");
    };
    let description = str_arg(args, &["description"]).unwrap_or_default();
    let active = str_arg(args, &["active_form", "activeForm"]);
    let blocked = id_list(args, &["blocked_by", "blockedBy"]).unwrap_or_default();
    match lists.add(&cx.session, &subject, &description, active, blocked) {
        Ok(item) => {
            mirror(cx, lists);
            let id = item.id.clone().unwrap_or_default();
            ToolOutcome::ok(format!("Added item {id}.\n\n{}", listing(&lists.snapshot(&cx.session))), format!("added #{id}"))
        }
        Err(e) => ToolOutcome::error(e),
    }
}

/// The `todo_update` tool.
pub fn todo_update_call(args: &Value, cx: &CallCtx) -> ToolOutcome {
    let lists = match lists(cx) {
        Ok(l) => l,
        Err(e) => return e,
    };
    let Some(id) = str_arg(args, &["id", "taskId", "task_id"]).or_else(|| args.get("id").and_then(Value::as_u64).map(|n| n.to_string())) else {
        return ToolOutcome::error("`id` is required: the item's id from the list");
    };
    let status = match str_arg(args, &["status"]).map(|s| parse_status(&s)).transpose() {
        Ok(s) => s,
        Err(e) => return ToolOutcome::error(e),
    };
    let patch = Patch {
        subject: str_arg(args, &["subject", "content"]),
        description: str_arg(args, &["description"]),
        active_form: str_arg(args, &["active_form", "activeForm"]),
        status,
        owner: str_arg(args, &["owner"]),
        blocked_by: id_list(args, &["blocked_by", "blockedBy"]),
        add_blocked_by: id_list(args, &["add_blocked_by", "addBlockedBy"]).unwrap_or_default(),
    };
    match lists.update(&cx.session, &id, patch, &caller_name(cx)) {
        Ok(item) => {
            mirror(cx, lists);
            let note = if item.status == TodoStatus::Completed { " Mark the next item in progress." } else { "" };
            ToolOutcome::ok(
                format!("Updated item {id}.{note}\n\n{}", listing(&lists.snapshot(&cx.session))),
                format!("#{id} {}", status_word(item.status)),
            )
        }
        Err(e) => ToolOutcome::error(e),
    }
}

fn status_word(s: TodoStatus) -> &'static str {
    match s {
        TodoStatus::Pending => "pending",
        TodoStatus::InProgress => "in progress",
        TodoStatus::Completed => "completed",
    }
}

/// The `todo_get` tool: one item in full, or the whole list.
pub fn todo_get_call(args: &Value, cx: &CallCtx) -> ToolOutcome {
    let lists = match lists(cx) {
        Ok(l) => l,
        Err(e) => return e,
    };
    let Some(id) = str_arg(args, &["id", "taskId", "task_id"]).or_else(|| args.get("id").and_then(Value::as_u64).map(|n| n.to_string())) else {
        let items = lists.snapshot(&cx.session);
        return ToolOutcome::ok(listing(&items), format!("{} items", items.len()));
    };
    match lists.get(&cx.session, &id) {
        Ok(item) => {
            let all = lists.snapshot(&cx.session);
            let this = item.id.clone().unwrap_or_default();
            let blocks: Vec<String> =
                all.iter().filter(|i| i.blocked_by.contains(&this)).filter_map(|i| i.id.as_ref().map(|x| format!("#{x}"))).collect();
            let waiting: Vec<String> = item
                .blocked_by
                .iter()
                .map(|b| match find(&all, b) {
                    Some(x) => format!("#{b} ({})", status_word(x.status)),
                    None => format!("#{b} (gone)"),
                })
                .collect();
            let mut text = format!("Item {this}: {}\nStatus: {}", item.content, status_word(item.status));
            if let Some(o) = &item.owner {
                text.push_str(&format!("\nOwner: {o}"));
            }
            if !item.description.is_empty() {
                text.push_str(&format!("\nDescription: {}", item.description));
            }
            if !waiting.is_empty() {
                text.push_str(&format!("\nBlocked by: {}", waiting.join(", ")));
            }
            if !blocks.is_empty() {
                text.push_str(&format!("\nBlocks: {}", blocks.join(", ")));
            }
            ToolOutcome::ok(text, format!("#{this}"))
        }
        Err(e) => ToolOutcome::error(e),
    }
}

// ---------------------------------------------------------------- the conductor's mirror

/// Keeps the conductor's [`TodoState`] equal to the shared list: loaded when a turn starts (a
/// resumed or restarted session finds its list), refreshed after each step (a subagent's update
/// reaches the stale-todo reminder and the stop check).
pub struct TaskListSync {
    lists: TaskLists,
}

impl TaskListSync {
    pub fn new(lists: TaskLists) -> Arc<Self> {
        Arc::new(Self { lists })
    }

    fn refresh(&self, convo: &mut Conversation) {
        if !is_conductor(&convo.session) {
            return;
        }
        let items = self.lists.snapshot(&convo.session);
        let st = convo.ext.get_or_default::<TodoState>();
        if st.items != items {
            st.items = items;
        }
    }
}

impl ToolMiddleware for TaskListSync {
    fn name(&self) -> &str {
        "task-list"
    }

    fn before_turn(&self, convo: &mut Conversation, _text: &mut String) -> TurnFlow {
        self.refresh(convo);
        TurnFlow::Continue
    }

    fn after_step(&self, _info: StepInfo, convo: &mut Conversation) -> Vec<Message> {
        self.refresh(convo);
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{SessionScope, ToolCtx};
    use crate::Services;
    use marlowe_model::CancelToken;
    use serde_json::json;

    fn sid(s: &str) -> SessionId {
        SessionId(s.into())
    }

    fn cx(lists: &TaskLists, session: &str) -> CallCtx {
        let tool = ToolCtx::new(sid(session), 0, None, CancelToken::new(), false);
        CallCtx::new(tool, SessionScope { ext: Default::default(), services: Services::new().with(lists.clone()) })
    }

    #[test]
    fn parity_t34_blocked_items_wait_for_their_blockers() {
        let lists = TaskLists::in_memory();
        let c = cx(&lists, "s1");
        let out = todo_add_call(&json!({"subject": "write the parser"}), &c);
        assert!(!out.is_error, "{}", out.content);
        let out = todo_add_call(&json!({"subject": "test the parser", "blocked_by": ["1"]}), &c);
        assert!(!out.is_error, "{}", out.content);
        let err = todo_update_call(&json!({"id": "2", "status": "in_progress"}), &c);
        assert!(err.is_error && err.content.contains("blocked by #1"), "{}", err.content);
        let ok = todo_update_call(&json!({"id": "1", "status": "in_progress"}), &c);
        assert!(!ok.is_error);
        let err = todo_update_call(&json!({"id": "2", "status": "in_progress"}), &c);
        assert!(err.is_error, "still blocked while 1 is in progress");
        let ok = todo_update_call(&json!({"id": "1", "status": "completed"}), &c);
        assert!(!ok.is_error);
        let ok = todo_update_call(&json!({"id": "2", "status": "in_progress"}), &c);
        assert!(!ok.is_error, "{}", ok.content);
        let two = lists.get(&sid("s1"), "2").unwrap();
        assert_eq!((two.status, two.owner.as_deref()), (TodoStatus::InProgress, Some("conductor")));
    }

    #[test]
    fn parity_t34_a_subagent_marks_a_conductor_item_done_in_the_one_list() {
        let lists = TaskLists::in_memory();
        let conductor = cx(&lists, "s1");
        todo_add_call(&json!({"subject": "fix the flaky test"}), &conductor);
        let sub = cx(&lists, "s1/agent-1");
        let done = todo_update_call(&json!({"id": "1", "status": "completed"}), &sub);
        assert!(!done.is_error, "{}", done.content);
        assert_eq!(lists.get(&sid("s1"), "1").unwrap().status, TodoStatus::Completed);
        // The subagent's own extensions are not given the conductor's items.
        assert!(sub.scope.with_ext(|e| e.get::<TodoState>().is_none()));
        // The conductor's mirror catches up at the step boundary.
        let sync = TaskListSync::new(lists.clone());
        let mut convo = Conversation::new(sid("s1"), None);
        sync.after_step(StepInfo { turn: 0, step: 1 }, &mut convo);
        assert_eq!(convo.ext.get::<TodoState>().unwrap().items[0].status, TodoStatus::Completed);
    }

    #[test]
    fn parity_t34_ids_are_stable_and_never_reused() {
        let lists = TaskLists::in_memory();
        let s = sid("s1");
        let a = lists.add(&s, "a", "", None, vec![]).unwrap();
        let b = lists.add(&s, "b", "", None, vec![]).unwrap();
        assert_eq!((a.id.as_deref(), b.id.as_deref()), (Some("1"), Some("2")));
        // TodoWrite replaces the list: "b" keeps its id, "a" goes, "c" is new and does not take 1.
        let mut items = vec![TodoItem { content: "b".into(), ..Default::default() }, TodoItem { content: "c".into(), ..Default::default() }];
        items[0].status = TodoStatus::InProgress;
        let out = lists.replace(&s, items);
        assert_eq!(out.iter().map(|i| i.id.as_deref().unwrap()).collect::<Vec<_>>(), ["2", "3"]);
        assert_eq!(out[0].status, TodoStatus::InProgress);
    }

    #[test]
    fn parity_t34_the_list_survives_a_restart_through_the_store() {
        let store: Arc<dyn TaskListStore> = Arc::new(MemoryListStore::default());
        let first = TaskLists::new(store.clone(), None);
        first.add(&sid("s1"), "survive", "details", None, vec![]).unwrap();
        first.add(&sid("s1"), "second", "", None, vec!["1".into()]).unwrap();
        drop(first);
        let after = TaskLists::new(store, None);
        let items = after.snapshot(&sid("s1/agent-3"));
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].description, "details");
        assert_eq!(items[1].blocked_by, ["1"]);
        assert_eq!(after.add(&sid("s1"), "third", "", None, vec![]).unwrap().id.as_deref(), Some("3"));
    }

    #[test]
    fn blockers_must_exist_and_cannot_form_a_cycle() {
        let lists = TaskLists::in_memory();
        let s = sid("s1");
        lists.add(&s, "a", "", None, vec![]).unwrap();
        lists.add(&s, "b", "", None, vec!["1".into()]).unwrap();
        assert!(lists.add(&s, "c", "", None, vec!["9".into()]).unwrap_err().contains("no item 9"));
        let cyc = lists.update(&s, "1", Patch { blocked_by: Some(vec!["2".into()]), ..Default::default() }, "x");
        assert!(cyc.unwrap_err().contains("cycle"));
        let me = lists.update(&s, "1", Patch { add_blocked_by: vec!["1".into()], ..Default::default() }, "x");
        assert!(me.unwrap_err().contains("itself"));
        assert!(lists.get(&s, "7").is_err());
    }

    #[test]
    fn todo_get_shows_one_item_in_full_and_the_list_without_an_id() {
        let lists = TaskLists::in_memory();
        let c = cx(&lists, "s1");
        todo_add_call(&json!({"subject": "design", "description": "sketch the schema"}), &c);
        todo_add_call(&json!({"subject": "build", "blocked_by": "1"}), &c);
        let one = todo_get_call(&json!({"id": "1"}), &c);
        assert!(one.content.contains("Blocks: #2") && one.content.contains("sketch the schema"), "{}", one.content);
        let two = todo_get_call(&json!({"id": "2"}), &c);
        assert!(two.content.contains("Blocked by: #1 (pending)"), "{}", two.content);
        let all = todo_get_call(&json!({}), &c);
        assert!(all.content.contains("design") && all.content.contains("build"));
    }
}
