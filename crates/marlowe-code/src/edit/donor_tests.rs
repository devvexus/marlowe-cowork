//! Donor edge cases as acceptance fixtures for the edit engine (M4.6 donor policy: their cases,
//! our code). Each test names its source; where our documented semantics differ on purpose, the
//! test asserts ours and says why. Tool-level donor cases (read windows, the Codex apply-patch
//! scenarios, rollback) live in `marlowe-exec/src/specs/files/tests.rs`.
//!
//! | Source | Repo, sha | Licence |
//! |---|---|---|
//! | `codex-rs/apply-patch/src/seek_sequence.rs` tests, `text_file.rs` line-ending rules | openai/codex `6c0c6759d0617d7670826b61aca0a931bbb6db1f` | Apache-2.0 |
//! | `crates/tools/src/fs_tools.rs` tests | flashback7766/flashagent `0b08afcd6042615d38d87e80c3e5edda2a49da71` | MIT |

use super::*;

fn applied(text: &str, old: &str, new: &str, all: bool) -> Applied {
    apply(text, old, new, all).unwrap_or_else(|e| panic!("{old:?} -> {new:?}: {}", e.message()))
}

// ---------------------------------------------------------------- codex seek_sequence

/// `test_exact_match_finds_sequence`: a run of whole lines is found where it starts.
#[test]
fn codex_seek_exact_match_finds_sequence() {
    let a = applied("foo\nbar\nbaz\n", "bar\nbaz", "BAR\nBAZ", false);
    assert_eq!((a.tier, a.lines.as_slice(), a.text.as_str()), (Tier::Exact, &[2][..], "foo\nBAR\nBAZ\n"));
}

/// `test_rstrip_match_ignores_trailing_whitespace`: trailing spaces and tabs on the file's lines
/// do not stop a match (our whitespace tier), and the tier is named.
#[test]
fn codex_seek_rstrip_ignores_trailing_whitespace() {
    let a = applied("foo   \nbar\t\t\n", "foo\nbar", "x\ny", false);
    assert_eq!(a.tier, Tier::Whitespace);
    assert_eq!(a.text, "x\ny\n");
}

/// `test_trim_match_ignores_leading_and_trailing_whitespace` — deliberate divergence. Codex
/// trims both sides of every line independently, so lines indented 4 and 3 spaces match a
/// pattern indented 0 and 0. We accept only ONE uniform indentation shift, because a non-uniform
/// re-indent changes the meaning of Python and YAML; the refusal says the text is there apart
/// from whitespace and shows the region.
#[test]
fn codex_seek_trim_both_sides_is_refused_unless_the_shift_is_uniform() {
    let e = apply("    foo   \n   bar\t\n", "foo\nbar", "x\ny", false).unwrap_err();
    assert_eq!(e.kind(), "not_found");
    assert!(e.message().contains("apart from whitespace"), "{}", e.message());
    // Uniform (both lines 4 deeper): accepted and re-indented.
    let a = applied("    foo   \n    bar\t\n", "foo\nbar", "x\ny", false);
    assert_eq!((a.tier, a.text.as_str()), (Tier::Whitespace, "    x\n    y\n"));
}

/// `test_pattern_longer_than_input_returns_none`: no panic, a plain miss.
#[test]
fn codex_seek_pattern_longer_than_input_is_a_miss() {
    let e = apply("just one line", "too\nmany\nlines", "x", false).unwrap_err();
    assert_eq!(e.kind(), "not_found");
}

/// seek_sequence's last pass maps typographic punctuation to ASCII. We do not guess at
/// characters; the miss shows the closest region so the model copies the real ones.
#[test]
fn codex_seek_typographic_punctuation_is_not_guessed_but_shown() {
    let file = "fn main() {\n    println!(\u{201c}hello \u{2014} world\u{201d});\n}\n";
    let e = apply(file, "    println!(\"hello - world\");", "x", false).unwrap_err();
    let m = e.message();
    assert!(m.contains("Did you mean lines 2-2") && m.contains("\u{201c}hello"), "{m}");
}

// ---------------------------------------------------------------- codex text_file

