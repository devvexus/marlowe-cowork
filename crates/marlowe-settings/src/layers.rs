//! Loading and merging the layers (see the crate root's table), the project trust digest, and
//! the wire view.

use std::path::{Path, PathBuf};

use marlowe_permission::rules::RuleSet;
use marlowe_proto::{PermissionMode, RuleAction, SettingsLayer, SettingsScope};

use crate::file::SettingsFile;
use crate::hooks::{self, HookEntry, HookEvent, HookKind, TrustStore, DEFAULT_TIMEOUT_SECS};
use crate::import;
use crate::rules::RuleEntry;

/// Where a layer came from, low to high precedence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Source {
    /// `<home>\settings.toml`.
    User,
    /// `~/.claude/settings.json` (only with `import.claude_user`).
    ClaudeUser,
    /// `<ws>\.claude\settings.json`.
    ClaudeProject,
    /// `<ws>\.marlowe\settings.toml`.
    Project,
    /// `<ws>\.claude\settings.local.json`.
    ClaudeLocal,
    /// `<ws>\.marlowe\settings.local.toml`.
    Local,
    /// `--settings`, headless flags.
    Cli,
    /// Approvals for this session only.
    Session,
}

impl Source {
    pub fn scope(self) -> SettingsScope {
        match self {
            Source::User | Source::ClaudeUser => SettingsScope::User,
            Source::ClaudeProject | Source::Project => SettingsScope::Project,
            Source::ClaudeLocal | Source::Local => SettingsScope::Local,
            Source::Cli | Source::Session => SettingsScope::Session,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Source::User => "user settings",
            Source::ClaudeUser => "~/.claude/settings.json",
            Source::ClaudeProject => ".claude/settings.json",
            Source::Project => ".marlowe/settings.toml",
            Source::ClaudeLocal => ".claude/settings.local.json",
            Source::Local => ".marlowe/settings.local.toml",
            Source::Cli => "command line",
            Source::Session => "this session",
        }
    }

    /// Lives in the workspace, so what it widens applies only once the user trusts it.
    pub fn project_scoped(self) -> bool {
        matches!(self, Source::ClaudeProject | Source::Project | Source::ClaudeLocal | Source::Local)
    }

    /// A Claude Code file Marlowe only reads.
    pub fn imported(self) -> bool {
        matches!(self, Source::ClaudeUser | Source::ClaudeProject | Source::ClaudeLocal)
    }
}

/// One loaded layer.
#[derive(Debug, Clone)]
pub struct Layer {
    pub source: Source,
    pub path: Option<PathBuf>,
    pub file: SettingsFile,
    /// The file's text as read (shown by `/config`).
    pub text: String,
}

/// The settings files of a workspace.
#[derive(Debug, Clone)]
pub struct Paths {
    pub user: PathBuf,
    pub claude_user: Option<PathBuf>,
    pub claude_project: Option<PathBuf>,
    pub project: Option<PathBuf>,
    pub claude_local: Option<PathBuf>,
    pub local: Option<PathBuf>,
}

impl Paths {
    pub fn new(home: &Path, workspace: Option<&Path>) -> Paths {
        let user_home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from);
        Paths {
            user: home.join("settings.toml"),
            claude_user: user_home.map(|h| h.join(".claude").join("settings.json")),
            claude_project: workspace.map(|w| w.join(".claude").join("settings.json")),
            project: workspace.map(|w| w.join(".marlowe").join("settings.toml")),
            claude_local: workspace.map(|w| w.join(".claude").join("settings.local.json")),
            local: workspace.map(|w| w.join(".marlowe").join("settings.local.toml")),
        }
    }

    /// The file a change at `scope` is written to (`Session` has none).
    pub fn for_scope(&self, scope: SettingsScope) -> Option<PathBuf> {
        match scope {
            SettingsScope::User => Some(self.user.clone()),
            SettingsScope::Project => self.project.clone(),
            SettingsScope::Local => self.local.clone(),
            SettingsScope::Session => None,
        }
    }
}

/// Every layer of one workspace, merged on demand.
#[derive(Debug, Clone)]
pub struct Settings {
    pub home: PathBuf,
    pub workspace: Option<PathBuf>,
    pub paths: Paths,
    /// Low to high precedence.
    pub layers: Vec<Layer>,
    /// The digest of what the project asks to be trusted with.
    pub digest: String,
    /// The user trusted this digest for this workspace (or there is nothing to trust).
    pub trusted: bool,
    /// Allow rules the user granted for this workspace through Marlowe ([`TrustStore::grant`]):
    /// in a project file they apply without trust and are not part of the digest.
    pub granted: Vec<String>,
    /// Files that could not be read or parsed, and invalid rules or hooks.
    pub errors: Vec<String>,
}

