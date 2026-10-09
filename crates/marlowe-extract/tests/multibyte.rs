//! **The corpus that was missing: non-ASCII.**
//!
//! `adversarial.rs` and `injector.rs` between them run ~30 hostile inputs through the extractor and
//! assert it never panics. Every byte in both is ASCII. A security audit found three reachable
//! panics by adding a single accented character, and all three suites stayed green over every one
//! of them — the tests were asserting "does not panic on these bytes", which is true and is not the
//! property anyone wanted.
//!
//! Each case below names the finding it pins. Reverting the corresponding fix makes it panic, and
//! a panic here is not a soft failure: it propagates out of `rayon` and out of
//! `std::thread::scope`, and its message carries ~256 bytes of the document to the orchestrator.

use marlowe_extract::{extract, Input};

/// One accented character. The entire audit finding, in one constant.
const ACCENT: char = 'é';

/// G2 — `decode_entities` sliced at a fixed `i + 32`, which is an arbitrary byte offset.
/// `&` + exactly 30 ASCII characters puts byte 32 inside the following two-byte character.
#[test]
fn an_entity_scan_window_landing_mid_character_does_not_panic() {
    let body = format!("<p>&{}\u{e9}</p>", "a".repeat(30));
    let d = extract(&Input::new(body.as_bytes()).content_type(Some("text/html"))).expect("extracts");
    assert!(d.text.contains('\u{e9}'), "the character survived: {:?}", d.text);
}

/// The same window at every offset either side, so the fix is not an off-by-one that happens to
/// clear one case.
#[test]
fn the_entity_scan_window_is_safe_at_every_nearby_offset() {
    for pad in 0..48 {
        for tail in ["\u{e9}", "\u{20ac}", "\u{1f600}"] {
            let body = format!("<p>&{}{tail}</p>", "a".repeat(pad));
            let _ = extract(&Input::new(body.as_bytes()).content_type(Some("text/html")))
                .expect("extracts");
        }
    }
}

/// G3 — the JSON escape arm stepped `i += 2` over a multi-byte escaped character and then sliced
/// at a non-boundary. A backslash followed directly by a two-byte character was enough.
#[test]
fn a_json_escape_of_a_multibyte_character_does_not_panic() {
    // Built from the constant, not written as an escape. The first attempt put `\\u{e9}` in the
    // source, which is seven literal ASCII characters — so the test exercised ASCII while claiming
    // to exercise a multi-byte escape. The same mistake as the corpus it was written to fix, one
    // level up. The premise assertion below is what caught it.
    let body = format!("[\"\\{ACCENT}\"]");
    assert!(body.as_bytes().contains(&0xC3), "premise: the input really is multi-byte");
    let d = extract(&Input::new(body.as_bytes()).content_type(Some("application/json")))
        .expect("extracts");
    assert!(d.text.contains(ACCENT), "got {:?}", d.text);
}

/// G17 — and the reader must not silently swallow the fields after a non-ASCII string.
#[test]
fn fields_after_a_non_ascii_json_string_are_not_swallowed() {
    let body = r#"{"a":"caf\u00e9 na\u00efve","secret_field":"KEPT","b":"ALSO-KEPT"}"#;
    let d = extract(&Input::new(body.as_bytes()).content_type(Some("application/json")))
        .expect("extracts");
    assert!(d.text.contains("KEPT"), "a following field was swallowed: {:?}", d.text);
    assert!(d.text.contains("ALSO-KEPT"), "a later field was swallowed: {:?}", d.text);
}

/// G4 — `close()` sliced `self.current` at an offset captured before `flush_block` cleared it.
/// `.min(len)` kept it in bounds and said nothing about char boundaries.
#[test]
fn a_heading_after_a_flush_does_not_panic_on_a_multibyte_character() {
    let d = extract(&Input::new("<p>a<h1>\u{e9}</h1>".as_bytes()).content_type(Some("text/html")))
        .expect("extracts");
    assert!(d.headings.iter().any(|h| h.text.contains('\u{e9}')), "got {:?}", d.headings);
}

