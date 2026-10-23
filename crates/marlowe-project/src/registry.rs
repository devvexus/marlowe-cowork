//! The form and kind registry: every form and project kind the build knows, as data.
//!
//! Forms live in `registry/forms/*.json` and kinds in `registry/kinds/*.json`, embedded at build
//! time. A kind is a template (default phases, the forms it uses, overview rollups, which saved
//! team tasks it supersedes); adding one is a JSON file and an entry in the lists below, not code
//! in the engine, the store or the renderer. A project state embeds the forms it uses, so a
//! project (and its screen) outlives a registry change.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::form::{Form, ShowAs};

/// The phase menu: the only phase names a plan may list.
pub const PHASE_MENU: [&str; 8] = ["Plan", "DataCollection", "DataValidation", "Thinking", "Checkin", "Report", "Finish", "Engineering"];

/// One line of the overview: a count of items (or agents) grouped by a field.
///
/// `over` is an item kind, or `agents` for the project's agents. `by` is a path read from each
/// item (`data.relevancy`, `status`); the overview counts distinct values.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rollup {
    /// The row's title in the overview.
    pub title: String,
    /// An item kind, or `agents`.
    pub over: String,
    /// The path grouped by.
    pub by: String,
    /// How a group is drawn; `badge` when the grouped field is an enum, else `text`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show: Option<ShowAs>,
}

/// A kind of project: the template a plan's `kind` names.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KindTemplate {
    /// The kind's name.
    pub kind: String,
    /// A title for lists.
    pub title: String,
    /// Whether a plan may name it. A kind that is designed and registered but not built is `false`.
    pub available: bool,
    /// The default phase list.
    pub phases: Vec<String>,
    /// The forms (`name@version`) a project of this kind embeds.
    pub forms: Vec<String>,
    /// Saved team tasks this kind replaces (the naming note points at `project` instead).
    #[serde(default)]
    pub supersedes: Vec<String>,
    /// The overview rows. Empty falls back to counting items by kind and status.
    #[serde(default)]
    pub rollups: Vec<Rollup>,
}

/// The registry of forms and kinds.
#[derive(Debug, Clone, Default)]
pub struct Registry {
    forms: BTreeMap<String, Form>,
    kinds: BTreeMap<String, KindTemplate>,
}

macro_rules! embedded_forms {
    ($($name:literal),* $(,)?) => {
        &[$(include_str!(concat!("../registry/forms/", $name, ".json"))),*]
    };
}

const FORMS: &[&str] = embedded_forms![
    "question", "attack_vectors", "page", "analyze", "claim", "cclaim", "condense", "verify", "lead_decision", "section", "answer", "audit",
    "validated", "experiment"
];

const KINDS: &[&str] = &[include_str!("../registry/kinds/research.json"), include_str!("../registry/kinds/engineering.json")];

impl Registry {
    /// An empty registry (tests that bring their own forms).
    pub fn empty() -> Registry {
        Registry::default()
    }

    /// Every form and kind the build ships. The embedded JSON is checked by tests, so a parse
    /// failure here is a build defect: such an entry is skipped rather than panicking at runtime.
    pub fn builtin() -> Registry {
        let mut r = Registry::empty();
        for src in FORMS {
            if let Ok(f) = serde_json::from_str::<Form>(src) {
                r.register_form(f);
            }
        }
        for src in KINDS {
            if let Ok(k) = serde_json::from_str::<KindTemplate>(src) {
                r.register_kind(k);
            }
        }
        r
    }

    /// Adds (or replaces) a form.
    pub fn register_form(&mut self, form: Form) {
        self.forms.insert(form.id(), form);
    }

    /// Adds (or replaces) a kind.
    pub fn register_kind(&mut self, kind: KindTemplate) {
        self.kinds.insert(kind.kind.clone(), kind);
    }

    /// The form `name@version`.
    pub fn form(&self, id: &str) -> Option<&Form> {
        self.forms.get(id)
    }

    /// Every form id.
    pub fn form_ids(&self) -> Vec<&str> {
        self.forms.keys().map(String::as_str).collect()
    }

    /// The kind template.
    pub fn kind(&self, name: &str) -> Option<&KindTemplate> {
        self.kinds.get(name)
    }

    /// The names of the kinds a plan may name, sorted.
    pub fn available_kinds(&self) -> Vec<&str> {
        self.kinds.values().filter(|k| k.available).map(|k| k.kind.as_str()).collect()
    }

    /// The forms of a kind, in the kind's order, cloned for embedding in a project state.
    pub fn forms_for(&self, kind: &str) -> BTreeMap<String, Form> {
        let mut out = BTreeMap::new();
        if let Some(k) = self.kind(kind) {
            for id in &k.forms {
                if let Some(f) = self.forms.get(id) {
                    out.insert(id.clone(), f.clone());
                }
            }
        }
        out
    }