impl Settings {
    /// Loads every layer for `workspace` (and `cli`, highest), checking trust in `<home>`.
    pub fn load(home: &Path, workspace: Option<&Path>, cli: Option<SettingsFile>) -> Settings {
        Self::load_with(home, workspace, cli, &TrustStore::load(home))
    }

    pub fn load_with(home: &Path, workspace: Option<&Path>, cli: Option<SettingsFile>, trust: &TrustStore) -> Settings {
        let paths = Paths::new(home, workspace);
        let mut layers = Vec::new();
        let mut errors = Vec::new();
        let mut read = |source: Source, path: Option<&PathBuf>, layers: &mut Vec<Layer>| {
            let Some(path) = path else { return };
            let Ok(text) = std::fs::read_to_string(path) else { return };
            let parsed = if source.imported() { import::from_claude_json(&text) } else { SettingsFile::from_toml(&text) };
            match parsed {
                Ok(file) => layers.push(Layer { source, path: Some(path.clone()), file, text }),
                Err(e) => errors.push(format!("{}: {e}", path.display())),
            }
        };
        read(Source::User, Some(&paths.user), &mut layers);
        let import = layers.first().map(|l| l.file.import.clone()).unwrap_or_default();
        if import.claude_user {
            read(Source::ClaudeUser, paths.claude_user.as_ref(), &mut layers);
        }
        if import.claude {
            read(Source::ClaudeProject, paths.claude_project.as_ref(), &mut layers);
        }
        read(Source::Project, paths.project.as_ref(), &mut layers);
        if import.claude {
            read(Source::ClaudeLocal, paths.claude_local.as_ref(), &mut layers);
        }
        read(Source::Local, paths.local.as_ref(), &mut layers);
        if let Some(file) = cli {
            let text = toml::to_string(&file).unwrap_or_default();
            layers.push(Layer { source: Source::Cli, path: None, file, text });
        }
        let mut s = Settings {
            home: home.to_path_buf(),
            workspace: workspace.map(Path::to_path_buf),
            paths,
            layers,
            digest: String::new(),
            trusted: false,
            granted: workspace.map(|w| trust.granted_for(w)).unwrap_or_default(),
            errors,
        };
        s.digest = s.compute_digest();
        s.trusted = s.nothing_to_trust() || workspace.is_some_and(|w| trust.is_trusted(w, &s.digest));
        let mut more = Vec::new();
        for (e, why) in crate::rules::invalid(&s.all_rule_rows()) {
            let place = e.path.as_ref().map_or(e.source.label().to_string(), |p| p.display().to_string());
            more.push(format!("{place}: `{}`: {why}", e.pattern));
        }
        for l in &s.layers {
            layer_hooks(l, &mut more);
        }
        s.errors.extend(more);
        s
    }

    fn project_layers(&self) -> impl Iterator<Item = &Layer> {
        self.layers.iter().filter(|l| l.source.project_scoped())
    }

    /// Whether a layer's widening settings apply (the user's own, or a trusted project's).
    fn applies(&self, l: &Layer) -> bool {
        !l.source.project_scoped() || self.trusted
    }

    /// Nothing project-scoped widens anything (no hooks, allows, directories, commands, hosts or
    /// mode).
    fn nothing_to_trust(&self) -> bool {
        self.project_layers().all(|l| {
            let p = &l.file.permissions;
            l.file.hooks.values().all(Vec::is_empty)
                && p.allow.iter().all(|a| self.granted.contains(a))
                && p.additional_directories.is_empty()
                && p.project_commands.is_empty()
                && p.default_mode.is_none()
                && l.file.network.allow_hosts.is_empty()
        })
    }

    fn compute_digest(&self) -> String {
        let mut hooks = Vec::new();
        let mut allows = Vec::new();
        let mut dirs = Vec::new();
        let mut commands = Vec::new();
        let mut mode = None;
        for l in self.project_layers() {
            hooks.extend(layer_hooks(l, &mut Vec::new()));
            allows.extend(l.file.permissions.allow.iter().filter(|a| !self.granted.contains(a)).cloned());
            allows.extend(l.file.network.allow_hosts.iter().map(|h| format!("host:{h}")));
            dirs.extend(l.file.permissions.additional_directories.iter().cloned());
            commands.extend(l.file.permissions.project_commands.iter().cloned());
            if let Some(m) = &l.file.permissions.default_mode {
                mode = Some(m.clone());
            }
        }
        hooks::digest(&hooks, &allows, &dirs, &commands, mode.as_deref())
    }