/// The anchor variant of the same stale offset.
#[test]
fn an_anchor_spanning_a_flush_does_not_panic_on_a_multibyte_character() {
    let markup = "<p>a<a href=\"/x\"><p>\u{e9}</a>";
    let _ = extract(&Input::new(markup.as_bytes()).content_type(Some("text/html")))
        .expect("extracts");
}

/// G9 — a raw-text element ends at `</name` followed by a TERMINATOR. Without that check the
/// script body was reparsed as markup and emitted as prose: inert JavaScript in a browser,
/// visible instructions here.
#[test]
fn a_script_does_not_end_at_a_lookalike_closing_tag() {
    let markup = "<script>var s=\"</scriptX>\";LEAKED-SCRIPT-BODY</script><p>real</p>";
    let d = extract(&Input::new(markup.as_bytes()).content_type(Some("text/html")))
        .expect("extracts");
    assert!(!d.text.contains("LEAKED-SCRIPT-BODY"), "script body leaked: {:?}", d.text);
    assert!(d.text.contains("real"));
}

/// G8 — a stray `</script>` decremented a skip depth that no open tag had raised, reopening a
/// skipped container's body as ordinary text.
#[test]
fn a_stray_closing_tag_cannot_reopen_a_skipped_container() {
    let markup = "<svg></script>HIDDEN-SVG-BODY</svg><p>real</p>";
    let d = extract(&Input::new(markup.as_bytes()).content_type(Some("text/html")))
        .expect("extracts");
    assert!(!d.text.contains("HIDDEN-SVG-BODY"), "skipped body leaked: {:?}", d.text);
}

/// G11 — an unclosed `<title>` made the entire document the title, bypassing every text cap and
/// then being stored permanently and prepended by the renderer.
#[test]
fn an_unclosed_title_does_not_become_the_whole_document() {
    let body = format!("<html><head><title>{}", "A".repeat(200_000));
    let d = extract(&Input::new(body.as_bytes()).content_type(Some("text/html"))).expect("extracts");
    let title = d.title.unwrap_or_default();
    assert!(title.len() <= 8_192, "title was {} bytes", title.len());
}

/// G1 — a panic payload must never reach the caller, because Rust's char-boundary message embeds
/// ~256 characters of the string being sliced, and `web` emits that detail at `AgentObserved`.
#[test]
fn no_error_detail_ever_quotes_the_document() {
    const MARKER: &str = "SECRET-DOCUMENT-BYTES-MUST-NOT-APPEAR-IN-AN-ERROR";
    let cases: Vec<Vec<u8>> = vec![
        format!("[\"{MARKER}\\{}\"]", ACCENT).into_bytes(),
        format!("<p>&{}{MARKER}\u{e9}</p>", "a".repeat(30)).into_bytes(),
        {
            let mut v = b"%PDF-1.7\n".to_vec();
            v.extend_from_slice(MARKER.as_bytes());
            v.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF].repeat(300));
            v
        },
    ];
    for bytes in cases {
        if let Err(e) = extract(&Input::new(&bytes)) {
            assert!(
                !e.to_string().contains(MARKER),
                "an error quoted the document: {e}"
            );
        }
    }
}

/// The whole point, restated as one assertion: hostile multi-byte input never unwinds into the
/// caller, because the caller is a rayon fan-out and a `thread::scope` that re-raises on join.
#[test]
fn multibyte_hostile_input_never_unwinds_into_the_caller() {
    let mut cases: Vec<Vec<u8>> = Vec::new();
    for tail in ["\u{e9}", "\u{20ac}", "\u{1f600}", "\u{0301}", "\u{fffd}"] {
        cases.push(format!("<p>&{}{tail}</p>", "a".repeat(30)).into_bytes());
        cases.push(format!("[\"\\{tail}\"]").into_bytes());
        cases.push(format!("<p>a<h1>{tail}</h1>").into_bytes());
        cases.push(format!("<a href=\"/x\">{tail}").into_bytes());
        cases.push(format!("{tail}<!--").into_bytes());
        cases.push(format!("<title>{tail}").into_bytes());
    }
    for bytes in cases {
        let r = std::panic::catch_unwind(|| extract(&Input::new(&bytes)));
        assert!(r.is_ok(), "unwound on {:?}", String::from_utf8_lossy(&bytes));
    }
}
