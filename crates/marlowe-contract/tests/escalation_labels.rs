//! **M3-DESIGN §3.6: an option label cannot contribute a line or move the cursor.**
//!
//! > *"Every other surface shows untrusted text to a human who is reading. This one shows it to a
//! > human about to choose between labelled options. A bidi override that swaps what an option
//! > reads as versus what it is lands directly on a decision."*
//!
//! # The trim-dependent control, in a new place
//!
//! `adr023_live.rs` passed on its first run because nothing was trimmed — the property it is named
//! for never occurred and it reported success anyway. The same shape is available here: a corpus of
//! "hostile" strings that turned out to be clean would make every assertion below vacuous, and the
//! test would be green on a build with `sanitize_line` deleted.
//!
//! So [`each_payload_in_the_corpus_actually_violates_something`] runs first and asserts each raw
//! input **is** hostile by the project's own predicate. If the corpus goes clean, that test fails
//! rather than the others quietly passing.

use marlowe_contract::escalation::{ArtifactHandle, OptionLabel, TextRejected, ValidatedSentence};
use marlowe_contract::text::is_renderable;

/// The payloads, each with the mechanism it is aiming at.
const CORPUS: &[(&str, &str)] = &[
    ("choose me\nterminate this agent", "a newline writes a second, fraudulent row"),
    ("choose me\r\nterminate this agent", "CRLF, same attack, different encoding"),
    ("safe option\u{1b}[2K\rterminate", "erase-line + CR repaints the row above"),
    ("\u{202e}option safe", "RIGHT-TO-LEFT OVERRIDE: what it reads as is not what it is"),
    ("a\u{2028}b", "LINE SEPARATOR — Zl, which some terminals break on"),
    ("a\u{200b}b", "ZERO WIDTH SPACE splits a word the human is matching against"),
    ("a\u{e0001}b", "the TAG block: a full invisible ASCII alphabet"),
    ("a\tb", "a tab, which is prose in a paragraph and a column in a decision row"),
];

/// **The control, and it runs first.** A corpus that had gone clean would make every other test in
/// this file pass without testing anything.
#[test]
fn each_payload_in_the_corpus_actually_violates_something() {
    for (raw, why) in CORPUS {
        let offends = raw.chars().any(|c| !is_renderable(c) || c == '\n' || c == '\t');
        assert!(
            offends,
            "the corpus entry {raw:?} ({why}) is clean by this project's own predicate, so every \
             assertion in this file that uses it is vacuous"
        );
    }
    assert!(CORPUS.len() >= 8, "the corpus shrank; a smaller corpus is a weaker claim");
}

/// Nothing survives normalisation that could add a row or move the cursor.
///
/// *Mutation:* have `normalise` return the raw string → every entry keeps its control character
/// → red on the first assertion.
#[test]
fn an_option_label_cannot_contribute_a_line_or_move_the_cursor() {
    for (raw, why) in CORPUS {
        let label = OptionLabel::normalise(raw)
            .unwrap_or_else(|e| panic!("{raw:?} ({why}) was refused rather than normalised: {e}"));
        let out = label.as_str();
        for bad in ['\n', '\r', '\t', '\u{1b}', '\u{202e}', '\u{2028}', '\u{200b}', '\u{e0001}'] {
            assert!(
                !out.contains(bad),
                "{raw:?} ({why}) normalised to {out:?}, which still carries U+{:04X}",
                bad as u32
            );
        }
        assert!(
            out.chars().all(is_renderable),
            "{raw:?} normalised to {out:?}, which is not renderable"
        );
        // **Marked, not dropped.** A stripped payload and a clean label must not be
        // indistinguishable to the person deciding — `text.rs` makes that argument for the
        // approval prompt and this row is the same decision surface.
        assert!(
            out.contains("<U+"),
            "{raw:?} was silently cleaned rather than marked: {out:?}"
        );
    }
}

