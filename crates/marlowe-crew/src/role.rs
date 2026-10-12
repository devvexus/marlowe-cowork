//! Role cards: markdown with a small YAML-ish frontmatter, then the role's instructions.
//!
//! ```text
//! ---
//! name: scout
//! description: One line.
//! slot: crew:scout            # optional; default crew:<name>, falling back to conductor
//! effort: low                 # off | low | medium | high | max
//! tools: [web_search, read]   # allowlist
//! budget: {tokens: 150000, steps: 25, usd: 0.50}
//! sandbox: none               # worktree | scratch | none
//! ---
//! Instructions...
//! ```
//!
//! Built-ins ship in `roles\*.md`; `<home>\roles\*.md` adds roles or overrides one by name.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use marlowe_model::Effort;
use marlowe_proto::{CrewBudget, RoleInfo, SandboxKind, Slot};

/// The shipped cards, `(file stem, text)`.
pub const BUILTIN_ROLES: [(&str, &str); 8] = [
    ("researcher", include_str!("../../../roles/researcher.md")),
    ("engineer", include_str!("../../../roles/engineer.md")),
    ("reviewer", include_str!("../../../roles/reviewer.md")),
    ("analyst", include_str!("../../../roles/analyst.md")),
    ("scout", include_str!("../../../roles/scout.md")),
    ("compressor", include_str!("../../../roles/compressor.md")),
    ("verifier", include_str!("../../../roles/verifier.md")),
    // A workflow script's agents (M4.6 W).
    ("workflow", include_str!("../../../roles/workflow.md")),
];

/// Where a card came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoleSource {
    Builtin,
    /// A file under `<home>\roles`.
    User(PathBuf),
}

/// A parsed role card.
#[derive(Debug, Clone, PartialEq)]
pub struct RoleCard {
    /// Lower-case `[a-z0-9_-]+`.
    pub name: String,
    pub description: String,
    /// The slot to resolve first; `crew:<name>` unless the card says otherwise.
    pub slot: Slot,
    pub effort: Effort,
    /// Tool allowlist. `dispatch` is never honoured here: runs are depth 1.
    pub tools: Vec<String>,
    pub budget: CrewBudget,
    pub sandbox: SandboxKind,
    /// The markdown body: the role's system instructions.
    pub instructions: String,
    pub source: RoleSource,
}

/// A card that does not parse.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RoleError {
    #[error("no frontmatter: the card must start with a `---` line and close it with another")]
    NoFrontmatter,
    #[error("missing `{0}`")]
    Missing(&'static str),
    #[error("unknown field `{0}`")]
    UnknownField(String),
    #[error("bad `{field}`: {value}")]
    BadField { field: &'static str, value: String },
    #[error("the card has no instructions after its frontmatter")]
    NoInstructions,
    #[error("could not read {path}: {message}")]
    Unreadable { path: String, message: String },
}

impl RoleCard {
    /// The wire view.
    pub fn info(&self) -> RoleInfo {
        RoleInfo {
            name: self.name.clone(),
            description: self.description.clone(),
            slot: self.slot.clone(),
            effort: self.effort,
            tools: self.tools.clone(),
            budget: self.budget,
            sandbox: self.sandbox,
            user_file: match &self.source {
                RoleSource::Builtin => None,
                RoleSource::User(p) => Some(p.display().to_string()),
            },
        }
    }

    /// Slots to try in order: the card's slot, then the conductor's.
    pub fn slot_chain(&self) -> Vec<Slot> {
        let mut v = vec![self.slot.clone()];
        if self.slot != Slot::Conductor {
            v.push(Slot::Conductor);
        }
        v
    }

    /// Whether the allowlist names `tool`. `dispatch` is always refused.
    pub fn allows(&self, tool: &str) -> bool {
        tool != crate::DISPATCH_TOOL && self.tools.iter().any(|t| t == tool)
    }
}

/// Parses one card.
pub fn parse_role(text: &str, source: RoleSource) -> Result<RoleCard, RoleError> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.lines();
    if lines.next().map(str::trim) != Some("---") {
        return Err(RoleError::NoFrontmatter);
    }
    let mut fields: BTreeMap<&'static str, String> = BTreeMap::new();
    let mut closed = false;
    let mut consumed = 0usize;
    for line in lines.by_ref() {
        consumed += 1;
        let t = line.trim();
        if t == "---" {
            closed = true;
            break;
        }
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        let Some((k, v)) = t.split_once(':') else {
            return Err(RoleError::UnknownField(t.to_string()));
        };
        let key = match k.trim().to_ascii_lowercase().as_str() {
            "name" => "name",
            "description" => "description",
            "slot" => "slot",
            "effort" => "effort",
            "tools" => "tools",
            "budget" => "budget",
            "sandbox" => "sandbox",
            other => return Err(RoleError::UnknownField(other.to_string())),
        };
        let v = if key == "description" { v } else { strip_comment(v) };
        fields.insert(key, v.trim().to_string());
    }
    if !closed {
        return Err(RoleError::NoFrontmatter);
    }
    let body: String = text.lines().skip(consumed + 1).collect::<Vec<_>>().join("\n");
    let instructions = body.trim().to_string();
    if instructions.is_empty() {
        return Err(RoleError::NoInstructions);
    }

    let name = fields.remove("name").ok_or(RoleError::Missing("name"))?.to_ascii_lowercase();
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return Err(RoleError::BadField { field: "name", value: name });
    }
    let description = fields.remove("description").ok_or(RoleError::Missing("description"))?;
    let slot = match fields.remove("slot") {
        None => Slot::Crew(name.clone()),
        Some(s) => s.parse().map_err(|_| RoleError::BadField { field: "slot", value: s })?,
    };
    let effort = match fields.remove("effort") {
        None => Effort::default(),
        Some(s) => Effort::ALL
            .into_iter()
            .find(|e| e.as_str().eq_ignore_ascii_case(&s))
            .ok_or(RoleError::BadField { field: "effort", value: s })?,
    };
    let tools = match fields.remove("tools") {
        None => Vec::new(),
        Some(s) => parse_list(&s).ok_or(RoleError::BadField { field: "tools", value: s })?,
    };
    let budget = match fields.remove("budget") {
        None => CrewBudget::default(),
        Some(s) => parse_budget(&s).ok_or(RoleError::BadField { field: "budget", value: s })?,
    };
    let sandbox = match fields.remove("sandbox").as_deref().map(str::to_ascii_lowercase).as_deref() {
        None | Some("scratch") => SandboxKind::Scratch,
        Some("worktree") => SandboxKind::Worktree,
        Some("none") => SandboxKind::None,
        Some(other) => return Err(RoleError::BadField { field: "sandbox", value: other.to_string() }),
    };
    Ok(RoleCard { name, description, slot, effort, tools, budget, sandbox, instructions, source })
}

