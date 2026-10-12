//! `read` numbers lines; `edit` and `write` know about the prefix.

mod common;

use common::*;
use marlowe_exec::text::*;
use serde_json::json;

fn prefix(n: usize) -> String {
    format!("{n:>6}\t")
}

#[test]
fn numbering_and_stripping_are_inverses_including_crlf() {
    for text in ["a\nb\nc\n", "a\r\nb\r\n", "no newline", "  indented\n\tx\n", "7\tlooks numbered\n"] {
        assert_eq!(strip_line_numbers(&number_lines(text, 1)).as_deref(), Some(text));
        assert_eq!(strip_line_numbers(&number_lines(text, 500)).as_deref(), Some(text));
    }
}

#[test]
fn the_stripper_refuses_anything_that_is_not_this_harnesss_prefix() {
    assert_eq!(strip_line_numbers(""), None);
    assert_eq!(strip_line_numbers("plain\n"), None);
    assert_eq!(strip_line_numbers("1\tshort\n"), None, "not padded to the width");
    assert_eq!(strip_line_numbers("     1: colon\n"), None);
    assert_eq!(strip_line_numbers(&format!("{}a\n{}b\n", prefix(1), prefix(3))), None, "gap");
    assert_eq!(strip_line_numbers(&format!("{}a\nplain\n", prefix(1))), None, "unnumbered line");
}

#[test]
fn a_read_returns_absolute_line_numbers_not_window_relative_ones() {
    let ws = Ws::new("ln-abs");
    ws.put("f.txt", (1..=100).map(|i| format!("row {i}\n")).collect::<String>());
    let o = call(&ws.tools(), "read", json!({"path": "f.txt", "range": "50-52"}));
    let want = format!("{}row 50\n{}row 51\n{}row 52", prefix(50), prefix(51), prefix(52));
    assert!(o.content.starts_with(&want), "{}", o.content);
}

#[test]
fn a_grep_hit_can_be_turned_into_a_read_without_counting() {
    let ws = Ws::new("ln-grep");
    ws.put("src/a.rs", (1..=40).map(|i| if i == 33 { "NEEDLE here\n".into() } else { format!("filler {i}\n") }).collect::<String>());
    let g = call(&ws.tools(), "grep", json!({"pattern": "NEEDLE", "output_mode": "content"}));
    assert_eq!(g.content.lines().next(), Some("src/a.rs:33:NEEDLE here"), "{}", g.content);
    let r = call(&ws.tools(), "read", json!({"path": "src/a.rs", "range": "33-33"}));
    assert!(r.content.starts_with(&format!("{}NEEDLE here", prefix(33))));
}

