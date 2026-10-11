use super::*;

/// Six captured runs (T32): the real output of rustc, `cargo test`, pytest, a Python traceback and
/// a Node throw from throwaway projects on this machine, and a pip install stream, each padded by
/// the program itself with the progress chatter a build produces (paths rewritten to `C:\work`).
const LOGS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/logs");

fn log(name: &str) -> String {
    let bytes = std::fs::read(format!("{LOGS}/{name}")).unwrap();
    clean(&String::from_utf8_lossy(&bytes))
}

#[test]
fn clean_resolves_redraws_and_colour() {
    assert_eq!(clean("a\r\nb\r\n"), "a\nb");
    assert_eq!(clean("  25%\r  50%\r 100%\nnext"), " 100%\nnext");
    assert_eq!(clean("\x1b[31merror\x1b[0m: bad \x1b]0;title\x07done"), "error: bad done");
    assert_eq!(clean("tail   \n\n"), "tail");
    assert_eq!(clean(""), "");
}

#[test]
fn output_that_fits_is_returned_untouched() {
    let s = summarize("one\ntwo\nthree", false, BUDGET);
    assert_eq!((s.text.as_str(), s.total_lines, s.cut), ("one\ntwo\nthree", 3, false));
    let s = summarize("", true, BUDGET);
    assert_eq!((s.text.as_str(), s.total_lines, s.cut), ("", 0, false));
}

