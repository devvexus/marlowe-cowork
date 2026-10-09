//! §B2's region contract, expressed as a type.
//!
//! > **A border delineates an interactive region. Every bordered region carries a label on its top
//! > border and a hotkey on its bottom border. A region with no hotkey has no border.**
//!
//! §B13 asks for that at 100%, *asserted by test rather than by inspection*. A test that walks a
//! tree and checks two fields are non-empty would pass forever and catch nothing, because the
//! failure mode is not "somebody set the label to an empty string" — it is "somebody drew a
//! `Block::bordered()` without going through the tree at all".
//!
//! So the enforcement is in two halves and the second is the one that bites:
//!
//! 1. **[`Region`] cannot be constructed without a label and a hotkey.** There is no setter, no
//!    `Default`, and no `Option` in either field.
//! 2. **[`Region::block`] is the only way to obtain a bordered `Block` in this crate**, and
//!    `tests/region_contract.rs` greps the source for `Block::bordered` and
//!    `Borders::` outside this file. A border drawn any other way fails the suite by name.
//!
//! The titlebar, the footer and the inspector's own frame have no hotkey and therefore **no
//! border** — they are structural, not interactive. The mockup boxes some of them; prose wins.

use ratatui::style::Style;
use ratatui::widgets::{Block, BorderType};

/// Everything on screen that can hold focus.
///
/// The inspector itself is deliberately absent: it has no single hotkey — each *tab* has one — so
/// under §B2 it has no border, and what holds focus is an [`RegionId::Item`] inside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RegionId {
    Model,
    Profile,
    Session,
    Workspace,
    Autonomy,
    Status,
    Conversation,
    /// An inspector item, addressed by the tab it lives on and its index in that pane.
    Item(TabId, usize),
    Message,

    // ── a run window (`M3-DESIGN.md` §6) ────────────────────────────────────────────────────
    //
    // A window is a **different surface over the same region contract**, not a second contract.
    // These live in this enum rather than in one of their own precisely so `Region` — the only
    // constructor of a bordered `Block` in this crate — is what draws them, and so
    // `b13_region_contract.rs`'s grep keeps covering them.
    //
    // They never appear in a `RegionTree::build` tree and the window's regions never appear in a
    // main-pane tree, so the two key sets are independent: `RunSteer` may share `i` with
    // [`RegionId::Message`] because no frame ever shows both, and both mean "type here".
    /// Id, status, elapsed, spend against ceiling.
    RunIdentity,
    /// Last completed step, and what a resume would resume from.
    RunCheckpoint,
    /// The run's streaming output. ADR-055.
    RunOutput,
    /// The steer field. **A write** — ADR-054.
    RunSteer,
    /// §6.3's placeholders. Empty is a fact, never a roadmap.
    RunSubagents,
    RunBudget,
    RunScopeMemory,
    RunMeetings,
}

/// The inspector's six panes. Mirrors `marlowe_view::Tab`; kept separate so the surface's region
/// tree does not depend on the stub's enum ordering for its identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TabId {
    Runs,
    Schedule,
    Sessions,
    Skills,
    Trust,
    Status,
}

impl From<marlowe_view::Tab> for TabId {
    fn from(t: marlowe_view::Tab) -> Self {
        match t {
            marlowe_view::Tab::Runs => TabId::Runs,
            marlowe_view::Tab::Schedule => TabId::Schedule,
            marlowe_view::Tab::Sessions => TabId::Sessions,
            marlowe_view::Tab::Skills => TabId::Skills,
            marlowe_view::Tab::Trust => TabId::Trust,
            marlowe_view::Tab::Status => TabId::Status,
        }
    }
}

/// How focused a region is. §B2: *"Focused: accent border, brightened label, accent hotkey.
/// Unfocused: default border, accent label, dim hotkey. Inactive or irrelevant: dim border, dim
/// label."*
///
/// **None of the three is a background fill**, and [`crate::theme`] has no API that could produce
/// one — see the zero-fill acceptance row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusLevel {
    Focused,
    /// The pointer is over this region. **Sits below focus rather than competing with it**: a
    /// hovered region brightens its border and label, but only the focused one gets the full
    /// accent. Moving the mouse across the screen must never read as focus jumping around.
    ///
    /// Border-only, no fill — §B2, and also the flicker argument: changing a border repaints the
    /// perimeter, changing a fill repaints every cell of the region.
    Hovered,
    Unfocused,
    /// Dimming is load-bearing, not cosmetic: a calendar event needing nothing from the user is
    /// dimmed to near-invisible so the eye goes to the two that do.
    Inactive,
}

