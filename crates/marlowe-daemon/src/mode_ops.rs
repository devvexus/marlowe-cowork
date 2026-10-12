//! Permission modes, rules, hooks and settings (track E, design 2.3 / 2.6 / 3.5): SetMode,
//! ListRules, AddRule, RemoveRule, ListHooks, TrustHooks, GetSettings, SetSetting.
//!
//! Every session gets one [`Permissions`] (mode, layered settings, granted rules) and one
//! [`HookRunner`] as services, and one [`HookMiddleware`] as middleware and observer. They are kept
//! per session in [`Registry`] (in `Core::ext`) so a mode change or a settings edit reaches a
//! running session at once, until [`forget`] drops them when the session closes. One PowerShell
//! parser child serves every session. Prompt hooks are judged on the Quick slot (else the
//! Conductor's).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use marlowe_agent::approval::{default_pwsh, ModeSink, Permissions, PwshParser};
use marlowe_agent::hooks::{HookMiddleware, HookRunner, PromptHookEvaluator};
use marlowe_agent::{Conversation, Services, ToolMiddleware, TurnObserver};
use marlowe_model::{CancelToken, ChatRequest, Message};
use marlowe_proto::{Event, HookInfo, Request, RuleAction, SessionId, SettingsScope, Slot};
use marlowe_settings::hooks::TrustStore;
use marlowe_settings::{edit, Settings};

use crate::daemon::Core;

/// One session's permission state, hook runner and hook middleware.
#[derive(Clone)]
struct Entry {
    perms: Permissions,
    runner: Arc<HookRunner>,
    hooks: Arc<HookMiddleware>,
}

/// Live sessions' entries, by session, and the PowerShell parser they share.
struct Registry {
    sessions: Mutex<HashMap<SessionId, Entry>>,
    pwsh: Option<PwshParser>,
}

fn registry(core: &Arc<Core>) -> Arc<Registry> {
    core.ext.get_or_init(|| Registry { sessions: Mutex::new(HashMap::new()), pwsh: default_pwsh() })
}

/// Drops a closed session's entry: its SessionEnd hooks run (`reason`: `clear`, `logout`,
/// `prompt_input_exit`, `other`), and its scratch dir goes once the last clone is dropped.
/// The session close path calls this; the next use of the session builds a fresh entry.
pub(crate) fn forget(core: &Arc<Core>, session: &SessionId, reason: &str) {
    let reg = registry(core);
    let gone = reg.sessions.lock().unwrap_or_else(|p| p.into_inner()).remove(session);
    if let Some(e) = gone {
        let _ = e.runner.session_end(reason);
    }
}

/// The entry of `convo`'s session, created on first use.
fn entry(core: &Arc<Core>, convo: &Conversation) -> Entry {
    let reg = registry(core);
    let mut map = reg.sessions.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(e) = map.get(&convo.session) {
        return e.clone();
    }
    let e = build(core, &reg, convo.session.clone(), convo.workspace.as_deref());
    map.insert(convo.session.clone(), e.clone());
    e
}

/// `session`'s hook runner (built on first use, as a conversation's is): what a workflow run
/// started outside a turn fires its SubagentStart/SubagentStop through.
pub(crate) fn session_hooks(core: &Arc<Core>, session: &SessionId, workspace: Option<&Path>) -> Arc<HookRunner> {
    let reg = registry(core);
    let mut map = reg.sessions.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(e) = map.get(session) {
        return e.runner.clone();
    }
    let e = build(core, &reg, session.clone(), workspace);
    map.insert(session.clone(), e.clone());
    e.runner
}

fn build(core: &Arc<Core>, reg: &Registry, session: SessionId, workspace: Option<&Path>) -> Entry {
    // The session's own scratch dir, not the whole temp dir (deletes there are outside).
    let name: String = session.0.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect();
    let scratch = std::env::temp_dir().join("marlowe").join(name);
    let perms = Permissions::with_parser(&core.home, workspace, None, reg.pwsh.clone()).with_scratch(scratch);
    let hooks = perms.settings().hooks();
    let runner = HookRunner::new(hooks, workspace.map(Path::to_path_buf), session)
        .with_permissions(perms.clone())
        .with_prompt_evaluator(Arc::new(QuickJudge { core: Arc::downgrade(core) }));
    let runner = Arc::new(runner);
    let hooks = Arc::new(HookMiddleware::new(runner.clone()));
    Entry { perms, runner, hooks }
}

/// Re-reads settings for every live session in `workspace` (or every session when `None`).
fn reload(core: &Arc<Core>, workspace: Option<&Path>) {
    let reg = registry(core);
    let map = reg.sessions.lock().unwrap_or_else(|p| p.into_inner());
    for e in map.values() {
        if workspace.is_none() || e.perms.workspace() == workspace {
            e.perms.reload();
            e.runner.set_hooks(e.perms.settings().hooks());
        }
    }
}

