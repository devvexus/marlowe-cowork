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
