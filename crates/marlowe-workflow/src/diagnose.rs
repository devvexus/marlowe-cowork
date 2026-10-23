//! Turns a parser's complaint into the fix: the parser reports where it gave up, which is often
//! many lines after the real mistake (a `(` opened lines earlier and never closed). Everything
//! here reads the text only; the script is never rewritten.

/// A found bracket or quote mistake: the line to show and what is wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unbalanced {
    pub line: u32,
    pub message: String,
    /// Whether the mistake is a bracket (a balanced example helps), not a string.
    pub bracket: bool,
}

const EXAMPLE: &str = "Balanced shape: every `(` and `[` closes before the next statement, e.g.\n    xs = parallel([\n        lambda: agent(\"first\"),\n        lambda: agent(\"second\"),\n    ])";

fn closer(open: char) -> char {
    match open {
        '(' => ')',
        '[' => ']',
        _ => '}',
    }
}

/// The first bracket or quote mistake in `source`, scanning like the lexer: comments, single and
/// triple quoted strings (with `\` escapes), and `(` `[` `{` pairs.
pub fn unbalanced(source: &str) -> Option<Unbalanced> {
    let c: Vec<char> = source.chars().collect();
    let mut stack: Vec<(char, u32)> = Vec::new();
    let (mut i, mut line) = (0usize, 1u32);
    while i < c.len() {
        match c[i] {
            '\n' => line += 1,
            '#' => {
                while i < c.len() && c[i] != '\n' {
                    i += 1;
                }
                continue;
            }
            q @ ('"' | '\'') => {
                let opened = line;
                let triple = c.get(i + 1) == Some(&q) && c.get(i + 2) == Some(&q);
                if triple {
                    i += 3;
                    let mut closed = false;
                    while i < c.len() {
                        match c[i] {
                            '\\' => {
                                if c.get(i + 1) == Some(&'\n') {
                                    line += 1;
                                }
                                i += 2;
                                continue;
                            }
                            '\n' => line += 1,
                            x if x == q && c.get(i + 1) == Some(&q) && c.get(i + 2) == Some(&q) => {
                                closed = true;
                                i += 3;
                                break;
                            }
                            _ => {}
                        }
                        i += 1;
                    }
                    if !closed {
                        return Some(Unbalanced {
                            line: opened,
                            message: format!(
                                "the triple-quoted string opened on line {opened} never closes: end it with {q}{q}{q} (three quotes of the same kind; a single {q} does not close it)"
                            ),
                            bracket: false,
                        });
                    }
                    continue;
                }
                i += 1;
                let mut closed = false;
                while i < c.len() {
                    match c[i] {
                        '\\' => {
                            if c.get(i + 1) == Some(&'\n') {
                                line += 1;
                            }
                            i += 2;
                            continue;
                        }
                        '\n' => break,
                        x if x == q => {
                            closed = true;
                            i += 1;
                            break;
                        }
                        _ => {}
                    }
                    i += 1;
                }
                if !closed {
                    return Some(Unbalanced {
                        line: opened,
                        message: format!(
                            "the string opened on line {opened} is not closed on that line: add the closing {q}, write \\n for a line break inside a string, or use triple quotes for several lines"
                        ),
                        bracket: false,
                    });
                }
                continue;
            }
            o @ ('(' | '[' | '{') => stack.push((o, line)),
            x @ (')' | ']' | '}') => match stack.pop() {
                None => {
                    return Some(Unbalanced { line, message: format!("`{x}` on line {line} closes nothing: there is no open `{}` before it", match x { ')' => '(', ']' => '[', _ => '{' }), bracket: true });
                }
                Some((o, at)) if closer(o) != x => {
                    return Some(Unbalanced {
                        line,
                        message: format!("`{x}` on line {line} comes before the `{o}` opened on line {at} is closed: put `{}` first", closer(o)),
                        bracket: true,
                    });
                }
                Some(_) => {}
            },
            _ => {}
        }
        i += 1;
    }
    // The earliest one left open: everything after it sits inside it, so it is the one to close.
    stack.first().copied().map(|(o, at)| Unbalanced {
        line: at,
        message: format!("the `{o}` opened on line {at} is never closed: add `{}` where its expression ends", closer(o)),
        bracket: true,
    })
}