/// Inserted lines take the file's ending; lines the edit did not touch keep theirs, including
/// a lone CR (`text_file.rs` keeps per-line endings).
#[test]
fn codex_text_file_untouched_lines_keep_their_endings() {
    let file = "one\r\ntwo\rthree\nfour\r\n";
    let a = applied(file, "four", "FOUR", false);
    assert_eq!(a.text, "one\r\ntwo\rthree\nFOUR\r\n");
    let crlf = "a\r\nb\r\n";
    assert_eq!(applied(crlf, "a", "a\nx", false).text, "a\r\nx\r\nb\r\n");
}

/// Deliberate divergence: Codex gives every line an ending ("updates have historically added a
/// trailing newline"). We never add bytes the model did not write, as Claude Code does not.
#[test]
fn codex_text_file_a_missing_final_newline_stays_missing() {
    assert_eq!(applied("x\ny", "y", "Y", false).text, "x\nY");
    assert_eq!(applied("x\ny", "x\ny", "a\nb", false).text, "a\nb");
}

// ---------------------------------------------------------------- flashagent fs_tools

/// `a_block_found_twice_by_the_tolerant_match_is_counted_not_lost`: a tolerant (EOL) match found
/// twice is an ambiguity with its count, never the first site; `replace_all` takes both.
#[test]
fn flash_tolerant_match_found_twice_is_counted() {
    let file = "x = 1\r\ny = 2\r\nz\r\nx = 1\r\ny = 2\r\n";
    let e = apply(file, "x = 1\ny = 2", "q", false).unwrap_err();
    assert!(matches!(e, EditError::Ambiguous { count: 2, tier: Tier::Eol, .. }), "{e:?}");
    assert!(e.message().contains("2 times"), "{}", e.message());
    assert_eq!(applied(file, "x = 1\ny = 2", "q", true).text, "q\r\nz\r\nq\r\n");
}

/// `an_edit_keeps_a_crlf_file_in_crlf`: an LF snippet and replacement on a CRLF file land as
/// CRLF; an LF file stays LF.
#[test]
fn flash_an_edit_keeps_a_crlf_file_in_crlf() {
    let a = applied("one\r\ntwo\r\nthree\r\n", "one\ntwo", "ONE\nTWO\nextra", false);
    assert_eq!(a.text, "ONE\r\nTWO\r\nextra\r\nthree\r\n");
    assert_eq!(applied("a\nb\n", "a", "a\nx", false).text, "a\nx\nb\n");
}

/// `replacements_are_counted_as_made` and `read_write_edit_roundtrip`'s refusals.
#[test]
fn flash_replacements_are_counted_and_refusals_named() {
    let a = applied("a a b", "a", "x", true);
    assert_eq!((a.text.as_str(), a.count), ("x x b", 2));
    let b = applied(&a.text, "b", "y", false);
    assert_eq!((b.text.as_str(), a.count + b.count), ("x x y", 3));
    let e = apply("a a", "a", "x", false).unwrap_err();
    assert!(e.message().contains("2 times"), "{}", e.message());
    let e = apply("x x x\n", "x", "y", false).unwrap_err();
    assert!(e.message().contains("3 times"), "{}", e.message());
    assert_eq!(apply("a", "", "x", false).unwrap_err().kind(), "empty_old");
    assert_eq!(apply("abc", "zzz", "y", false).unwrap_err().kind(), "not_found");
}

/// `losing_the_indentation_does_not_make_a_match_ambiguous`: two identical lines at different
/// depths are two sites; tolerance never turns into a guess.
#[test]
fn flash_lost_indentation_never_makes_a_guess() {
    let file = "fn a() {\n    call();\n}\nfn b() {\n    call();\n}\n";
    let e = apply(file, "call();", "call(1);", false).unwrap_err();
    assert!(matches!(e, EditError::Ambiguous { count: 2, .. }), "{e:?}");
    // At the whitespace tier too: an unindented two-line snippet against two indented copies.
    let file = "if a:\n    x = 1\n    y = 2\nif b:\n    x = 1\n    y = 2\n";
    let e = apply(file, "x = 1\ny = 2", "z = 3", false).unwrap_err();
    assert!(matches!(e, EditError::Ambiguous { count: 2, tier: Tier::Whitespace, .. }), "{e:?}");
}

