//! **What models this provider serves**, so `/provider openrouter` has a list to offer.
//!
//! # Why this is a separate thing from the driver
//!
//! The driver talks to `/chat/completions` with a credential. This talks to `/models`, which is
//! **public** — no key, no account, no `Authorization` header — so it carries none of the driver's
//! key-containment surface and needs none of its transport. `marlowe_net::fetch` is the whole
//! mechanism.
//!
//! That difference is the reason it is worth stating out loud: a reader who assumes every call to
//! `openrouter.ai` carries the key would look for a leak here, and there is nothing to find
//! because there is nothing to leak.
//!
//! # It is fetched on the switch, never on the status path
//!
//! `Daemon::status()` is called on essentially every tick of the surface. A network call there
//! would put an internet round trip inside the repaint loop, which is the freeze this project has
//! logged before under a different cause. The catalogue is fetched **once, when the user asks to
//! switch provider**, and cached on the config; a failure degrades to the configured slug rather
//! than emptying the picker.
//!
//! # The parse is separate from the fetch, and only the parse has a test
//!
//! `parse_models` is pure and is where every decision lives — which field is the id, what to do
//! with an entry that has none, what ordering the picker gets. `fetch_models` is a socket and one
//! line of glue. Testing the glue would need a network; testing the decisions does not.

/// Where the catalogue lives. Public, and on the same host the driver already talks to — so this
/// is not a new destination, it is another path on an existing relationship.
const CATALOGUE_URL: &str = "https://openrouter.ai/api/v1/models";

/// How many bytes of catalogue to read. The real response is a few hundred KB of JSON and this is
/// a bound, not an expectation: an endpoint that answered with a gigabyte would otherwise be a
/// memory fault in the daemon rather than a failed switch.
const MAX_CATALOGUE_BYTES: usize = 8 * 1024 * 1024;

/// Every model id the catalogue lists, sorted, deduplicated.
///
/// Sorted because it populates a picker and a picker whose order changes between fetches makes
/// the same keystroke choose different things. Deduplicated because an id is an address and two
/// entries at one address is the catalogue's problem, not the picker's.
pub fn parse_models(json: &str) -> Result<Vec<String>, String> {
    let v: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("the catalogue is not JSON: {e}"))?;
    let Some(data) = v.get("data").and_then(|d| d.as_array()) else {
        // **Named rather than defaulted to empty.** An empty picker and a catalogue we could not
        // read are different states, and only one of them is worth showing a remedy for.
        return Err("the catalogue has no `data` array".to_string());
    };
    let mut out: Vec<String> = data
        .iter()
        // An entry with no `id` is skipped rather than rendered as a blank row: a picker option
        // that selects nothing is a control that lies about what it does.
        .filter_map(|m| m.get("id").and_then(|i| i.as_str()))
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .collect();
    out.sort();
    out.dedup();
    if out.is_empty() {
        return Err("the catalogue listed no usable model ids".to_string());
    }
    Ok(out)
}

/// Fetch and parse the catalogue. **One round trip, on the switch, never on the status path.**
pub fn fetch_models() -> Result<Vec<String>, String> {
    let target = marlowe_net::Target::parse(CATALOGUE_URL).map_err(|e| e.to_string())?;
    let fetched = marlowe_net::fetch(&target).map_err(|e| e.to_string())?;
    if fetched.status != 200 {
        return Err(format!("openrouter.ai answered {} for its model list", fetched.status));
    }
    let body = String::from_utf8_lossy(
        &fetched.bytes[..fetched.bytes.len().min(MAX_CATALOGUE_BYTES)],
    )
    .into_owned();
    parse_models(&body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_taken_sorted_and_deduplicated() {
        let json = r#"{"data":[
            {"id":"z/last","name":"Z"},
            {"id":"a/first","name":"A"},
            {"id":"a/first","name":"A again"}
        ]}"#;
        assert_eq!(parse_models(json).unwrap(), vec!["a/first", "z/last"]);
    }

    #[test]
    fn an_entry_with_no_usable_id_is_skipped_rather_than_rendered_blank() {
        // A picker option that selects nothing is a control that lies about what it does.
        let json = r#"{"data":[{"name":"no id"},{"id":""},{"id":"real/model"}]}"#;
        assert_eq!(parse_models(json).unwrap(), vec!["real/model"]);
    }

    #[test]
    fn an_unreadable_catalogue_is_an_error_and_not_an_empty_list() {
        // The two states are different and only one deserves a remedy. Defaulting to empty would
        // put a picker with nothing in it on screen and call that a successful switch.
        assert!(parse_models("not json").is_err());
        assert!(parse_models(r#"{"models":[]}"#).is_err(), "no `data` array");
        assert!(parse_models(r#"{"data":[]}"#).is_err(), "no usable ids");
        assert!(parse_models(r#"{"data":[{"name":"x"}]}"#).is_err(), "no usable ids");
    }
}