pub(crate) fn dispatch(core: &Arc<Core>, client: u64, req: Request) {
    let reply = |e: Event| core.bus.reply(client, e);
    let fail = |session: Option<SessionId>, message: String| core.bus.reply(client, Event::Error { session, message });
    match req {
        Request::SetMode { session, mode } => {
            let reg = registry(core);
            let existing = reg.sessions.lock().unwrap_or_else(|p| p.into_inner()).get(&session).cloned();
            let e = match existing {
                Some(e) => e,
                None => match core.session(&session) {
                    Ok(h) => {
                        // Loading the session may have started its runner, which made the entry.
                        let mut map = reg.sessions.lock().unwrap_or_else(|p| p.into_inner());
                        map.entry(session.clone())
                            .or_insert_with(|| build(core, &reg, session.clone(), h.workspace.as_deref().map(Path::new)))
                            .clone()
                    }
                    Err((s, m)) => return fail(s, m),
                },
            };
            e.perms.set_mode(mode);
            core.bus.publish(Event::ModeChanged { session, mode });
        }
        Request::ListRules { workspace } => reply(rules_event(core, workspace.as_deref())),
        Request::AddRule { action, pattern, scope, workspace } => {
            let ws = workspace.as_deref().map(PathBuf::from);
            match add_rule(core, action, &pattern, scope, ws.as_deref()) {
                Ok(()) => reply(rules_event(core, workspace.as_deref())),
                Err(m) => fail(None, m),
            }
        }
        Request::RemoveRule { pattern, scope, workspace } => {
            let ws = workspace.as_deref().map(PathBuf::from);
            match Settings::remove_rule(&core.home, ws.as_deref(), scope, &pattern) {
                Ok(true) => {
                    reload(core, ws.as_deref());
                    reply(rules_event(core, workspace.as_deref()))
                }
                Ok(false) => fail(None, format!("`{pattern}` is not in the {scope:?} settings")),
                Err(e) => fail(None, e.to_string()),
            }
        }
        Request::ListHooks { workspace } => reply(hooks_event(core, workspace.as_deref())),
        Request::TrustHooks { workspace, digest } => {
            let ws = PathBuf::from(&workspace);
            let s = Settings::load(&core.home, Some(&ws), None);
            if s.digest != digest {
                return fail(None, "the project's hooks or settings changed since you looked; review them again".into());
            }
            match TrustStore::trust(&core.home, &ws, &digest) {
                Ok(()) => {
                    reload(core, Some(&ws));
                    reply(hooks_event(core, Some(&workspace)))
                }
                Err(e) => fail(None, e.to_string()),
            }
        }
        Request::GetSettings { workspace } => reply(settings_event(core, workspace.as_deref())),
        Request::SetSetting { scope, key, value, workspace } => {
            let ws = workspace.as_deref().map(PathBuf::from);
            let Some(path) = marlowe_settings::layers::Paths::new(&core.home, ws.as_deref()).for_scope(scope) else {
                return fail(None, "a setting needs a file: choose user, project or local".into());
            };
            let was_trusted = ws.as_deref().is_some_and(|w| Settings::load(&core.home, Some(w), None).trusted);
            match edit::set_key(&path, &key, &value) {
                Ok(()) => {
                    if let (Some(w), true) = (ws.as_deref(), scope != SettingsScope::User) {
                        let _ = Settings::retrust_after_edit(&core.home, w, was_trusted);
                    }
                    reload(core, ws.as_deref());
                    reply(settings_event(core, workspace.as_deref()))
                }
                Err(e) => fail(None, e.to_string()),
            }
        }
        other => crate::client::not_yet(core, client, &other, "E"),
    }
}

fn add_rule(core: &Arc<Core>, action: RuleAction, pattern: &str, scope: SettingsScope, ws: Option<&Path>) -> Result<(), String> {
    marlowe_settings::rules::validate(pattern)?;
    if scope == SettingsScope::Session {
        if action != RuleAction::Allow {
            return Err("only allow rules can be added for this session alone".into());
        }
        let reg = registry(core);
        let map = reg.sessions.lock().unwrap_or_else(|p| p.into_inner());
        let mut any = false;
        for e in map.values().filter(|e| ws.is_none() || e.perms.workspace() == ws) {
            e.perms.grant(&[pattern.to_string()], SettingsScope::Session)?;
            any = true;
        }
        return if any { Ok(()) } else { Err("no live session to add the rule to".into()) };
    }
    // A user's allow in a project file is recorded as theirs, so it applies untrusted too.
    Settings::grant(&core.home, ws, scope, action, &[pattern.to_string()]).map_err(|e| e.to_string())?;
    reload(core, ws);
    Ok(())
}

