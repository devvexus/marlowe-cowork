//! The key registry, validated **at startup**.
//!
//! CLAUDE.md's standing rule: *"Watch for defaults that make a mismatch unobservable. Prefer a
//! load-time error to a sensible default."* A duplicate hotkey is exactly that shape. The sensible
//! default is first-match-wins, and it fails silently: one region's key quietly stops working, the
//! screen still says `(w)` on its bottom border, and nothing observes the disagreement. §B13's
//! *"regions reachable by keyboard alone — 100%"* would go green while a region was unreachable.
//!
//! So the registry is built and validated before the first frame, across **all six inspector
//! panes** rather than only the visible one, and a conflict is a refusal to start.
//!
//! # Three scopes, and the rule against shadowing
//!
//! | scope | keys | when active |
//! |---|---|---|
//! | region | `m p s w a v c i` | always (§B10: hotkeys jump focus directly) |
//! | tab | `1`–`6` | always |
//! | item | per pane, per §B7 | when that pane is displayed |
//!
//! **An item key may not equal a region or tab key.** It could — only one pane is visible at a
//! time, so a shadow would be well-defined — but a well-defined shadow is still a lie: the
//! Workspace region's border says `(w)`, and a `w` that sometimes means Workspace and sometimes
//! means "Tomorrow" is a discoverability failure with no cue that it happened.
//!
//! **Item keys may repeat across panes.** Two panes are never visible at once, and nothing on
//! screen claims otherwise.

use std::collections::BTreeMap;

use crate::region::{RegionId, TabId};

/// What a key does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Binding {
    /// Jump focus to a region.
    Focus(RegionId),
    /// Switch the inspector to a tab.
    Tab(TabId),
}

/// A startup refusal. Carries both claimants, because "duplicate key" without them is a bug report
/// the reader has to reproduce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyConflict {
    pub key: char,
    pub first: String,
    pub second: String,
}

impl std::fmt::Display for KeyConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "hotkey '{}' is claimed by both {} and {}. §B2 says the hotkey on a region's bottom \
             border is how you reach it; two regions claiming one key means one of those borders \
             is lying, and first-match-wins would hide which. Give one of them another key.",
            self.key, self.first, self.second
        )
    }
}

impl std::error::Error for KeyConflict {}

/// Every key the interface answers to, resolved once.
#[derive(Debug, Clone, Default)]
pub struct KeyRegistry {
    global: BTreeMap<char, Binding>,
    /// Per pane: key -> item index within that pane.
    items: BTreeMap<TabId, BTreeMap<char, usize>>,
}

impl KeyRegistry {
    /// Build and validate. **Every pane, not just the visible one** — a collision in the Trust
    /// pane must be a startup error on a session that never opens Trust, or it ships.
    ///
    /// `items_for` supplies the items of each pane; the registry does not know where they come
    /// from, so the layout that owns the panes stays the caller's.
    pub fn build(
        items_for: impl Fn(marlowe_view::Tab) -> Vec<marlowe_view::Item>,
    ) -> Result<Self, KeyConflict> {
        let mut reg = KeyRegistry::default();

        let region_keys: [(char, RegionId, &str); 8] = [
            ('m', RegionId::Model, "Model"),
            ('p', RegionId::Profile, "Profile"),
            ('s', RegionId::Session, "Session"),
            ('w', RegionId::Workspace, "Workspace"),
            ('a', RegionId::Autonomy, "Autonomy"),
            ('v', RegionId::Status, "Status band"),
            ('c', RegionId::Conversation, "Conversation"),
            ('i', RegionId::Message, "Message"),
        ];
        let mut owner: BTreeMap<char, String> = BTreeMap::new();

        // §B10's copy keys are reserved before anything else claims them.
        //
        // They are dispatched ahead of this registry, so a pane item bound to `y` would not
        // conflict — it would simply never fire, which is the silent-shadowing failure this whole
        // type exists to make impossible. Recording them as owners turns that into a startup error
        // naming both claimants, exactly as a region collision does.
        owner.insert('y', "copy (§B10)".to_string());
        owner.insert('Y', "copy transcript (§B10)".to_string());

        for (key, id, name) in region_keys {
            reg.claim_global(key, Binding::Focus(id), name, &mut owner)?;
        }
        for tab in marlowe_view::Tab::ALL {
            reg.claim_global(
                tab.digit(),
                Binding::Tab(tab.into()),
                &format!("the {} tab", tab.title()),
                &mut owner,
            )?;
        }

        for tab in marlowe_view::Tab::ALL {
            let mut pane: BTreeMap<char, usize> = BTreeMap::new();
            let tab_items = items_for(tab);
            for (i, item) in tab_items.iter().enumerate() {
                // **An item with no letter claims none.** The pane's pool is finite; see
                // `Item::key` for why running out beats handing two items the same key.
                let Some(item_key) = item.key else { continue };
                if let Some(existing) = owner.get(&item_key) {
                    return Err(KeyConflict {
                        key: item_key,
                        first: existing.clone(),
                        second: format!("the {} item '{}'", tab.title(), item.label),
                    });
                }
                if let Some(prev) = pane.get(&item_key) {
                    let prev_label = tab_items[*prev].label.clone();
                    return Err(KeyConflict {
                        key: item_key,
                        first: format!("the {} item '{prev_label}'", tab.title()),
                        second: format!("the {} item '{}'", tab.title(), item.label),
                    });
                }
                pane.insert(item_key, i);
            }
            reg.items.insert(tab.into(), pane);
        }

        Ok(reg)
    }