#[test]
fn the_byte_ceiling_is_measured_on_the_numbered_body_and_the_notice_is_unnumbered() {
    let ws = Ws::new("ln-ceiling");
    ws.put("f.txt", (1..=1_500).map(|i| format!("{}\n", "w".repeat(60 + i % 7))).collect::<String>());
    let o = call(&ws.tools(), "read", json!({"path": "f.txt"}));
    let (body, notice) = o.content.split_once("\n\n[showing").expect("truncated by bytes");
    assert!(body.len() <= READ_WINDOW_BYTES, "{} > {}", body.len(), READ_WINDOW_BYTES);
    assert!(body.ends_with("w
"), "cut on a whole line");
    let last: usize = body.lines().last().unwrap().split('\t').next().unwrap().trim().parse().unwrap();
    // A window continues by offset, as in Claude Code.
    assert!(notice.contains(&format!("Continue with offset {}", last + 1)), "{notice}");
}

#[test]
fn a_file_that_fits_once_numbered_is_not_truncated() {
    let ws = Ws::new("ln-fits");
    ws.put("f.txt", "short\n".repeat(100));
    let o = call(&ws.tools(), "read", json!({"path": "f.txt"}));
    assert!(!o.content.contains("[showing"));
}

#[test]
fn a_replacing_that_still_carries_the_prefix_is_refused_by_name_and_located() {
    let ws = Ws::new("ln-prefixed");
    ws.put("f.txt", "one\ntwo\nthree\nfour\n");
    let pasted = format!("{}three\n{}four\n", prefix(3), prefix(4));
    let o = call(&ws.tools(), "edit", json!({"path": "f.txt", "replacing": pasted, "content": "x\n"}));
    assert!(o.is_error);
    assert!(o.content.contains("line-number prefix") && o.content.contains("starting at line 3"), "{}", o.content);
    assert_eq!(ws.get("f.txt"), "one\ntwo\nthree\nfour\n", "unchanged");
}

#[test]
fn a_file_that_really_contains_numbered_text_is_edited_normally() {
    let ws = Ws::new("ln-real");
    let doc = format!("{}alpha\n{}beta\n", prefix(1), prefix(2));
    ws.put("t.txt", &doc);
    let o = call(&ws.tools(), "edit", json!({"path": "t.txt", "replacing": format!("{}beta", prefix(2)), "content": format!("{}gamma", prefix(2))}));
    assert!(!o.is_error, "{}", o.content);
    assert_eq!(ws.get("t.txt"), format!("{}alpha\n{}gamma\n", prefix(1), prefix(2)));
}

#[test]
fn consecutiveness_separates_a_prefix_from_a_coincidence() {
    let ws = Ws::new("ln-coincidence");
    ws.put("f.txt", "keep\n     7\tlooks numbered\nend\n");
    let o = call(&ws.tools(), "edit", json!({"path": "f.txt", "replacing": "keep", "content": "     7\tone\n"}));
    assert!(!o.is_error, "a single numbered-looking line is plausible content: {}", o.content);
}

#[test]
fn edit_refuses_numbered_content_and_names_write() {
    let ws = Ws::new("ln-content");
    ws.put("f.txt", "alpha\nbeta\n");
    let numbered = format!("{}x\n{}y\n", prefix(1), prefix(2));
    let o = call(&ws.tools(), "edit", json!({"path": "f.txt", "replacing": "alpha", "content": numbered}));
    assert!(o.is_error && o.content.contains("`write`"), "{}", o.content);
    assert_eq!(ws.get("f.txt"), "alpha\nbeta\n");
}

#[test]
fn write_stores_numbered_text_and_says_that_it_did() {
    let ws = Ws::new("ln-write");
    let numbered = format!("{}x\n{}y\n", prefix(1), prefix(2));
    let o = call(&ws.tools(), "write", json!({"path": "t.txt", "content": numbered}));
    assert!(!o.is_error);
    assert!(o.content.contains("line-number prefix"), "{}", o.content);
    assert_eq!(ws.get("t.txt"), numbered);
}

#[test]
fn a_crlf_file_survives_a_read_and_the_snippet_it_returns_still_edits() {
    let ws = Ws::new("ln-crlf");
    ws.put("c.txt", "one\r\ntwo\r\nthree\r\n");
    let r = call(&ws.tools(), "read", json!({"path": "c.txt"}));
    // `read` shows CRLF as LF; the edit below still lands on the CRLF file.
    let snippet = strip_line_numbers(r.content.trim_end()).expect("numbered");
    assert!(snippet.contains("two\nthree"), "{snippet}");
    let snippet = "two\r\nthree";
    let o = call(&ws.tools(), "edit", json!({"path": "c.txt", "replacing": snippet, "content": "2\r\n3"}));
    assert!(!o.is_error, "{}", o.content);
    assert_eq!(ws.get("c.txt"), "one\r\n2\r\n3\r\n");
}

#[test]
fn an_lf_snippet_against_a_crlf_file_is_diagnosed_as_line_endings() {
    let ws = Ws::new("ln-lf-vs-crlf");
    ws.put("c.txt", "one\r\ntwo\r\nthree\r\n");
    // The line-ending tier applies it and says so; the file keeps its CRLF terminators.
    let o = call(&ws.tools(), "edit", json!({"path": "c.txt", "replacing": "one\ntwo", "content": "x"}));
    assert!(!o.is_error, "{}", o.content);
    assert!(o.content.contains("normalising line endings"), "{}", o.content);
    assert_eq!(ws.get("c.txt"), "x\r\nthree\r\n");
}

#[test]
fn the_named_sites_are_bounded_by_the_constant() {
    let ws = Ws::new("ln-sites");
    ws.put("f.txt", "dup\n".repeat(30));
    let o = call(&ws.tools(), "edit", json!({"path": "f.txt", "replacing": "dup", "content": "x"}));
    assert!(o.is_error);
    assert!(o.content.contains("occurs 30 times"));
    assert!(o.content.contains(&format!("and {} more", 30 - MAX_EDIT_SITES_NAMED)), "{}", o.content);
    assert_eq!(ws.get("f.txt"), "dup\n".repeat(30));
}