    fn all_rule_rows(&self) -> Vec<RuleEntry> {
        self.rule_rows(false)
    }

    fn rule_rows(&self, in_force: bool) -> Vec<RuleEntry> {
        let mut out = Vec::new();
        for l in &self.layers {
            let p = &l.file.permissions;
            for (action, list) in [(RuleAction::Deny, &p.deny), (RuleAction::Ask, &p.ask), (RuleAction::Allow, &p.allow)] {
                for pattern in list {
                    if in_force && action == RuleAction::Allow && !self.applies(l) && !self.granted.contains(pattern) {
                        continue;
                    }
                    out.push(RuleEntry { action, pattern: pattern.clone(), source: l.source, path: l.path.clone() });
                }
            }
        }
        out
    }

    /// The rules in force: every deny and ask, and the allows of layers that apply.
    pub fn rules(&self) -> Vec<RuleEntry> {
        self.rule_rows(true)
    }

    /// The engine's rule set for [`Settings::rules`].
    pub fn rule_set(&self) -> RuleSet {
        crate::rules::rule_set(&self.rules())
    }

    /// The default mode: the highest layer that sets one. A project file may not choose
    /// `bypass` or `dont_ask`, and an untrusted one chooses nothing.
    pub fn default_mode(&self) -> Option<PermissionMode> {
        let mut mode = None;
        for l in &self.layers {
            let Some(m) = l.file.permissions.default_mode.as_deref().and_then(parse_mode) else { continue };
            if l.source.project_scoped() && (!self.trusted || matches!(m, PermissionMode::Bypass | PermissionMode::DontAsk)) {
                continue;
            }
            mode = Some(m);
        }
        mode
    }

    /// Whether new sessions start with Teams on: the highest layer that sets it (a project file
    /// counts only when it applies).
    pub fn teams_default(&self) -> bool {
        self.layers.iter().filter(|l| self.applies(l)).filter_map(|l| l.file.teams.default).last().unwrap_or(false)
    }

    /// The team-size guideline (specialists): the highest layer that sets one, else 10.
    pub fn team_size(&self) -> u32 {
        self.layers.iter().filter(|l| self.applies(l)).filter_map(|l| l.file.teams.size).last().unwrap_or(10).clamp(1, 100)
    }

    /// Hooks that run: from layers that apply, unless a layer that applies disables them all.
    pub fn hooks(&self) -> Vec<HookEntry> {
        if self.layers.iter().any(|l| self.applies(l) && l.file.disable_all_hooks) {
            return Vec::new();
        }
        self.layers.iter().filter(|l| self.applies(l)).flat_map(|l| layer_hooks(l, &mut Vec::new())).collect()
    }

    /// Every configured hook and whether it is trusted (for `/hooks`).
    pub fn hook_rows(&self) -> Vec<(HookEntry, bool)> {
        self.layers
            .iter()
            .flat_map(|l| {
                let t = self.applies(l);
                layer_hooks(l, &mut Vec::new()).into_iter().map(move |h| (h, t))
            })
            .collect()
    }

    /// Hooks of `event` that run.
    pub fn hooks_for(&self, event: HookEvent) -> Vec<HookEntry> {
        self.hooks().into_iter().filter(|h| h.event == event).collect()
    }

    /// Extra workspace roots, absolute (relative ones are under the workspace).
    pub fn additional_directories(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for l in self.layers.iter().filter(|l| self.applies(l)) {
            for d in &l.file.permissions.additional_directories {
                let p = PathBuf::from(d);
                let p = if p.is_absolute() { p } else { self.workspace.as_deref().map_or(p.clone(), |w| w.join(&p)) };
                if !out.contains(&p) {
                    out.push(p);
                }
            }
        }
        out
    }

    /// Rule patterns `accept_edits` frees.
    pub fn project_commands(&self) -> Vec<String> {
        self.collect(|l| &l.file.permissions.project_commands, true)
    }

    /// Paths no command may write or delete (every layer: they only narrow).
    pub fn protected_paths(&self) -> Vec<PathBuf> {
        self.collect(|l| &l.file.permissions.protected_paths, false).into_iter().map(PathBuf::from).collect()
    }

    pub fn protected_branches(&self) -> Vec<String> {
        self.collect(|l| &l.file.permissions.protected_branches, false)
    }

    /// Hosts whose first contact does not ask.
    pub fn allow_hosts(&self) -> Vec<String> {
        self.collect(|l| &l.file.network.allow_hosts, true).into_iter().map(|h| h.to_ascii_lowercase()).collect()
    }

