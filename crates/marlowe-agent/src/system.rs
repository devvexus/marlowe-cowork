//! The system prompt: persona core, capability blocks, user block, environment block.
//!
//! Blocks are appended only when their capability is live; the prompt never leads the harness.

use marlowe_model::SystemBlock;

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
}

impl PersonaBlock {
    pub fn text(self) -> &'static str {
        match self {
            PersonaBlock::Briefing => BLOCK_BRIEFING,
            PersonaBlock::Crew => BLOCK_CREW,
            PersonaBlock::Memory => BLOCK_MEMORY,
            PersonaBlock::Voice => BLOCK_VOICE,
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
}

impl Environment {
    /// The environment as of now, on this machine.
    pub fn current(workspace: Option<String>, model: impl Into<String>) -> Self {
        let now = chrono::Local::now().format("%A %-d %B %Y, %H:%M (UTC%:z)").to_string();
        Self { now, os: os_name(), workspace, model: model.into() }
    }
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

/// Builds the system blocks for a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemPrompt {
    core: String,
    blocks: Vec<PersonaBlock>,
    user: Option<UserInfo>,
    env: Option<Environment>,
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
        Self { core: core.into(), blocks: Vec::new(), user: None, env: None }
    }

    /// Appends a capability block (once).
    pub fn block(mut self, b: PersonaBlock) -> Self {
        if !self.blocks.contains(&b) {
            self.blocks.push(b);
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

    pub fn blocks(&self) -> &[PersonaBlock] {
        &self.blocks
    }

    /// Renders to system blocks: [core + capability blocks] (cached), [user] (cached),
    /// [environment] (not cached, last, since it changes every turn).
    pub fn render(&self) -> Vec<SystemBlock> {
        let mut out = self.render_stable();
        out.extend(self.render_environment());
        out
    }

    /// The stable tier: core + capability blocks, then the user block if non-empty.
    pub(crate) fn render_stable(&self) -> Vec<SystemBlock> {
        let mut out = Vec::new();
        let mut stable = self.core.trim_end().to_string();
        for b in &self.blocks {
            stable.push_str("

");
            stable.push_str(b.text().trim_end());
        }
        out.push(SystemBlock { text: stable, cache: true });

        if let Some(u) = &self.user {
            let mut t = String::from("<user>
");
            if let Some(n) = u.name.as_deref().filter(|s| !s.trim().is_empty()) {
                t.push_str(&format!("Name: {}
", n.trim()));
            }
            if let Some(a) = u.address.as_deref().filter(|s| !s.trim().is_empty()) {
                t.push_str(&format!("Formal address: {}
", a.trim()));
            }
            t.push_str("</user>");
            if t != "<user>
</user>" {
                out.push(SystemBlock { text: t, cache: true });
            }
        }
        out
    }

    /// The volatile environment block, if set. Never cached.
    pub(crate) fn render_environment(&self) -> Option<SystemBlock> {
        let e = self.env.as_ref()?;
        let mut t = String::from("<environment>
");
        t.push_str(&format!("Now: {}
", e.now));
        t.push_str(&format!("OS: {}
", e.os));
        if let Some(w) = &e.workspace {
            t.push_str(&format!("Workspace: {w}
"));
        }
        if !e.model.is_empty() {
            t.push_str(&format!("Model: {}
", e.model));
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
        for want in ["<memory_tools>", "<profile>", "`remember`", "`recall`", "`correct`", "use uv, not pip"] {
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
            });
        let r = p.render();
        assert_eq!(r.len(), 3);
        assert_eq!(r[0].text, "core");
        assert!(r[1].text.contains("Name: Matthew") && r[1].text.contains("Formal address: sir"));
        assert!(r[1].cache);
        assert!(r[2].text.contains("Workspace: C:/work"));
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
