//! The system prompt: persona core, capability blocks, user block, environment block.
//!
//! Blocks are appended only when their capability is live; the prompt never leads the harness.

use marlowe_model::SystemBlock;
use marlowe_proto::GitInfo;
use marlowe_tools::ToolGroup;

/// The stable persona core.
pub const PERSONA_CORE: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../persona/v3.md"));

const BLOCK_BRIEFING: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../persona/blocks/briefing.md"));
const BLOCK_MEMORY: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../persona/blocks/memory.md"));
const BLOCK_CREW: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../persona/blocks/crew.md"));
const BLOCK_VOICE: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../persona/blocks/voice.md"));

/// An optional persona capability block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PersonaBlock {
    /// A Wake briefing is in context (M3).
    Briefing,
    /// The `dispatch` tool is exposed (M4).
    Crew,
    /// A memory service is attached: the profile block and the memory tools exist (M2.6).
    Memory,
    /// The reply will be spoken (M3+).
    Voice,
    /// A tool group's block (`persona/blocks/<group>.md`), declared beside the group's tools in
    /// `marlowe-exec/src/specs/<group>.rs` and handed over by [`crate::ToolHost::persona_blocks`]
    /// only while the group is exposed.
    Tool { group: ToolGroup, text: &'static str },
}

impl PersonaBlock {
    pub fn text(self) -> &'static str {
        match self {
            PersonaBlock::Briefing => BLOCK_BRIEFING,
            PersonaBlock::Crew => BLOCK_CREW,
            PersonaBlock::Memory => BLOCK_MEMORY,
            PersonaBlock::Voice => BLOCK_VOICE,
            PersonaBlock::Tool { text, .. } => text,
        }
    }
}

/// Who the user is, from the profile at runtime.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserInfo {
    pub name: Option<String>,
    /// Formal address for greetings and alerts, e.g. `sir`.
    pub address: Option<String>,
}

/// Facts about the current call's environment. Rebuilt every turn.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Environment {
    /// Local date and time, human-readable.
    pub now: String,
    pub os: String,
    pub workspace: Option<String>,
    pub model: String,
    /// The workspace's git state (branch, dirty count, last commit), when it is a repository.
    pub repo: Option<GitInfo>,
    /// One-line facts about the tools the prompt's tail should carry ([`crate::ToolHost::prompt_hints`]:
    /// the MCP hint).
    pub hints: Vec<String>,
}

impl Environment {
    /// The environment as of now, on this machine. `repo` is left empty; the caller sets it.
    pub fn current(workspace: Option<String>, model: impl Into<String>) -> Self {
        let now = chrono::Local::now().format("%A %-d %B %Y, %H:%M (UTC%:z)").to_string();
        Self { now, os: os_name(), workspace, model: model.into(), repo: None, hints: Vec::new() }
    }
}

/// `main · 3 uncommitted · last commit 1a2b3c4 "fix the gate"`.
fn repo_line(g: &GitInfo) -> String {
    let mut t = if g.branch.is_empty() { "detached HEAD".to_string() } else { g.branch.clone() };
    t.push_str(&match g.dirty {
        0 => " · clean".to_string(),
        1 => " · 1 uncommitted change".to_string(),
        n => format!(" · {n} uncommitted changes"),
    });
    match (&g.head, &g.subject) {
        (Some(h), Some(s)) => t.push_str(&format!(" · last commit {h} \"{}\"", s.trim())),
        (None, Some(s)) => t.push_str(&format!(" · last commit \"{}\"", s.trim())),
        (Some(h), None) => t.push_str(&format!(" · last commit {h}")),
        (None, None) => {}
    }
    t
}

fn os_name() -> String {
    let os = match std::env::consts::OS {
        "windows" => "Windows",
        "macos" => "macOS",
        "linux" => "Linux",
        other => other,
    };
    format!("{os} ({})", std::env::consts::ARCH)
}

/// The project context that sits in the stable prefix after the persona (design 3.4 order:
/// persona -> blocks -> [tools] -> instruction files -> project profile -> repo map -> skills
/// index -> output style). Built per session (track F's instructions middleware keeps it in
/// `Conversation::ext`), fitted to the prefix budget per model ([`crate::context::fit_prefix`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PromptSections {
    /// `<instructions>` block (MARLOWE.md / AGENTS.md / CLAUDE.md, imports, always-on rules).
    pub instructions: String,
    /// The project profile (verify track's build/test commands), when known.
    pub project_profile: String,
    /// The ranked repo map (code track), when built.
    pub repo_map: String,
    /// Names and descriptions of model-invocable skills.
    pub skills_index: String,
    /// The active output style, appended last in the stable prefix.
    pub output_style: String,
    /// Loading problems and budget truncations, surfaced in `/context` and at session start.
    pub warnings: Vec<String>,
}