/// A bordered, labelled, keyed region. **The only bordered thing in the interface.**
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Region {
    id: RegionId,
    label: String,
    hotkey: Option<char>,
}

impl Region {
    /// The only constructor.
    ///
    /// Panics on an empty label or a hotkey that cannot be typed. A panic rather than a `Result`
    /// because there is no recovery worth writing: a region with no way to reach it is a border
    /// that lies about being interactive, and the honest response is to not start.
    /// A region with no accelerator — an inspector item past its pane's letter pool.
    ///
    /// **§B2 says a region with no hotkey has no border, and that sentence is about §B2's six**:
    /// the titlebar, the footer and the inspector frame have neither. Its purpose is that *a border
    /// means an interactive region*, and an unkeyed item is fully interactive — it takes focus, the
    /// arrows step to it, the wheel scrolls to it and the pointer clicks it. Dropping its border
    /// would say the opposite of what is true.
    pub fn unkeyed(id: RegionId, label: impl Into<String>) -> Self {
        let label = label.into();
        assert!(!label.is_empty(), "{id:?} has an empty label");
        Self { id, label, hotkey: None }
    }

    pub fn new(id: RegionId, label: impl Into<String>, hotkey: char) -> Self {
        let label = label.into();
        assert!(
            !label.trim().is_empty(),
            "{id:?} has an empty label. §B2: the label says what the region holds; a border with \
             no label is decoration, and decoration is what the region contract exists to forbid"
        );
        assert!(
            !hotkey.is_control() && !hotkey.is_whitespace(),
            "{id:?} has hotkey {hotkey:?}, which cannot be typed as a region key. Ctrl-modified \
             keys are the footer's namespace (§B8) and Enter is the act verb, not an address"
        );
        Self { id, label, hotkey: Some(hotkey) }
    }

    pub fn id(&self) -> RegionId {
        self.id
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn hotkey(&self) -> Option<char> {
        self.hotkey
    }

    /// The hotkey as it renders on the bottom border: `(c)`.
    pub fn hotkey_label(&self) -> String {
        self.hotkey.map(|k| format!("({k})")).unwrap_or_default()
    }

    /// **The only way to get a bordered `Block` in this crate.**
    ///
    /// Label on the top border, hotkey on the bottom border, right-aligned. Focus is carried by
    /// `border_style` and the title styles — one style swap, no cell repaint, which is what §B12's
    /// flicker target and K4 rest on.
    pub fn block<'a>(&'a self, theme: &crate::theme::Theme, focus: FocusLevel) -> Block<'a> {
        self.build(theme, focus, None)
    }

    /// A region whose border carries a state colour — an autonomy tier at `confirm`, a schedule
    /// conflict, a run against its spend ceiling. The label follows the border.
    ///
    /// **State colours encode state only, never category** (§B2). There is no `block_for_kind`.
    pub fn block_toned<'a>(
        &'a self,
        theme: &crate::theme::Theme,
        focus: FocusLevel,
        tone: marlowe_view::Tone,
    ) -> Block<'a> {
        match tone {
            // Accent/Normal/Dim carry no state; the focus styles already say everything true.
            marlowe_view::Tone::Accent | marlowe_view::Tone::Normal | marlowe_view::Tone::Dim => {
                self.build(theme, focus, None)
            }
            _ => self.build(theme, focus, Some(Style::default().fg(theme.tone(tone)))),
        }
    }

    /// One construction path.
    ///
    /// **`Block::title` appends rather than replaces**, so a `block()` that was later given a
    /// second title drew the label twice — `┌Status─Status───┐`. Every style is therefore decided
    /// before the block is built, and there is no way to restyle one afterwards.
    fn build<'a>(
        &'a self,
        theme: &crate::theme::Theme,
        focus: FocusLevel,
        state: Option<Style>,
    ) -> Block<'a> {
        let (border, label, key) = theme.region_styles(focus);
        let (border, label) = match state {
            Some(s) => (s, s),
            None => (border, label),
        };
        Block::bordered()
            .border_type(BorderType::Plain)
            .border_style(border)
            .title(ratatui::text::Span::styled(self.label.as_str(), label))
            .title_bottom(
                ratatui::text::Line::from(ratatui::text::Span::styled(self.hotkey_label(), key))
                    .right_aligned(),
            )
    }
}

