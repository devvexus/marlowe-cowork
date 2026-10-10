//! **Adversarial inputs for the never-raw rule**, from the review of the maths build.
//!
//! Each case is one way LaTeX source could have reached the screen: a finished reply whose `$`
//! never closed, a formula cut in half by a table's `|`, a table too wide to lay out, `\tag{…}`,
//! `\$` inside a span, a maths environment mid-paragraph, the user's own code. And the other side
//! of the rule: code and currency stay exactly as written.

use marlowe_surface::chrome::Ink;
use marlowe_surface::{markdown, Theme};
use ratatui::text::Line;

fn texts(ls: &[Line<'_>]) -> String {
    ls.iter()
        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

fn prose(s: &str, live: bool, w: usize) -> String {
    let t = Theme::default_truecolor();
    let base = Ink::Speech.style(&t);
    if live {
        texts(&markdown::render_prose_live(s, w, &t, base))
    } else {
        texts(&markdown::render_prose(s, w, &t, base))
    }
}

fn user(s: &str) -> String {
    let t = Theme::default_truecolor();
    texts(&markdown::render_text(s, 80, &t, t.normal(), false))
}

/// Inputs whose every frame, live or finished, must be free of `\`, `$` and source braces.
const NEVER_RAW: &[&str] = &[
    "Let $\\alpha be the rate and we go on",
    "The answer is $\\frac{a}{b",
    "- item $\\frac{1}{2}$\n- $$\\sum_{i=1}^n i$$",
    "> quote $\\sqrt[3]{x}$\n> $$\\int_0^1 f$$",
    "| a | b |\n|---|---|\n| $|x|$ | $\\frac{1}{2}$ |",
    "| a | b |\n|---|---|\n| x | $\\frac{1}{2",
    "| aaaaaaaa | bbbbbbbbb | ccccccccc | dddddddd | eeeeeeeee | ffffffff | gggggggg |\n|---|---|---|---|---|---|---|\n| $\\alpha$ | b | c | d | e | f | g |",
    "$$\n\\begin{pmatrix} a & b \\\\ c & d\n$$",
    "$$\\foo{x} + \\bar{y}$$",
    "$$\\tag{\\alpha} x = 1$$",
    "$$x = 1",
    "\\[ x = \\frac{1}{2}",
    "\\begin{aligned} a &= b \\\\ c &= d",
    "\\begin{array}{cc} a & b \\end{array}",
    "Inline \\(x^2",
    "We get \\begin{pmatrix} a & b \\\\ c & d \\end{pmatrix}. Next.",
    "We get \\begin{pmatrix} a & b",
    "so that $$\\frac{a}{b",
    "ends with $$",
    "Let $\\alpha$, then $\\beta, the rest.",
];

fn assert_clean(shown: &str, case: &str, mode: &str) {
    assert!(!shown.contains('\\'), "{mode}: a backslash reached the grid for {case:?}:\n{shown}");
    assert!(!shown.contains('$'), "{mode}: a dollar reached the grid for {case:?}:\n{shown}");
    assert!(!shown.contains(['{', '}']), "{mode}: a brace reached the grid for {case:?}:\n{shown}");
}

#[test]
fn no_adversarial_input_puts_source_on_screen_live_or_finished() {
    for case in NEVER_RAW {
        assert_clean(&prose(case, false, 60), case, "finished");
        assert_clean(&prose(case, false, 20), case, "finished, narrow");
        // Every prefix, as it streams.
        let chars: Vec<char> = case.chars().collect();
        for n in 1..=chars.len() {
            let partial: String = chars[..n].iter().collect();
            assert_clean(&prose(&partial, true, 60), case, &format!("live after {n} chars"));
        }
    }
}

#[test]
fn a_dollar_that_never_closed_before_a_command_is_maths_up_to_the_prose() {
    assert_eq!(prose("Let $\\alpha be the rate.", false, 60), "Let α be the rate.");
    assert_eq!(prose("The answer is $\\frac{a}{b", false, 60), "The answer is a/b");
    assert_eq!(markdown::maths_one_line("Title $\\frac{a}{b"), "Title a/b");
    assert_eq!(user("my $\\alpha"), "my α");
    // A dollar sign before a number or a word is still a dollar sign.
    assert_eq!(prose("It costs $5 and $10.", false, 60), "It costs $5 and $10.");
    assert_eq!(prose("Price \\$5 and \\$10 total.", false, 60), "Price $5 and $10 total.");
}

#[test]
fn a_bare_symbol_command_in_prose_is_typeset_but_a_path_is_not() {
    assert_eq!(prose("the rate \\eta is small, and x \\to y.", false, 60), "the rate η is small, and x → y.");
    let path = "See C:\\Users\\theta\\alpha and .\\beta and \\n.";
    assert_eq!(prose(path, false, 80), path);
    // Streaming: `\et` is held, never shown, until it is known to be `\eta`.
    for partial in ["the rate \\", "the rate \\e", "the rate \\et", "the rate \\eta"] {
        assert!(!prose(partial, true, 60).contains('\\'), "{partial:?}");
    }
}

#[test]
fn an_escaped_dollar_inside_a_span_is_not_its_closer() {
    assert_eq!(prose("Cost is $\\$5$ here.", false, 60), "Cost is $5 here.");
}

#[test]
fn a_bar_inside_a_formula_does_not_split_a_table_cell() {
    let shown = prose("| a | b |\n|---|---|\n| $|x|$ | $\\|v\\|$ |", false, 60);
    let last = shown.lines().last().unwrap();
    assert!(last.contains("|x|") && last.contains("‖v‖"), "{shown}");
}

#[test]
fn code_stays_exactly_as_written() {
    let fence = "```bash\necho $$ $x$ \\frac{a}{b}\n```";
    assert_eq!(prose(fence, false, 60), "  echo $$ $x$ \\frac{a}{b}");
    assert_eq!(prose("Use `echo $HOME$PATH` and `$x$`.", false, 60), "Use echo $HOME$PATH and $x$.");
    // The user's own code, in backticks or a fence, is not typeset either.
    assert_eq!(user("run `echo $A$B` now"), "run `echo $A$B` now");
    assert_eq!(user("```\nx=$(a)$(b)\n```"), "```\nx=$(a)$(b)\n```");
    // But a ```math fence in the user's text is maths.
    assert!(!user("```math\n\\frac{a}{b}\n```").contains('\\'));
}

#[test]
fn display_maths_mid_paragraph_is_held_on_one_row_while_it_streams() {
    // One row whatever has arrived: a height estimated from the partial rose and fell token by
    // token, and the pane jumped with it. Landing only grows the reply downward.
    let live = prose("so that $$\\frac{a}{b", true, 60);
    let done = prose("so that $$\\frac{a}{b}$$", false, 60);
    assert_eq!(live.lines().count(), 1, "live:\n{live}");
    assert!(done.lines().count() > 1, "done:\n{done}");
    assert!(live.contains('…'));
}