impl PromptSections {
    pub fn is_empty(&self) -> bool {
        self.instructions.trim().is_empty()
            && self.project_profile.trim().is_empty()
            && self.repo_map.trim().is_empty()
            && self.skills_index.trim().is_empty()
            && self.output_style.trim().is_empty()
    }
}

/// Builds the system blocks for a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemPrompt {
    core: String,
    blocks: Vec<PersonaBlock>,
    user: Option<UserInfo>,
    env: Option<Environment>,
    sections: PromptSections,
    /// `--append-system-prompt` (H4): last in the stable tier.
    appended: Option<String>,
}

impl Default for SystemPrompt {
    fn default() -> Self {
        Self::new()
    }
}

impl SystemPrompt {
    /// The v3 persona core with nothing appended.
    pub fn new() -> Self {
        Self::with_core(PERSONA_CORE)
    }

    /// A custom core (tests, crew role cards).
    pub fn with_core(core: impl Into<String>) -> Self {
        Self { core: core.into(), blocks: Vec::new(), user: None, env: None, sections: PromptSections::default(), appended: None }
    }

    /// Appends a capability block (once).
    pub fn block(mut self, b: PersonaBlock) -> Self {
        if !self.blocks.contains(&b) {
            self.blocks.push(b);
        }
        self
    }

    /// Appends each of `blocks` (once), e.g. a tool host's [`crate::ToolHost::persona_blocks`].
    pub fn blocks_from(mut self, blocks: impl IntoIterator<Item = PersonaBlock>) -> Self {
        for b in blocks {
            self = self.block(b);
        }
        self
    }

    pub fn user(mut self, u: UserInfo) -> Self {
        self.user = Some(u);
        self
    }

    pub fn environment(mut self, e: Environment) -> Self {
        self.env = Some(e);
        self
    }

    /// Replaces the environment in place (per turn).
    pub fn set_environment(&mut self, e: Environment) {
        self.env = Some(e);
    }

    /// The environment set on this prompt, if any.
    pub fn env(&self) -> Option<&Environment> {
        self.env.as_ref()
    }

    pub fn blocks(&self) -> &[PersonaBlock] {
        &self.blocks
    }

    /// Sets the project sections (already fitted to the prefix budget).
    pub fn sections(mut self, s: PromptSections) -> Self {
        self.sections = s;
        self
    }

    /// Replaces the project sections in place.
    pub fn set_sections(&mut self, s: PromptSections) {
        self.sections = s;
    }

    pub fn current_sections(&self) -> &PromptSections {
        &self.sections
    }

    /// The core and capability blocks as one text (what the persona budget counts).
    pub fn persona_text(&self) -> String {
        let mut stable = self.core.trim_end().to_string();
        for b in &self.blocks {
            stable.push_str("\n\n");
            stable.push_str(b.text().trim_end());
        }
        stable
    }

    /// Renders to system blocks: [core + capability blocks] (cached), project sections, [user]
    /// (cached), output style (cached), [environment] (not cached, last: it changes every turn).
    pub fn render(&self) -> Vec<SystemBlock> {
        let mut out = self.render_stable();
        out.extend(self.render_environment());
        out
    }

    /// The stable tier: core + capability blocks, the project sections (instructions, profile,
    /// repo map, skills index), the user block if non-empty, then the output style.
    pub(crate) fn render_stable(&self) -> Vec<SystemBlock> {
        let mut out = vec![SystemBlock { text: self.persona_text(), cache: true }];
        let s = &self.sections;
        for text in [&s.instructions, &s.project_profile, &s.repo_map, &s.skills_index] {
            if !text.trim().is_empty() {
                out.push(SystemBlock { text: text.trim().to_string(), cache: false });
            }
        }

        if let Some(u) = &self.user {
            let mut t = String::from("<user>\n");
            if let Some(n) = u.name.as_deref().filter(|s| !s.trim().is_empty()) {
                t.push_str(&format!("Name: {}\n", n.trim()));
            }
            if let Some(a) = u.address.as_deref().filter(|s| !s.trim().is_empty()) {
                t.push_str(&format!("Formal address: {}\n", a.trim()));
            }
            t.push_str("</user>");
            if t != "<user>\n</user>" {
                out.push(SystemBlock { text: t, cache: true });
            }
        }
        if !s.output_style.trim().is_empty() {
            out.push(SystemBlock { text: s.output_style.trim().to_string(), cache: true });
        }
        if let Some(a) = &self.appended {
            out.push(SystemBlock { text: a.trim().to_string(), cache: true });
        }
        out
    }