/// **Refused, never truncated**, and the cap is measured on the string that reaches the screen.
#[test]
fn a_label_that_cannot_fit_is_refused_rather_than_shortened() {
    let long = "x".repeat(OptionLabel::MAX_CHARS + 1);
    assert!(matches!(
        OptionLabel::normalise(&long),
        Err(TextRejected::TooLong { chars, max })
            if chars == OptionLabel::MAX_CHARS + 1 && max == OptionLabel::MAX_CHARS
    ));
    assert!(OptionLabel::normalise(&"x".repeat(OptionLabel::MAX_CHARS)).is_ok());

    // The sentence has its own, larger cap, and the two do not share a constant by accident:
    // a label is one row of a decision list, a sentence is a paragraph under it.
    assert!(ValidatedSentence::normalise(&"x".repeat(ValidatedSentence::MAX_CHARS)).is_ok());
    assert!(ValidatedSentence::normalise(&"x".repeat(ValidatedSentence::MAX_CHARS + 1)).is_err());
    assert!(ValidatedSentence::MAX_CHARS > OptionLabel::MAX_CHARS);
}

/// An empty label is a row a human cannot choose between.
#[test]
fn a_blank_label_is_refused_rather_than_rendered_as_an_empty_row() {
    for blank in ["", "   ", "\n", "\u{200b}\u{200b}"] {
        // The zero-width entries normalise to `<U+200B>` markers, which are NOT blank — that is
        // the marking discipline working, and it is asserted rather than assumed.
        match OptionLabel::normalise(blank) {
            Err(TextRejected::Empty) => {}
            Ok(l) => assert!(
                l.as_str().contains("<U+"),
                "{blank:?} produced a non-empty label {:?} that is not a marker",
                l.as_str()
            ),
            Err(e) => panic!("{blank:?} was refused for the wrong reason: {e}"),
        }
    }
}

/// **CLAUDE.md instance #12: a validating constructor must be the only way in, and `serde` is a
/// way in.**
///
/// A checkpoint, an MCP descriptor and a spawn request are all outside input. Every in-code test
/// above would stay green on a build with `#[derive(Deserialize)]` here, and the one path that
/// reads outside input would skip `normalise` entirely.
///
/// *Mutation:* replace the hand-written `Deserialize` with a derive on the
/// `#[serde(transparent)]` newtype → both `from_str` calls succeed → red.
#[test]
fn serde_cannot_route_around_normalise() {
    // A JSON string carrying an escape sequence. This is what a checkpoint written by a build
    // with a hole in it, or an MCP descriptor, looks like on the way in.
    let hostile = r#""a\u001b[2Kb""#;
    let l: OptionLabel = serde_json::from_str(hostile).expect("marked, not refused");
    assert!(
        !l.as_str().contains('\u{1b}') && l.as_str().contains("<U+001B>"),
        "serde produced {:?}, which means the decode did not go through `normalise`",
        l.as_str()
    );

    // The half a `#[derive(Deserialize)]` cannot fake: a bound is not something a field-wise
    // decode checks.
    let over = format!("\"{}\"", "x".repeat(OptionLabel::MAX_CHARS + 1));
    assert!(
        serde_json::from_str::<OptionLabel>(&over).is_err(),
        "a checkpoint carrying an over-long label decoded -- `normalise` was not consulted"
    );
    let over_s = format!("\"{}\"", "x".repeat(ValidatedSentence::MAX_CHARS + 1));
    assert!(serde_json::from_str::<ValidatedSentence>(&over_s).is_err());

    // The positive control: a clean label still decodes, so the two assertions above are not
    // passing because `Deserialize` is broken for every input.
    let fine: OptionLabel = serde_json::from_str("\"widen the scope\"").expect("a plain label");
    assert_eq!(fine.as_str(), "widen the scope");
}

/// The artifact handle is an address or it is refused — no path, no URL, no name a model chose.
#[test]
fn an_artifact_handle_is_an_address_and_serde_cannot_route_around_that_either() {
    assert!(serde_json::from_str::<ArtifactHandle>("\"../../etc/passwd\"").is_err());
    assert!(serde_json::from_str::<ArtifactHandle>("\"file:///etc/passwd\"").is_err());
    let ok: ArtifactHandle =
        serde_json::from_str("\"0123456789abcdef0123456789abcdef\"").expect("32 hex characters");
    assert_eq!(ok.as_str().len(), ArtifactHandle::HEX_CHARS);
}