/// `an_edit_lands_even_when_the_model_lost_the_indentation`: an under-indented block (one
/// uniform shift) still lands, re-indented. Divergence in the replacement: FlashAgent re-indents
/// the replacement's first line to the site and leaves the rest as sent; we apply the snippet's
/// shift to every line, so a replacement written at the snippet's depth lands at the file's.
#[test]
fn flash_an_under_indented_block_still_lands() {
    let file = "async def handle(self):\n        if media:\n            return await send(event)\n";
    let a = applied(file, "    if media:\n        return await send(event)", "    if media:\n        return None", false);
    assert_eq!(a.tier, Tier::Whitespace);
    let b = applied(&a.text, "async def handle(self):", "async def handle(self, q):", false);
    assert_eq!(b.text, "async def handle(self, q):\n        if media:\n            return None\n");
}

/// `a_missing_old_string_points_at_the_closest_line` and `an_invented_keyword_is_named_rather_than_guessed_at`.
#[test]
fn flash_a_near_miss_names_the_closest_line_and_the_word() {
    let file = "use std::io;\n\npub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n";
    let m = apply(file, "pub fn add(a: u32, b: u32) -> u32 {", "x", false).unwrap_err().message();
    assert!(m.contains("Did you mean lines 3-3") && m.contains("     3\tpub fn add(a: i32"), "{m}");
    assert!(m.contains("You wrote `u32` where the file's line 3 has `i32`."), "{m}");

    let file = "struct S {\n    plan: Vec<PlanStep>,\n}\n";
    let m = apply(file, "    pub plan: Vec<PlanStep>,", "    pub plan: Vec<Step>,", false).unwrap_err().message();
    assert!(m.contains("Did you mean lines 2-2") && m.contains("     2\t    plan: Vec<PlanStep>,"), "{m}");
    assert!(m.contains("You wrote `pub` on a line the file has without it (line 2)."), "{m}");
}

/// `a_different_line_is_not_reported_as_a_word_mistake`: no word hint for an unrelated line.
#[test]
fn flash_an_unrelated_line_gets_no_word_hint() {
    let file = "fn a() {\n    let x = compute(1, 2);\n}\n";
    let m = apply(file, "    let x = totally_different(9);", "y", false).unwrap_err().message();
    assert!(!m.contains("You wrote") && !m.contains("leaves out"), "{m}");
}

/// `a_byte_order_mark_neither_hides_line_one_nor_is_lost` (engine half; the bytes are in
/// `doc`): line one after a BOM matches, and the BOM survives the round trip.
#[test]
fn flash_a_bom_neither_hides_line_one_nor_is_lost() {
    let doc = TextDoc::decode("\u{feff}using A;\r\nusing B;\r\nclass C {}\r\n".as_bytes()).unwrap();
    let a = applied(&doc.text, "using A;\nusing B;", "using A;\nusing B;\nusing D;", false);
    assert_eq!(doc.encode_text(&a.text), "\u{feff}using A;\r\nusing B;\r\nusing D;\r\nclass C {}\r\n".as_bytes());
}

/// `edit_file_applies_edits_in_sequence` and `apply_edits_is_pure_and_validates`: a plan applies
/// its edits in order, in memory, and an empty plan changes nothing.
#[test]
fn flash_edits_apply_in_sequence_without_touching_disk() {
    use marlowe_os::workspace::LocalFs;
    let dir = std::env::temp_dir().join(format!("marlowe-donor-seq-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("a.txt");
    std::fs::write(&p, "one\n").unwrap();
    let e = |old: &str, new: &str| FileEdit { path: p.clone(), old: old.into(), new: new.into(), replace_all: false };
    let planned = plan(&LocalFs, &[e("one", "two"), e("two", "three")]).unwrap();
    assert_eq!(planned[0].text, "three\n");
    assert_eq!(std::fs::read_to_string(&p).unwrap(), "one\n", "planning writes nothing");
    commit(&LocalFs, &planned).unwrap();
    assert_eq!(std::fs::read_to_string(&p).unwrap(), "three\n");
    assert!(plan(&LocalFs, &[]).unwrap().is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}