    /// Appends `text` after everything else in the stable tier (`marlowe -p
    /// --append-system-prompt`).
    pub fn append(mut self, text: impl Into<String>) -> Self {
        let text = text.into();
        self.appended = (!text.trim().is_empty()).then_some(text);
        self
    }

    /// The volatile environment block, if set. Never cached.
    pub(crate) fn render_environment(&self) -> Option<SystemBlock> {
        let e = self.env.as_ref()?;
        let mut t = String::from("<environment>\n");
        t.push_str(&format!("Now: {}\n", e.now));
        t.push_str(&format!("OS: {}\n", e.os));
        if let Some(w) = &e.workspace {
            t.push_str(&format!("Workspace: {w}\n"));
        }
        if let Some(g) = &e.repo {
            t.push_str(&format!("Repo: {}\n", repo_line(g)));
        }
        if !e.model.is_empty() {
            t.push_str(&format!("Model: {}\n", e.model));
        }
        for hint in &e.hints {
            t.push_str(hint.trim());
            t.push('\n');
        }
        t.push_str("</environment>");
        Some(SystemBlock { text: t, cache: false })
    }

    /// All blocks joined, for providers with a single system string.
    pub fn render_text(&self) -> String {
        self.render().into_iter().map(|b| b.text).collect::<Vec<_>>().join("\n\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_core_is_the_v3_persona_and_names_no_operator() {
        assert!(PERSONA_CORE.contains("You are Marlowe."));
        assert!(!PERSONA_CORE.contains("Matthew"), "the artifact never names its operator");
        let r = SystemPrompt::new().render();
        assert_eq!(r.len(), 1);
        assert!(r[0].cache);
        assert!(r[0].text.starts_with("<identity>"));
    }

    #[test]
    fn words_to_avoid_lists_the_transparency_qualifiers() {
        let (_, rest) = PERSONA_CORE.split_once("<words_to_avoid>").unwrap();
        let (section, _) = rest.split_once("</words_to_avoid>").unwrap();
        assert!(section.contains("just to be transparent"), "{section}");
        assert!(section.contains(", to be transparent,"), "{section}");
    }

    #[test]
    fn capability_blocks_appear_only_when_added() {
        let plain = SystemPrompt::new().render_text();
        assert!(!plain.contains("<voice_channel>"));
        assert!(!plain.contains("<crew>"));
        let voiced = SystemPrompt::new().block(PersonaBlock::Voice).block(PersonaBlock::Voice).render_text();
        assert_eq!(voiced.matches("<voice_channel>").count(), 1);
    }

    #[test]
    fn the_memory_block_describes_the_tools_and_names_no_operator() {
        assert!(!SystemPrompt::new().render_text().contains("<memory_tools>"));
        let t = SystemPrompt::new().block(PersonaBlock::Memory).render_text();
        for want in ["<memory_tools>", "<profile>", "`recall`"] {
            assert!(t.contains(want), "{want}");
        }
        assert!(!PersonaBlock::Memory.text().contains("Matthew"));
    }

    #[test]
    fn user_and_environment_blocks_render_in_order() {
        let p = SystemPrompt::with_core("core")
            .user(UserInfo { name: Some("Matthew".into()), address: Some("sir".into()) })
            .environment(Environment {
                now: "Thursday 9 October 2026, 08:14".into(),
                os: "Windows (x86_64)".into(),
                workspace: Some("C:/work".into()),
                model: "ollama/qwen3:8b".into(),
                repo: Some(GitInfo {
                    branch: "main".into(),
                    dirty: 3,
                    head: Some("1a2b3c4".into()),
                    subject: Some("gate refactor".into()),
                    ..Default::default()
                }),
                hints: vec!["Tools from MCP servers (mcp__server__tool): call one by name.".into()],
            });
        let r = p.render();
        assert_eq!(r.len(), 3);
        assert_eq!(r[0].text, "core");
        assert!(r[1].text.contains("Name: Matthew") && r[1].text.contains("Formal address: sir"));
        assert!(r[1].cache);
        assert!(r[2].text.contains("Workspace: C:/work"));
        assert!(r[2].text.contains("Repo: main · 3 uncommitted changes · last commit 1a2b3c4 \"gate refactor\""), "{}", r[2].text);
        assert!(r[2].text.contains("Tools from MCP servers (mcp__server__tool)"), "{}", r[2].text);
        assert!(!r[2].cache, "the environment changes every turn and is never cached");
    }

    #[test]
    fn an_empty_user_block_is_omitted() {
        let r = SystemPrompt::with_core("core").user(UserInfo::default()).render();
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn the_current_environment_is_filled() {
        let e = Environment::current(None, "m");
        assert!(!e.now.is_empty());
        assert!(!e.os.is_empty());
    }
}
