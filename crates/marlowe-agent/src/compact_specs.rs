//! The built-in tools' model-facing specs, in one reviewed file (`compact_specs.json`): name,
//! one-line description and a flat schema whose plain-string properties carry no `type` (the typed
//! form for strict endpoints is `marlowe_provider::wire::typed_schema`). Every group file and the
//! daemon's memory/crew tools take their spec from here, so wording and argument names change in
//! one place and the pinned fixtures in `marlowe-exec` show it.

use std::sync::OnceLock;

use marlowe_model::ToolSpec;

static SPECS: OnceLock<Vec<ToolSpec>> = OnceLock::new();

/// Every built-in spec, in the order of the file.
pub fn all() -> &'static [ToolSpec] {
    SPECS.get_or_init(|| serde_json::from_str(include_str!("compact_specs.json")).expect("compact_specs.json is valid"))
}

/// The spec of built-in tool `name`. Panics when the file has none (a registration bug the first
/// test that builds a host catches).
pub fn spec(name: &str) -> ToolSpec {
    all().iter().find(|s| s.name == name).unwrap_or_else(|| panic!("compact_specs.json has no tool `{name}`")).clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_file_parses_and_names_are_unique() {
        let names: std::collections::BTreeSet<_> = all().iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names.len(), all().len());
        assert!(all().iter().all(|s| !s.description.is_empty() && s.input_schema["type"] == "object"));
    }
}