/// A parse message with its expected-token list cut down to what a person can act on.
pub fn shorten_expected(msg: &str) -> String {
    let Some(at) = msg.find("expected one of ") else { return msg.to_string() };
    let (head, list) = (&msg[..at], &msg[at + "expected one of ".len()..]);
    // The list ends at the first parenthesis the checker appended (` (team task scripts ...`).
    let (list, tail) = match list.find(" (") {
        Some(p) if list[..p].ends_with('"') => (&list[..p], &list[p..]),
        _ => (list, ""),
    };
    let items = list.matches('"').count() / 2;
    if list.contains("\"INDENT\"") && items <= 2 {
        return format!("{head}this line must be indented under the line above it (the block after a `:` is indented by four spaces){tail}");
    }
    if items > 8 {
        return format!("{head}the statement cannot continue here; expected an operator, `,`, a closing bracket or the end of the line (look just above for an unclosed `(` or `[`, or a missing `,`){tail}");
    }
    format!("{head}expected one of {list}{tail}")
}

/// Whether the parse message asks for an indented block.
pub fn wants_indent(msg: &str) -> bool {
    msg.contains("\"INDENT\"")
}

/// The parse error, completed: shortened token list, the bracket or quote mistake the parser may
/// have run past, and a balanced example for brackets. A mistake that begins after the line the
/// parser stopped on is a second problem, not the cause: it is left for the next check.
pub fn explain_parse(msg: &str, source: &str, line: Option<u32>) -> String {
    let mut out = shorten_expected(msg.trim_end());
    if let Some(u) = unbalanced(source).filter(|u| line.is_none_or(|l| u.line <= l)) {
        out.push_str(&format!("; {}", u.message));
        if u.bracket {
            out.push_str(&format!("\n{EXAMPLE}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_close_names_the_opener() {
        let u = unbalanced("x = parallel([\n  lambda: agent(\n    \"a\"\n])\n").unwrap();
        assert_eq!(u.line, 4);
        assert!(u.message.contains("`]` on line 4") && u.message.contains("`(` opened on line 2") && u.message.contains("put `)` first"), "{u:?}");
        let u = unbalanced("def main(args):\n    x = foo(1,\n    return x\n").unwrap();
        assert!(u.message.contains("`(` opened on line 2 is never closed"), "{u:?}");
    }

    #[test]
    fn strings_and_comments_hide_brackets() {
        assert_eq!(unbalanced("x = \"(\" + '[' + \"\"\"{\n(\"\"\"  # ) ]\ny = [1, (2)]\n"), None);
    }

    #[test]
    fn an_unclosed_triple_quote_names_its_opening_line() {
        let u = unbalanced("a = 1\nb = f\"\"\"text\nmore\", label=\"x\"\n").unwrap();
        assert_eq!(u.line, 2);
        assert!(!u.bracket && u.message.contains("opened on line 2 never closes"), "{u:?}");
        let u = unbalanced("a = \"one\nb = 2\n").unwrap();
        assert!(u.message.contains("opened on line 1 is not closed on that line"), "{u:?}");
    }

    #[test]
    fn the_token_list_is_cut() {
        let long = "unexpected keyword 'return' here, expected one of \"\\n\", \"!=\", \"%\", \"%=\", \"&\", \"&=\", \"(\", \")\", \"*\", \"*=\", \"+\"";
        let s = shorten_expected(long);
        assert!(s.contains("expected an operator") && !s.contains("\"%=\""), "{s}");
        let short = "unexpected symbol ']' here, expected one of \")\" or \",\"";
        assert_eq!(shorten_expected(short), short);
        let indent = "unexpected identifier 'log' here, expected one of \"\\n\" or \"INDENT\"";
        assert!(shorten_expected(indent).contains("must be indented"));
    }
}