    fn claim_global(
        &mut self,
        key: char,
        binding: Binding,
        name: &str,
        owner: &mut BTreeMap<char, String>,
    ) -> Result<(), KeyConflict> {
        if let Some(existing) = owner.get(&key) {
            return Err(KeyConflict {
                key,
                first: existing.clone(),
                second: name.to_string(),
            });
        }
        owner.insert(key, name.to_string());
        self.global.insert(key, binding);
        Ok(())
    }

    /// Resolve a key press. `tab` is the pane currently displayed, because item keys are
    /// pane-scoped.
    pub fn resolve(&self, key: char, tab: TabId) -> Option<Binding> {
        if let Some(b) = self.global.get(&key) {
            return Some(*b);
        }
        self.items
            .get(&tab)
            .and_then(|pane| pane.get(&key))
            .map(|i| Binding::Focus(RegionId::Item(tab, *i)))
    }

    /// Every key this registry answers to, for the acceptance suite.
    pub fn all_keys(&self) -> Vec<char> {
        let mut keys: Vec<char> = self.global.keys().copied().collect();
        for pane in self.items.values() {
            keys.extend(pane.keys().copied());
        }
        keys.sort_unstable();
        keys.dedup();
        keys
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(label: &str, key: char) -> marlowe_view::Item {
        marlowe_view::Item::new(label, key, marlowe_view::Tone::Normal, &[])
    }

    fn registry_with(runs: Vec<marlowe_view::Item>) -> Result<KeyRegistry, KeyConflict> {
        KeyRegistry::build(|tab| match tab {
            marlowe_view::Tab::Runs => runs.clone(),
            _ => Vec::new(),
        })
    }

    #[test]
    fn an_empty_item_set_has_no_conflicts() {
        registry_with(Vec::new()).expect("region and tab keys do not collide");
    }

    #[test]
    fn item_keys_are_pane_scoped_and_globals_win_everywhere() {
        let reg = registry_with(vec![item("Steer", 'g')]).unwrap();
        assert_eq!(
            reg.resolve('c', TabId::Runs),
            Some(Binding::Focus(RegionId::Conversation))
        );
        assert_eq!(
            reg.resolve('2', TabId::Runs),
            Some(Binding::Tab(TabId::Schedule))
        );
        assert!(matches!(
            reg.resolve('g', TabId::Runs),
            Some(Binding::Focus(RegionId::Item(TabId::Runs, _)))
        ));
        assert_eq!(reg.resolve('g', TabId::Schedule), None);
    }

    #[test]
    fn an_item_key_that_shadows_a_region_key_is_refused() {
        let err = registry_with(vec![item("Tomorrow", 'w')]).unwrap_err();
        assert_eq!(err.key, 'w');
    }

    #[test]
    fn a_conflict_names_both_claimants() {
        let c = KeyConflict {
            key: 'w',
            first: "Workspace".into(),
            second: "the Schedule item 'Tomorrow'".into(),
        };
        let msg = c.to_string();
        assert!(msg.contains("Workspace") && msg.contains("Tomorrow"));
    }
}