    /// The kinds that replace a saved team task of this name (case and dashes/underscores ignored).
    pub fn superseded_by(&self, task: &str) -> Option<&str> {
        let norm = |s: &str| s.to_lowercase().replace('_', "-");
        let t = norm(task);
        self.kinds.values().find(|k| k.available && k.supersedes.iter().any(|s| norm(s) == t)).map(|k| k.kind.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::form::{Field, FieldType, Who};

    #[test]
    fn every_embedded_form_and_kind_parses() {
        for src in FORMS {
            serde_json::from_str::<Form>(src).unwrap_or_else(|e| panic!("form does not parse: {e}\n{}", &src[..src.len().min(80)]));
        }
        for src in KINDS {
            serde_json::from_str::<KindTemplate>(src).unwrap_or_else(|e| panic!("kind does not parse: {e}"));
        }
        let r = Registry::builtin();
        assert_eq!(r.form_ids().len(), FORMS.len());
        assert_eq!(r.kinds.len(), KINDS.len());
    }

    #[test]
    fn every_registered_form_is_sound() {
        let r = Registry::builtin();
        for id in r.form_ids() {
            let errs = r.form(id).unwrap().validate();
            assert!(errs.is_empty(), "{id}: {errs:#?}");
        }
    }

    #[test]
    fn every_kind_lists_forms_that_exist_and_phases_from_the_menu() {
        let r = Registry::builtin();
        for k in r.kinds.values() {
            for f in &k.forms {
                assert!(r.form(f).is_some(), "{} lists unknown form {f}", k.kind);
            }
            for p in &k.phases {
                assert!(PHASE_MENU.contains(&p.as_str()), "{} lists unknown phase {p}", k.kind);
            }
        }
        assert_eq!(r.available_kinds(), ["research"]);
    }

    #[test]
    fn no_form_carries_a_colour_value() {
        let r = Registry::builtin();
        for id in r.form_ids() {
            let v = serde_json::to_value(r.form(id).unwrap()).unwrap();
            assert_eq!(crate::form::find_hex_colour(&v), None, "{id}");
        }
        for src in FORMS.iter().chain(KINDS) {
            assert!(!src.contains("#9B") && !src.contains("#8f") && !src.contains("#d6") && !src.contains("#dd") && !src.contains("#7f"), "a palette hex is in a registry file");
        }
    }

    #[test]
    fn every_form_round_trips_through_serde_unchanged() {
        let r = Registry::builtin();
        for id in r.form_ids() {
            let f = r.form(id).unwrap();
            let s = serde_json::to_string(f).unwrap();
            let back: Form = serde_json::from_str(&s).unwrap();
            assert_eq!(&back, f, "{id}");
        }
    }

    /// JSON equality where `0` and `0.0` are the same number.
    fn same(a: &serde_json::Value, b: &serde_json::Value) -> bool {
        use serde_json::Value::*;
        match (a, b) {
            (Number(x), Number(y)) => x.as_f64() == y.as_f64(),
            (Array(x), Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(p, q)| same(p, q)),
            (Object(x), Object(y)) => x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| same(v, w))),
            _ => a == b,
        }
    }

    #[test]
    fn a_registered_forms_file_equals_its_reserialised_value() {
        // The JSON on disk is the canonical form: reading and writing it changes no value.
        for src in FORMS {
            let raw: serde_json::Value = serde_json::from_str(src).unwrap();
            let f: Form = serde_json::from_value(raw.clone()).unwrap();
            assert!(same(&serde_json::to_value(&f).unwrap(), &raw), "{} does not survive a read and write", f.id());
        }
    }

    #[test]
    fn the_model_never_sees_a_code_field_in_any_research_form() {
        let r = Registry::builtin();
        let plan = crate::plan::test_plan();
        for id in r.kind("research").unwrap().forms.iter() {
            let f = r.form(id).unwrap();
            if !f.fields.iter().any(|x| x.w == Who::Model) {
                continue;
            }
            let schema = crate::form::model_schema(f, &plan);
            marlowe_contract::schema::check_shape(&schema).unwrap_or_else(|e| panic!("{id}: {e}"));
            let props = schema["properties"].as_object().unwrap();
            for field in &f.fields {
                assert_eq!(props.contains_key(&field.key), field.w == Who::Model, "{id}.{}", field.key);
            }
        }
    }

    #[test]
    fn analyze_schema_matches_the_design_table() {
        let r = Registry::builtin();
        let plan = crate::plan::test_plan();
        let s = crate::form::model_schema(r.form("analyze@1").unwrap(), &plan);
        let keys: Vec<&String> = s["properties"].as_object().unwrap().keys().collect();
        assert_eq!(keys, ["relevancy", "source_kind", "one_line", "notes", "claims", "additional_sources", "records"]);
        assert_eq!(s["properties"]["relevancy"]["enum"], serde_json::json!(["Critical", "High", "Medium", "Low", "Skip"]));
        assert_eq!(s["properties"]["claims"]["maxItems"], 8);
        assert_eq!(s["properties"]["claims"]["items"]["properties"].as_object().unwrap().len(), 2, "only text and quote; sub is code");
        let add = s["properties"]["additional_sources"]["items"]["properties"].as_object().unwrap();
        assert_eq!(add.keys().collect::<Vec<_>>(), ["link", "reference", "opportunity", "notes"], "index and source are filled by code");
        assert_eq!(s["properties"]["records"]["items"]["properties"].as_object().unwrap().len(), 5);
        // A model-written url cannot exist in the schema: no string field offers one.
        assert!(!s.to_string().contains("uri"));
    }

    #[test]
    fn a_saved_task_a_kind_replaces_is_found_by_its_loose_name() {
        let r = Registry::builtin();
        assert_eq!(r.superseded_by("web-research"), Some("research"));
        assert_eq!(r.superseded_by("Web_Research"), Some("research"));
        assert_eq!(r.superseded_by("triage"), None);
    }

    #[test]
    fn a_form_can_be_registered_without_touching_any_code_path() {
        let mut r = Registry::empty();
        let mut f = Field::new("title", FieldType::String, Who::Plan);
        f.max_len = Some(40);
        r.register_form(Form { form: "widget".into(), version: 2, title: "Widget".into(), item_label: None, fields: vec![f], rules: vec![] });
        assert!(r.form("widget@2").is_some());
    }
}
