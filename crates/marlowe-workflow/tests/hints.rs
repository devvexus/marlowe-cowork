//! Python habits in a workflow script get a hint with the failing line quoted; the script is
//! never rewritten.

fn errors(src: &str) -> String {
    let d = marlowe_workflow::check("x.star", src).expect_err("the script should be refused");
    marlowe_workflow::validate::render(&d)
}

const HEAD: &str = "META = {\"name\": \"x\", \"description\": \"d\"}\n";

#[test]
fn each_habit_gets_its_hint_and_the_line() {
    let table: [(&str, &str, &str); 7] = [
        ("import os\ndef main(args):\n    return 1\n", "Starlark has no import", "import os"),
        ("def main(args):\n    x = args.get(\"a\")\n    if x is None:\n        return 1\n    return 2\n", "no `is` or `is not`", "if x is None:"),
        ("def main(args):\n    if x is not None:\n        return 1\n    return 2\n", "no `is` or `is not`", "if x is not None:"),
        ("def main(args):\n    if isinstance(args, dict):\n        return 1\n    return 2\n", "no isinstance", "if isinstance(args, dict):"),
        ("def main(args):\n    return f\"{args.name} here\"\n", "f-string may hold only a plain name", "return f\"{args.name} here\""),
        ("def main(args):\n    return \"one\n two\"\n", "cannot span lines", "return \"one"),
        ("def main(args):\n    return \"a\" \"b\"\n", "does not join neighbouring strings", "return \"a\" \"b\""),
    ];
    for (i, (body, hint, line)) in table.iter().enumerate() {
        let e = errors(&format!("{HEAD}{body}"));
        assert!(e.contains(hint), "case {i}: {e}");
        assert!(e.contains(&format!("`{line}`")), "case {i} quotes the line: {e}");
    }
}

#[test]
fn a_valid_script_with_those_words_in_strings_is_untouched() {
    let ok = format!("{HEAD}def main(args):\n    return \"import os is a habit; isinstance a b\" + f\"{{n}}\" if False else \"x\"\n");
    // Valid or not, no habit hint appears for words inside string literals.
    if let Err(d) = marlowe_workflow::check("x.star", &ok) {
        let e = marlowe_workflow::validate::render(&d);
        assert!(!e.contains("Starlark has no"), "{e}");
    }
}

#[test]
fn a_function_or_meta_given_twice_is_refused_with_both_lines() {
    let e = errors(&format!("{HEAD}def main(args):\n    return agent(\"work\")\n\ndef main(args): pass\n"));
    assert!(e.contains("`main` is defined twice (lines 2 and 5)"), "{e}");
    let e = errors(&format!("{HEAD}{HEAD}def main(args):\n    return 1\n"));
    assert!(e.contains("`META` is set twice (lines 1 and 2)"), "{e}");
    // Different functions, and a nested def of the same name, are fine.
    let ok = format!("{HEAD}def a():\n    def b():\n        return 1\n    return b()\n\ndef b():\n    return 2\n\ndef main(args):\n    return a() + b()\n");
    assert!(marlowe_workflow::check("x.star", &ok).is_ok());
}

// ---- parse errors name the real mistake (the unclosed bracket or string), not the symptom

fn one_error(src: &str) -> String {
    let d = marlowe_workflow::check("x.star", src).expect_err("the script should be refused");
    assert_eq!(d.len(), 1, "one problem, one message: {d:?}");
    d[0].to_string()
}

#[test]
fn an_unclosed_bracket_names_the_line_that_opened_it() {
    let src = format!(
        "{HEAD}def main(args):\n    rs = parallel([\n        lambda: agent(\n            \"one\", label=\"a\"\n        ,\n        lambda: agent(\"two\"),\n    ])\n    return rs\n"
    );
    let e = one_error(&src);
    assert!(e.contains("opened on line 4") && e.contains("put `)` first"), "{e}");
    assert!(e.contains("Balanced shape"), "{e}");
    assert!(!e.contains("\"%=\""), "no 50-token list: {e}");
}

#[test]
fn an_unclosed_triple_quote_says_where_it_opened() {
    let src = format!("{HEAD}def main(args):\n    r = agent(f\"\"\"Compile:\n a\", label=\"x\")\n    return r\n");
    let e = one_error(&src);
    assert!(e.contains("triple-quoted string opened on line 3 never closes"), "{e}");
}

#[test]
fn javascript_words_inside_strings_are_not_javascript() {
    // "bullet points" holds `let `, "=== Kimi ===" holds `===`; the failure is a plain missing paren.
    let src = format!("{HEAD}def main(args):\n    r = agent(\"- bullet points\n=== Kimi Data ===\"\n    return r\n");
    let e = one_error(&src);
    assert!(!e.contains("no `const`"), "{e}");
    let js = format!("{HEAD}def main(args):\n    const r = 1\n    return r\n");
    assert!(one_error(&js).contains("no `const`"));
}

#[test]
fn a_missing_indent_says_so() {
    let e = one_error(&format!("{HEAD}def main(args):\nlog(\"x\")\n    return 1\n"));
    assert!(e.contains("must be indented"), "{e}");
}

#[test]
fn no_main_is_one_message_with_a_skeleton() {
    let src = format!("{HEAD}x = agent(\"a\")\ny = parallel([lambda: agent(\"b\")])\n");
    let e = one_error(&src);
    assert!(e.contains("no `def main(args):`") && e.contains("return result") && e.contains("lines 2, 3"), "{e}");
}

#[test]
fn an_unindented_call_after_main_is_blamed_on_the_indent() {
    let src = format!("{HEAD}def main(args):\n    log(\"x\")\n\nreport = agent(\"write\")\nlog(\"done\")\n");
    let e = one_error(&src);
    assert!(e.contains("not indented") && e.contains("main's body ends at line 3") && e.contains("agent() on line 5"), "{e}");
}

#[test]
fn python_keywords_without_an_equivalent_get_a_hint() {
    let table = [
        ("while", "    while True:\n        pass\n    return 1\n", "no `while`"),
        ("try", "    try:\n        x = 1\n    except:\n        x = 2\n    return x\n", "no try/except"),
        ("class", "    class A:\n        pass\n    return 1\n", "no `class`"),
        ("with", "    with open(\"a\") as f:\n        pass\n    return 1\n", "no `with`"),
        ("assert", "    assert 1 == 1\n    return 1\n", "no `assert`"),
        ("else if", "    if 1 == 2:\n        return 0\n    else if 2 == 2:\n        return 1\n    return 2\n", "`elif`"),
    ];
    for (what, body, hint) in table {
        let e = errors(&format!("{HEAD}def main(args):\n{body}"));
        assert!(e.contains(hint), "{what}: {e}");
    }
}