/// Drops a trailing ` # comment`.
fn strip_comment(v: &str) -> &str {
    match v.find(" #") {
        Some(i) => &v[..i],
        None => v,
    }
}

/// `[a, b, c]` (or a bare `a, b`).
fn parse_list(s: &str) -> Option<Vec<String>> {
    let inner = s.trim();
    let inner = match (inner.strip_prefix('['), inner.ends_with(']')) {
        (Some(rest), true) => &rest[..rest.len() - 1],
        (None, false) => inner,
        _ => return None,
    };
    let items: Vec<String> = inner
        .split(',')
        .map(|t| t.trim().trim_matches(|c| c == '"' || c == '\'').to_string())
        .filter(|t| !t.is_empty())
        .collect();
    items.iter().all(|t| t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')).then_some(items)
}

/// `{tokens: 150000, steps: 25, usd: 0.50}`; any subset, `_` allowed in numbers.
fn parse_budget(s: &str) -> Option<CrewBudget> {
    let inner = s.trim().strip_prefix('{')?.strip_suffix('}')?;
    let mut b = CrewBudget::default();
    for part in inner.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let (k, v) = part.split_once(':')?;
        let v = v.trim().replace('_', "");
        match k.trim() {
            "tokens" => b.tokens = Some(v.parse().ok()?),
            "steps" => b.steps = Some(v.parse().ok()?),
            "usd" => b.usd = Some(v.trim_start_matches('$').parse().ok().filter(|u: &f64| u.is_finite() && *u >= 0.0)?),
            _ => return None,
        }
    }
    Some(b)
}

/// The role cards in force: built-ins, then `<home>\roles\*.md` (same name overrides).
#[derive(Debug, Clone, Default)]
pub struct Roles {
    cards: BTreeMap<String, RoleCard>,
    problems: Vec<(PathBuf, RoleError)>,
}

impl Roles {
    /// The shipped cards only.
    pub fn builtin() -> Roles {
        let mut roles = Roles::default();
        for (stem, text) in BUILTIN_ROLES {
            let card = parse_role(text, RoleSource::Builtin).unwrap_or_else(|e| panic!("built-in role {stem}: {e}"));
            roles.cards.insert(card.name.clone(), card);
        }
        roles
    }

    /// Built-ins plus the user's cards. A user card that does not parse is skipped and kept in
    /// [`Roles::problems`]; it never removes the built-in of the same name.
    pub fn load(home: &Path) -> Roles {
        let mut roles = Roles::builtin();
        let dir = home.join("roles");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return roles;
        };
        let mut paths: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("md")))
            .collect();
        paths.sort();
        for path in paths {
            let parsed = std::fs::read_to_string(&path)
                .map_err(|e| RoleError::Unreadable { path: path.display().to_string(), message: e.to_string() })
                .and_then(|text| parse_role(&text, RoleSource::User(path.clone())));
            match parsed {
                Ok(card) => {
                    roles.cards.insert(card.name.clone(), card);
                }
                Err(e) => roles.problems.push((path, e)),
            }
        }
        roles
    }

    /// The card named `name` (case-insensitive).
    pub fn get(&self, name: &str) -> Option<&RoleCard> {
        self.cards.get(&name.trim().to_ascii_lowercase())
    }

    /// Every card, by name.
    pub fn cards(&self) -> impl Iterator<Item = &RoleCard> {
        self.cards.values()
    }

    /// Every card's wire view, by name.
    pub fn infos(&self) -> Vec<RoleInfo> {
        self.cards.values().map(RoleCard::info).collect()
    }

    /// User cards that were skipped, and why.
    pub fn problems(&self) -> &[(PathBuf, RoleError)] {
        &self.problems
    }
}