#[test]
fn parity_t32_a_long_line_in_short_output_comes_back_whole() {
    // Claude Code returns any output under 30 KB as it is; a clipped line with no pointer and no
    // log on disk would lose the rest of it for good.
    let one = format!("{{\"data\":\"{}\"}}", "x".repeat(5_000));
    let s = summarize(&one, false, BUDGET);
    assert_eq!((s.text.as_str(), s.cut), (one.as_str(), false));
    let mixed = format!("head
{one}
tail");
    assert_eq!(summarize(&mixed, true, BUDGET).text, mixed);
}

#[test]
fn a_clipped_line_in_over_budget_output_is_marked_as_cut() {
    // Over the budget the lines are clipped, and then the log must be kept.
    let long = "y".repeat(2_000);
    let text = (0..20).map(|i| format!("{i} {long}")).collect::<Vec<_>>().join("
");
    let s = summarize(&text, false, BUDGET);
    assert!(s.cut && s.text.contains("more bytes]"), "{}", s.text.len());
}

#[test]
fn identical_runs_collapse() {
    let text = format!("start\n{}end", "same\n".repeat(50_000));
    let s = summarize(&text, false, BUDGET);
    assert!(s.cut && s.text.len() <= BUDGET, "{}", s.text.len());
    assert!(s.text.contains("[... the line above repeated 49999 more times]"), "{}", s.text);
    assert!(s.text.starts_with("start\nsame\n") && s.text.ends_with("end"));
}

/// A run's output with every claim a model needs to act on it.
fn must_keep(name: &str, failed: bool, keep: &[&str]) -> Summary {
    let full = log(name);
    assert!(full.len() > BUDGET, "{name} is a log that needs summarising ({} bytes)", full.len());
    let s = summarize(&full, failed, BUDGET);
    assert!(s.cut, "{name}");
    assert!(s.text.len() <= BUDGET, "{name}: {} bytes", s.text.len());
    assert_eq!(s.total_lines, full.split('\n').count(), "{name}");
    for k in keep {
        assert!(s.text.contains(k), "{name}: the summary lost {k:?}\n{}", s.text);
    }
    s
}

#[test]
fn parity_t32_six_captured_logs_keep_what_failed() {
    // 1. rustc: four errors at the top of 420 warnings. Every `error` line survives.
    let full = log("rustc_errors.log");
    let errors: Vec<&str> = full.lines().filter(|l| l.starts_with("error")).collect();
    assert!(errors.len() >= 5, "{errors:?}");
    let s = must_keep("rustc_errors.log", true, &[]);
    for e in &errors {
        assert!(s.text.contains(e), "lost {e}");
    }
    assert!(s.text.contains("expected `i32`, found `&str`"), "the first error's detail survives");

    // 2. cargo test: 1400 `ok` lines, two failure blocks, the totals.
    must_keep(
        "cargo_test_fail.log",
        true,
        &[
            "---- tests::parses_config_with_defaults stdout ----",
            "---- tests::totals_match stdout ----",
            "assertion `left == right` failed: config keys differ",
            "left: [\"alpha\", \"beta\", \"gamma\"]",
            "test result: FAILED. 1400 passed; 2 failed",
        ],
    );

    // 3. pytest: 700 passing lines, two failures, the short summary.
    must_keep(
        "pytest_fail.log",
        true,
        &[
            "FAILED test_many.py::test_api_response_shape",
            "FAILED test_many.py::test_division",
            "ZeroDivisionError: division by zero",
            "2 failed, 700 passed",
        ],
    );

    // 4. a Python traceback after 900 chatter lines.
    must_keep(
        "python_traceback.log",
        true,
        &["[loader] giving up", "[Previous line repeated 116 more times]", "RecursionError: maximum recursion depth exceeded"],
    );

    // 5. node: 1500 progress lines then an uncaught TypeError.
    must_keep("node_throw.log", true, &["TypeError: Unexpected token at offset 4", "Node.js v24.14.0"]);

    // 6. a successful install: head and tail kept, the warnings are the middle's blocks.
    let s = must_keep(
        "pip_install_ok.log",
        false,
        &["Collecting numpy==1.26.4", "Successfully installed numpy-1.26.4 dep_1-1.1.0 dep_2-1.2.0", "WARNING: The script tool_"],
    );
    assert!(s.text.contains("lines omitted"), "{}", s.text);
}

#[test]
fn a_failed_run_spends_the_middle_on_failures_a_successful_one_on_warnings() {
    let mut text = String::new();
    for i in 0..4_000 {
        text.push_str(&format!("step {i} running\n"));
        if i == 1_500 {
            text.push_str("warning: this API is deprecated\n");
        }
        if i == 2_500 {
            text.push_str("error: linker exploded\n  caused by: out of memory\n\nnext\n");
        }
    }
    let failed = summarize(&text, true, BUDGET);
    assert!(failed.text.contains("error: linker exploded") && failed.text.contains("caused by: out of memory"));
    assert!(!failed.text.contains("deprecated"), "a failure's middle is for errors");
    let ok = summarize(&text, false, BUDGET);
    assert!(ok.text.contains("this API is deprecated") && !ok.text.contains("linker exploded"));
}

#[test]
fn raw_cuts_only_the_middle() {
    let text: String = (0..5_000).map(|i| format!("line {i}\n")).collect();
    let text = text.trim_end().to_string();
    let s = middle_cut(&text, BUDGET);
    assert!(s.cut && s.text.len() <= BUDGET, "{}", s.text.len());
    assert!(s.text.starts_with("line 0\nline 1\n") && s.text.ends_with("line 4999"));
    assert!(s.text.contains("lines omitted"));
    assert_eq!(s.total_lines, 5_000);
    assert!(!middle_cut("short", BUDGET).cut);
    // One enormous line is still cut.
    let one = "x".repeat(200_000);
    let s = middle_cut(&one, BUDGET);
    assert!(s.cut && s.text.len() < BUDGET + 200, "{}", s.text.len());
}

#[test]
fn the_pointer_names_the_file_and_the_line_count() {
    let p = std::path::Path::new("C:\\logs\\j3.log");
    assert_eq!(pointer(p, 1_433), "[full output: C:/logs/j3.log, 1433 lines]");
}

#[test]
#[ignore = "prints the six summaries for a human to read"]
fn show_the_summaries() {
    for (name, failed) in [
        ("rustc_errors.log", true),
        ("cargo_test_fail.log", true),
        ("pytest_fail.log", true),
        ("python_traceback.log", true),
        ("node_throw.log", true),
        ("pip_install_ok.log", false),
    ] {
        let s = summarize(&log(name), failed, BUDGET);
        println!("=============== {name}: {} bytes of {} lines\n{}", s.text.len(), s.total_lines, s.text);
    }
}
