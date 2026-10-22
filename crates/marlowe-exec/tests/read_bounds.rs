//! `read` is bounded: ranges cannot overflow or unwind, big files are capped and say so.

mod common;

use common::*;
use marlowe_exec::text::MAX_READ_BYTES;
use serde_json::json;

fn numbered(n: usize) -> String {
    (1..=n).map(|i| format!("line {i}\n")).collect()
}

#[test]
fn the_range_that_overflowed_returns_a_result() {
    let ws = Ws::new("rb-overflow");
    ws.put("f.txt", numbered(5));
    let t = ws.tools();
    let o = call(&t, "read", json!({"path": "f.txt", "range": "1-18446744073709551615"}));
    assert!(!o.is_error, "{}", o.content);
    assert!(o.content.contains("line 5"));
}

#[test]
fn no_range_argument_can_unwind_the_executor() {
    let ws = Ws::new("rb-nopanic");
    ws.put("f.txt", numbered(5));
    let t = ws.tools();
    for range in ["", "-", "a-b", "0-0", "5-1", "2", "-3", "1-", "99999999999999999999-1", "1-2-3", "  ", "٣-٤"] {
        let o = call(&t, "read", json!({"path": "f.txt", "range": range}));
        assert!(o.is_error, "range {range:?} should be refused: {}", o.content);
    }
    let o = call(&t, "read", json!({"path": "f.txt", "range": 7}));
    assert!(!o.is_error, "a non-string range is ignored like an absent one");
}

#[test]
fn an_inverted_range_is_refused_with_the_swap() {
    let ws = Ws::new("rb-inverted");
    ws.put("f.txt", numbered(50));
    let o = call(&ws.tools(), "read", json!({"path": "f.txt", "range": "40-20"}));
    assert!(o.is_error);
    assert!(o.content.contains("\"20-40\""), "{}", o.content);
}

#[test]
fn an_ordinary_range_selects_the_lines_it_names() {
    let ws = Ws::new("rb-range");
    ws.put("f.txt", numbered(30));
    let o = call(&ws.tools(), "read", json!({"path": "f.txt", "range": "10-12"}));
    assert!(!o.is_error);
    assert!(o.content.contains("    10\tline 10"));
    assert!(o.content.contains("    12\tline 12"));
    assert!(!o.content.contains("line 9\n") && !o.content.contains("line 13"));
    assert_eq!(o.summary.split(' ').next(), Some("3"));
}

#[test]
fn a_range_past_the_end_says_how_long_the_file_is() {
    let ws = Ws::new("rb-past");
    ws.put("f.txt", numbered(10));
    let o = call(&ws.tools(), "read", json!({"path": "f.txt", "range": "500-600"}));
    assert!(o.is_error);
    assert!(o.content.contains("the file has 10"), "{}", o.content);
}

/// A file past the old 4 MB prefix is read by window and its tail is reachable (track A's review
/// finding): a range returns exactly its lines, an offset at the last line returns it, and the
/// default window continues by offset.
#[test]
fn an_oversized_file_is_windowed_and_its_tail_is_reachable() {
    let ws = Ws::new("rb-big");
    let line = "x".repeat(99) + "\n";
    let n = (MAX_READ_BYTES as usize / line.len()) + 100;
    ws.put("big.txt", line.repeat(n));
    let numbered = |content: &str| -> Vec<usize> {
        content.lines().filter_map(|l| l.split_once('\t')).filter_map(|(k, _)| k.trim().parse().ok()).collect()
    };
    let o = call(&ws.tools(), "read", json!({"path": "big.txt", "range": "1-3"}));
    assert!(!o.is_error, "{}", o.content);
    assert!(!o.content.contains("stopped reading"), "{}", o.content);
    assert_eq!(numbered(&o.content), [1, 2, 3]);
    let o = call(&ws.tools(), "read", json!({"path": "big.txt", "offset": n}));
    assert!(!o.is_error, "{}", o.content);
    assert_eq!(numbered(&o.content), [n], "the last line is reachable");
    let o = call(&ws.tools(), "read", json!({"path": "big.txt"}));
    assert!(o.content.contains("Continue with offset"), "windowing notice");
}

#[test]
fn a_file_at_exactly_the_cap_is_not_reported_as_truncated() {
    let ws = Ws::new("rb-exact");
    let mut body = "y".repeat(99) + "\n";
    body = body.repeat(MAX_READ_BYTES as usize / body.len());
    body.push_str(&"z".repeat(MAX_READ_BYTES as usize - body.len()));
    assert_eq!(body.len() as u64, MAX_READ_BYTES);
    ws.put("exact.txt", &body);
    let o = call(&ws.tools(), "read", json!({"path": "exact.txt", "range": "1-2"}));
    assert!(!o.content.contains("stopped reading"), "{}", &o.content[..200.min(o.content.len())]);
}

#[test]
fn a_binary_file_is_named_rather_than_decoded_or_refused() {
    let ws = Ws::new("rb-bin");
    ws.put("b.bin", [0xff, 0xfe, 0x00, 0x80, 0x81, 0xc3, 0x28]);
    let o = call(&ws.tools(), "read", json!({"path": "b.bin"}));
    assert!(!o.is_error);
    assert!(o.content.contains("not UTF-8"), "{}", o.content);
    assert!(!o.content.contains('\u{fffd}'));
}

#[test]
fn a_text_file_is_returned_whole_and_a_missing_one_is_an_error() {
    let ws = Ws::new("rb-whole");
    ws.put("f.txt", "alpha\nbeta\n");
    let o = call(&ws.tools(), "read", json!({"path": "f.txt"}));
    assert!(!o.is_error);
    assert!(o.content.contains("     1\talpha") && o.content.contains("     2\tbeta"));
    assert!(call(&ws.tools(), "read", json!({"path": "nope.txt"})).is_error);
    assert!(call(&ws.tools(), "read", json!({})).is_error);
}

#[test]
fn an_empty_file_reads_as_zero_lines() {
    let ws = Ws::new("rb-empty");
    ws.put("e.txt", "");
    let o = call(&ws.tools(), "read", json!({"path": "e.txt"}));
    assert!(!o.is_error);
    assert!(o.summary.starts_with("0 lines"), "{}", o.summary);
}

#[test]
fn a_large_file_notice_states_the_whole_file_cost_against_the_window() {
    let ws = Ws::new("rb-cost");
    ws.put("long.txt", numbered(6_000));
    let t = ws.tools().with_context_tokens(32_768);
    let o = call(&t, "read", json!({"path": "long.txt"}));
    assert!(o.content.contains("The whole file is about"), "{}", &o.content[o.content.len() - 300..]);
    let t = ws.tools().with_context_tokens(2_000_000);
    let o = call(&t, "read", json!({"path": "long.txt"}));
    // The 32 KB ceiling still cuts a 6,000-line file on a huge window, so the cost is stated
    // against that window too.
    assert!(o.content.contains("your window is 2000000 tokens"), "{}", &o.content[o.content.len() - 300..]);
}