    fn collect(&self, f: impl Fn(&Layer) -> &Vec<String>, widening: bool) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for l in &self.layers {
            if widening && !self.applies(l) {
                continue;
            }
            for v in f(l) {
                if !out.contains(v) {
                    out.push(v.clone());
                }
            }
        }
        out
    }

    /// The layers as the client shows them.
    pub fn layer_views(&self) -> Vec<SettingsLayer> {
        self.layers
            .iter()
            .map(|l| SettingsLayer { scope: l.source.scope(), path: l.path.as_ref().map(|p| p.display().to_string()), text: l.text.clone() })
            .collect()
    }

    /// Writes `rules` with `action` to the settings file of `scope` (not `Session`). An allow
    /// written to a project file is recorded as the user's own grant ([`TrustStore::grant`]), so
    /// it applies after a restart even while the project's other settings are untrusted; a
    /// trusted project stays trusted.
    pub fn grant(home: &Path, workspace: Option<&Path>, scope: SettingsScope, action: RuleAction, rules: &[String]) -> Result<(), crate::SettingsError> {
        let path = Paths::new(home, workspace)
            .for_scope(scope)
            .ok_or_else(|| crate::SettingsError::Invalid("no settings file for that scope (a workspace is needed for project and local)".into()))?;
        let was_trusted = workspace.is_some_and(|w| Settings::load(home, Some(w), None).trusted);
        for r in rules {
            crate::edit::add_rule(&path, action, r)?;
        }
        if let (Some(ws), true) = (workspace, scope != SettingsScope::User) {
            if action == RuleAction::Allow {
                TrustStore::grant(home, ws, rules)?;
            }
            Settings::retrust_after_edit(home, ws, was_trusted)?;
        }
        Ok(())
    }

    /// Removes `pattern` from the settings file of `scope` (and the user's grants); whether it
    /// was there.
    pub fn remove_rule(home: &Path, workspace: Option<&Path>, scope: SettingsScope, pattern: &str) -> Result<bool, crate::SettingsError> {
        let path = Paths::new(home, workspace)
            .for_scope(scope)
            .ok_or_else(|| crate::SettingsError::Invalid("session rules end with the session; there is no file to remove it from".into()))?;
        let removed = crate::edit::remove_rule(&path, pattern)?;
        if let (Some(ws), true) = (workspace, scope != SettingsScope::User) {
            TrustStore::revoke(home, ws, pattern)?;
        }
        Ok(removed)
    }

    /// After the user changed a project file through Marlowe (an "always for this project"
    /// approval, `/permissions add`), trust the new state if the old one was trusted.
    pub fn retrust_after_edit(home: &Path, workspace: &Path, was_trusted: bool) -> std::io::Result<()> {
        if !was_trusted {
            return Ok(());
        }
        let fresh = Settings::load(home, Some(workspace), None);
        if fresh.trusted {
            return Ok(());
        }
        TrustStore::trust(home, workspace, &fresh.digest)
    }
}

/// `accept_edits`, Claude Code's `acceptEdits`, ...
pub fn parse_mode(name: &str) -> Option<PermissionMode> {
    Some(match import::mode_name(name.trim())? {
        "default" => PermissionMode::Default,
        "accept_edits" => PermissionMode::AcceptEdits,
        "plan" => PermissionMode::Plan,
        "bypass" => PermissionMode::Bypass,
        "dont_ask" => PermissionMode::DontAsk,
        _ => return None,
    })
}

/// A layer's hooks; unknown events and malformed hooks go to `errors`.
fn layer_hooks(l: &Layer, errors: &mut Vec<String>) -> Vec<HookEntry> {
    let mut out = Vec::new();
    let place = l.path.as_ref().map_or(l.source.label().to_string(), |p| p.display().to_string());
    for (name, groups) in &l.file.hooks {
        let Some(event) = HookEvent::parse(name) else {
            errors.push(format!("{place}: unknown hook event `{name}`"));
            continue;
        };
        for g in groups {
            for h in &g.hooks {
                let kind = match (h.kind.as_str(), &h.command, &h.prompt) {
                    ("command", Some(c), _) if !c.trim().is_empty() => HookKind::Command(c.clone()),
                    ("prompt", _, Some(p)) if !p.trim().is_empty() => HookKind::Prompt(p.clone()),
                    (k, _, _) => {
                        errors.push(format!("{place}: a `{name}` hook of type `{k}` without its command or prompt"));
                        continue;
                    }
                };
                out.push(HookEntry {
                    event,
                    matcher: g.matcher.clone().filter(|m| !m.trim().is_empty()),
                    kind,
                    timeout: h.timeout.unwrap_or(DEFAULT_TIMEOUT_SECS).max(1),
                    source: l.source,
                    path: l.path.clone(),
                });
            }
        }
    }
    out
}