fn rules_event(core: &Arc<Core>, workspace: Option<&str>) -> Event {
    let ws = workspace.map(PathBuf::from);
    let s = Settings::load(&core.home, ws.as_deref(), None);
    let mut rules: Vec<_> = s.rules().iter().map(|r| r.info()).collect();
    let reg = registry(core);
    let map = reg.sessions.lock().unwrap_or_else(|p| p.into_inner());
    for e in map.values().filter(|e| e.perms.workspace() == ws.as_deref()) {
        for pattern in e.perms.session_rules() {
            let info = marlowe_proto::RuleInfo { action: RuleAction::Allow, pattern, scope: SettingsScope::Session, source: Some("this session".into()) };
            if !rules.contains(&info) {
                rules.push(info);
            }
        }
    }
    Event::Rules { rules }
}

fn hooks_event(core: &Arc<Core>, workspace: Option<&str>) -> Event {
    let ws = workspace.map(PathBuf::from);
    let s = Settings::load(&core.home, ws.as_deref(), None);
    let hooks: Vec<HookInfo> = s.hook_rows().into_iter().map(|(h, trusted)| h.info(trusted)).collect();
    Event::Hooks { hooks, digest: s.digest.clone() }
}

fn settings_event(core: &Arc<Core>, workspace: Option<&str>) -> Event {
    let ws = workspace.map(PathBuf::from);
    Event::Settings { layers: Settings::load(&core.home, ws.as_deref(), None).layer_views() }
}

/// E's loop middleware for one session (hooks), added by `stack.rs`. Modes and rules are decided
/// in the call runner from the [`Permissions`] service.
pub(crate) fn middleware(core: &Arc<Core>, convo: &Conversation) -> Vec<Arc<dyn ToolMiddleware>> {
    vec![entry(core, convo).hooks]
}

/// E's observers for one session (SessionStart and Notification hooks), added by `stack.rs`.
pub(crate) fn observers(core: &Arc<Core>, convo: &Conversation) -> Vec<Arc<dyn TurnObserver>> {
    vec![entry(core, convo).hooks]
}

/// E's services for one session (permissions, the hook runner), added by `stack.rs`.
pub(crate) fn services(core: &Arc<Core>, convo: &Conversation, services: Services) -> Services {
    let e = entry(core, convo);
    // `exit_plan` changes the mode from inside a turn: the clients hear it as from SetMode.
    let weak = Arc::downgrade(core);
    let sink = ModeSink(Arc::new(move |session: &SessionId, mode| {
        if let Some(core) = weak.upgrade() {
            core.bus.publish(Event::ModeChanged { session: session.clone(), mode });
        }
    }));
    services.with(e.perms).with(e.runner).with(sink)
}

/// Judges prompt hooks with one short model call.
struct QuickJudge {
    core: Weak<Core>,
}

const JUDGE_TIMEOUT: Duration = Duration::from_secs(30);

const JUDGE_SYSTEM: &str = "You evaluate a hook for a coding agent. Read the instruction and the event JSON, then answer \
with one JSON object and nothing else: {\"decision\": \"approve\" or \"block\", \"reason\": \"...\"}. Block only when \
the instruction says the action should not proceed or the work is not finished.";

impl PromptHookEvaluator for QuickJudge {
    fn evaluate(&self, prompt: &str) -> Result<String, String> {
        let core = self.core.upgrade().ok_or("the daemon is shutting down")?;
        let (provider, info, _) =
            core.brains.agent_parts(&Slot::Quick).or_else(|_| core.brains.agent_parts(&Slot::Conductor)).map_err(|e| e.to_string())?;
        let mut req = ChatRequest::new(info.id.clone(), vec![Message::user(prompt)]);
        req.system = vec![marlowe_model::SystemBlock { text: JUDGE_SYSTEM.into(), cache: false }];
        req.max_output_tokens = Some(400);
        let cancel = CancelToken::new();
        let timer = cancel.clone();
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let finished = done.clone();
        std::thread::spawn(move || {
            let until = std::time::Instant::now() + JUDGE_TIMEOUT;
            while std::time::Instant::now() < until {
                if finished.load(std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            timer.cancel();
        });
        let out = provider.stream(&req, &cancel, &mut |_| {});
        done.store(true, std::sync::atomic::Ordering::SeqCst);
        let text = out.map_err(|e| e.to_string())?.text;
        // The answer is the JSON object in the text.
        match (text.find('{'), text.rfind('}')) {
            (Some(a), Some(b)) if b > a => Ok(text[a..=b].to_string()),
            _ => Err(format!("the model did not answer with JSON: {}", text.chars().take(120).collect::<String>())),
        }
    }
}