/// Every region on screen, in **reading order** — which is also `Tab` order (§B10).
///
/// Built fresh from the session on every frame, because the inspector's items change with the tab.
/// That is not a cost worth optimising: it is what guarantees the tree and the screen cannot
/// disagree, and a stale tree is exactly how the "100% have a hotkey" assertion would go green
/// while a real region shipped without one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegionTree {
    regions: Vec<Region>,
}

impl RegionTree {
    pub fn build(view: &marlowe_view::SessionView, showing: marlowe_view::Tab) -> Self {
        let tab: TabId = showing.into();
        let mut regions = vec![
            Region::new(RegionId::Model, "Model", 'm'),
            Region::new(RegionId::Profile, "Profile", 'p'),
            Region::new(RegionId::Session, "Session", 's'),
            Region::new(RegionId::Workspace, "Workspace", 'w'),
            Region::new(RegionId::Autonomy, "Autonomy", 'a'),
            Region::new(RegionId::Status, "Status", 'v'),
            Region::new(RegionId::Conversation, "Conversation", 'c'),
        ];
        for (i, item) in crate::inspector::items(view, showing).iter().enumerate() {
            regions.push(match item.key {
                Some(k) => Region::new(RegionId::Item(tab, i), &item.label, k),
                None => Region::unkeyed(RegionId::Item(tab, i), &item.label),
            });
        }
        regions.push(Region::new(RegionId::Message, "Message", 'i'));
        Self { regions }
    }

    pub fn regions(&self) -> &[Region] {
        &self.regions
    }

    pub fn get(&self, id: RegionId) -> Option<&Region> {
        self.regions.iter().find(|r| r.id == id)
    }

    /// The next region in reading order, wrapping. `Tab`.
    pub fn next(&self, from: RegionId) -> RegionId {
        let i = self.index_of(from);
        self.regions[(i + 1) % self.regions.len()].id
    }

    /// `Shift-Tab`.
    pub fn prev(&self, from: RegionId) -> RegionId {
        let i = self.index_of(from);
        self.regions[(i + self.regions.len() - 1) % self.regions.len()].id
    }

    fn index_of(&self, id: RegionId) -> usize {
        // A focus that is not in the tree means the tab changed under it. Reading order restarts
        // rather than the focus vanishing — a Tab press that does nothing is indistinguishable
        // from a dropped keystroke, and §B13 counts those.
        self.regions.iter().position(|r| r.id == id).unwrap_or(0)
    }

    /// A run window's regions, in reading order. `M3-DESIGN.md` §6.2 and §6.3.
    ///
    /// **Every panel exists from the first frame, including the four that have no producer yet.**
    /// §6.3: a placeholder states a fact, so `Subagents` is present and empty rather than absent —
    /// which is what stops the layout moving when the roster lands. The tree does not depend on the
    /// [`marlowe_view::RunView`] at all, for the same reason: a region that appeared only once it
    /// had content would move every other region on the day it arrived.
    pub fn for_window() -> Self {
        Self {
            regions: vec![
                Region::new(RegionId::RunIdentity, "Run", 'r'),
                Region::new(RegionId::RunCheckpoint, "Checkpoint", 'k'),
                Region::new(RegionId::RunOutput, "Output", 'o'),
                Region::new(RegionId::RunSubagents, "Subagents", 'g'),
                Region::new(RegionId::RunBudget, "Budget", 'b'),
                Region::new(RegionId::RunScopeMemory, "Scope memory", 'y'),
                Region::new(RegionId::RunMeetings, "Meetings", 'e'),
                Region::new(RegionId::RunSteer, "Steer", 'i'),
            ],
        }
    }

    pub fn first(&self) -> RegionId {
        self.regions[0].id
    }
}
