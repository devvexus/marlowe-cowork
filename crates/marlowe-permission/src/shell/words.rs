// Copyright 2025 OpenAI (codex), Apache-2.0. Derived from codex-rs/shell-command/src/bash.rs
// (`try_parse_word_only_commands_sequence`, `parse_shell_lc_plain_commands`) and
// command_safety/powershell_tree_sitter.rs at 6c0c6759d0617d7670826b61aca0a931bbb6db1f.
// Modified for Marlowe: the tree-sitter grammars are replaced by two small strict lexers with the
// same acceptance rules; the tests and the PowerShell fixture are Codex's.
// See http://www.apache.org/licenses/LICENSE-2.0 and THIRD_PARTY.md.

//! Literal command lines: the argv of every command when a line is nothing but plain words
//! joined by `&&`, `||`, `;`, `|` (and newlines). Anything the shell would expand, redirect,
//! group or compute (`$x`, `*`, `~`, `{a,b}`, `(...)`, `>`, assignments, comments) makes it
//! `None`. A line with a literal form runs exactly the words shown, which is what lets a policy
//! free a read-only line without trusting a lexical guess.

use super::paths::program;

/// The argv of every command of a bash line, when the line is word-only.
pub fn bash_words(src: &str) -> Option<Vec<Vec<String>>> {
    let s: Vec<char> = src.chars().collect();
    let mut i = 0;
    let mut cmds: Vec<Vec<String>> = Vec::new();
    let mut cur: Vec<String> = Vec::new();
    // After an operator that needs a command on its right (`&&`, `||`, `|`).
    let mut need_cmd = false;
    // The current command already ended with `;` (so a second `;` is an empty command).
    let mut just_semi = false;
    while i < s.len() {
        let c = s[i];
        match c {
            ' ' | '\t' | '\r' => i += 1,
            '\n' => {
                if !cur.is_empty() {
                    cmds.push(std::mem::take(&mut cur));
                }
                just_semi = false;
                i += 1;
            }
            ';' => {
                if cur.is_empty() {
                    return None;
                }
                cmds.push(std::mem::take(&mut cur));
                just_semi = true;
                i += 1;
                if s.get(i) == Some(&';') {
                    return None;
                }
            }
            '&' | '|' => {
                let double = s.get(i + 1) == Some(&c);
                if c == '&' && !double {
                    return None; // background `&`
                }
                if cur.is_empty() {
                    return None;
                }
                cmds.push(std::mem::take(&mut cur));
                need_cmd = true;
                just_semi = false;
                i += if double { 2 } else { 1 };
                continue;
            }
            _ => {
                let (word, next) = bash_word(&s, i, cur.is_empty())?;
                cur.push(word);
                need_cmd = false;
                just_semi = false;
                i = next;
                continue;
            }
        }
        if c != ' ' && c != '\t' && c != '\r' && need_cmd && c == '\n' {
            // `ls &&\n pwd` continues the list; fine.
        }
    }
    let _ = just_semi;
    if need_cmd {
        return None;
    }
    if !cur.is_empty() {
        cmds.push(cur);
    }
    if cmds.is_empty() {
        return None;
    }
    Some(cmds)
}

/// Characters that make a bare bash word something other than its spelling.
const BASH_SPECIAL: &[char] = &['{', '}', '*', '?', '[', ']', '\\', '~', '^', '#', '$', '`', '(', ')', '<', '>', '!'];

/// One word starting at `i`: bare text, `'...'` and `"..."` segments, concatenated. `first` is
/// the command name: it must be a single bare segment and may not look like an assignment.
fn bash_word(s: &[char], mut i: usize, first: bool) -> Option<(String, usize)> {
    let mut out = String::new();
    let mut segments = 0;
    let mut bare_only = true;
    let start = i;
    while i < s.len() {
        let c = s[i];
        match c {
            ' ' | '\t' | '\r' | '\n' | ';' | '&' | '|' => break,
            '\'' => {
                let end = s[i + 1..].iter().position(|&x| x == '\'')? + i + 1;
                out.extend(&s[i + 1..end]);
                i = end + 1;
                segments += 1;
                bare_only = false;
            }
            '"' => {
                let mut j = i + 1;
                loop {
                    let ch = *s.get(j)?;
                    match ch {
                        '"' => break,
                        '$' | '`' => return None,
                        '\\' => {
                            if matches!(s.get(j + 1), Some('$' | '`' | '"' | '\\' | '\n')) {
                                return None;
                            }
                            out.push('\\');
                        }
                        other => out.push(other),
                    }
                    j += 1;
                }
                i = j + 1;
                segments += 1;
                bare_only = false;
            }
            _ => {
                let begin = i;
                while i < s.len() && !matches!(s[i], ' ' | '\t' | '\r' | '\n' | ';' | '&' | '|' | '\'' | '"') {
                    if BASH_SPECIAL.contains(&s[i]) {
                        return None;
                    }
                    i += 1;
                }
                let text: String = s[begin..i].iter().collect();
                if begin == start && text.starts_with('=') {
                    return None;
                }
                out.push_str(&text);
                segments += 1;
            }
        }
    }
    if first {
        // tree-sitter's `command_name` is a single `word`; `NAME=value` is an assignment.
        if !bare_only || segments != 1 || out.contains('=') {
            return None;
        }
    }
    if segments > 1 && out.is_empty() {
        return None;
    }
    Some((out, i))
}

/// `bash -c '...'` / `bash -lc '...'` / `zsh -lc` / `sh -c`: the plain commands of the script.
pub fn shell_lc_plain_commands(argv: &[String]) -> Option<Vec<Vec<String>>> {
    let (_, script) = extract_bash_command(argv)?;
    bash_words(script)
}

/// The shell and script of `[bash|zsh|sh, -c|-lc, script]`.
pub fn extract_bash_command(argv: &[String]) -> Option<(&str, &str)> {
    let [shell, flag, script] = argv else { return None };
    if !matches!(flag.as_str(), "-lc" | "-c") || !matches!(program(shell).as_str(), "bash" | "zsh" | "sh") {
        return None;
    }
    Some((shell, script))
}

/// The argv of every command of a PowerShell line, when the line is word-only (Codex's
/// lowering: what the PowerShell AST would turn into constant command elements).
pub fn powershell_words(src: &str) -> Option<Vec<Vec<String>>> {
    let s: Vec<char> = src.chars().collect();
    let mut i = 0;
    let mut cmds: Vec<Vec<String>> = Vec::new();
    let mut cur: Vec<String> = Vec::new();
    let mut need_cmd = false;
    while i < s.len() {
        let c = s[i];
        match c {
            ' ' | '\t' | '\r' => i += 1,
            '\n' | ';' => {
                if c == ';' && cur.is_empty() {
                    return None;
                }
                if need_cmd && c == ';' {
                    return None;
                }
                if !cur.is_empty() {
                    cmds.push(std::mem::take(&mut cur));
                }
                i += 1;
            }
            '#' => {
                // A comment runs to the end of the line.
                while i < s.len() && s[i] != '\n' {
                    i += 1;
                }
            }
            '<' if s.get(i + 1) == Some(&'#') => {
                let rest: String = s[i + 2..].iter().collect();
                let end = rest.find("#>")?;
                i += 2 + rest[..end].chars().count() + 2;
            }
            '|' | '&' => {
                let double = s.get(i + 1) == Some(&c);
                if c == '&' && !double {
                    return None; // the call operator, or a background job
                }
                if cur.is_empty() {
                    return None;
                }
                cmds.push(std::mem::take(&mut cur));
                need_cmd = true;
                i += if double { 2 } else { 1 };
            }
            _ => {
                let (word, next) = ps_word(&s, i, cur.is_empty())?;
                cur.push(word);
                need_cmd = false;
                i = next;
            }
        }
    }
    if need_cmd {
        return None;
    }
    if !cur.is_empty() {
        cmds.push(cur);
    }
    if cmds.is_empty() {
        return None;
    }
    // `using`, `param`, `begin`/`process`/`end`, `trap`, `#requires` and keywords make it a
    // script, not a command list.
    for c in &cmds {
        let head = c[0].to_ascii_lowercase();
        if matches!(
            head.as_str(),
            "using" | "param" | "begin" | "process" | "end" | "clean" | "dynamicparam" | "trap" | "if" | "else" | "elseif"
                | "for" | "foreach" | "while" | "do" | "switch" | "function" | "filter" | "workflow" | "class" | "enum"
                | "try" | "catch" | "finally" | "return" | "throw" | "break" | "continue" | "exit" | "data"
        ) {
            return None;
        }
    }
    if src.lines().any(|l| l.trim_start().to_ascii_lowercase().starts_with("#requires")) {
        return None;
    }
    Some(cmds)
}

/// One PowerShell word at `i`: bare text (with backtick escapes), `'...'` (with `''`), `"..."`
/// without expansions. Numbers must be plain decimal integers; `-Name:value`, `--%`, `@...`,
/// `$...`, `(`, `,`, typographic dashes and quotes, and PowerShell 7 escapes are refused.
fn ps_word(s: &[char], mut i: usize, first: bool) -> Option<(String, usize)> {
    let mut out = String::new();
    let mut quoted = false;
    // Length of `out` before the first quote: the part PowerShell reads as parameter syntax.
    let mut bare_head: Option<usize> = None;
    let start = i;
    let ends = |c: char| matches!(c, ' ' | '\t' | '\r' | '\n' | ';' | '|');
    while i < s.len() && !ends(s[i]) {
        let c = s[i];
        match c {
            '\'' => {
                bare_head.get_or_insert(out.len());
                let mut j = i + 1;
                loop {
                    let ch = *s.get(j)?;
                    if ch == '\'' {
                        if s.get(j + 1) == Some(&'\'') {
                            out.push('\'');
                            j += 2;
                            continue;
                        }
                        break;
                    }
                    out.push(ch);
                    j += 1;
                }
                i = j + 1;
                quoted = true;
            }
            '"' => {
                bare_head.get_or_insert(out.len());
                let mut j = i + 1;
                loop {
                    let ch = *s.get(j)?;
                    match ch {
                        '"' => {
                            if s.get(j + 1) == Some(&'"') {
                                out.push('"');
                                j += 2;
                                continue;
                            }
                            break;
                        }
                        '$' => return None,
                        '`' => {
                            let e = *s.get(j + 1)?;
                            out.push(ps_escape(e)?);
                            j += 2;
                            continue;
                        }
                        '\u{201C}' | '\u{201D}' | '\u{201E}' => return None,
                        other => out.push(other),
                    }
                    j += 1;
                }
                i = j + 1;
                quoted = true;
            }
            '`' => {
                let e = *s.get(i + 1)?;
                out.push(ps_escape(e)?);
                i += 2;
            }
            '&' if s.get(i + 1) == Some(&'&') => break,
            '&' => return None,
            '$' | '@' | '(' | ')' | '{' | '}' | ',' | '<' | '>' | '[' if i == start || c != '[' => return None,
            '\u{2013}' | '\u{2014}' | '\u{2015}' | '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{2018}' | '\u{2019}' => {
                return None;
            }
            '#' if i == start => return None,
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    if i == start {
        return None;
    }
    if out.is_empty() {
        return None; // `''`
    }
    let head = &out[..bare_head.unwrap_or(out.len())];
    // `-Name:value` binds a parameter argument the lowering does not model.
    if head.starts_with('-') && !head.starts_with("--") && (head.contains(':') || head.contains('=')) {
        return None;
    }
    // `--short#;`: PowerShell reads a trailing `#` before a separator differently.
    if !quoted && out.ends_with('#') {
        return None;
    }
    if !quoted {
        let lower = out.to_ascii_lowercase();
        if lower == "--%" {
            return None;
        }
        if first && (out.starts_with('-') || out.starts_with('.') && out.len() == 1) {
            return None;
        }
        if looks_numeric(&out) && !plain_integer(&out) {
            return None;
        }
    }
    Some((out, i))
}

/// A backtick escape inside or outside double quotes: Windows PowerShell 5.1's set. `` `e ``,
/// `` `u{...} `` (PowerShell 7) and unknown escapes are refused rather than guessed.
fn ps_escape(e: char) -> Option<char> {
    Some(match e {
        '0' => '\0',
        'a' => '\u{7}',
        'b' => '\u{8}',
        'f' => '\u{c}',
        'n' => '\n',
        'r' => '\r',
        't' => '\t',
        'v' => '\u{b}',
        ' ' | '"' | '\'' | '`' | '$' | '#' | '|' | ';' | ',' | '&' | '(' | ')' | '{' | '}' | '@' | '<' | '>' => e,
        _ => return None,
    })
}

/// Starts like a PowerShell number literal (`1`, `-1`, `0x10`, `1kb`, `1.0`, `01`).
fn looks_numeric(t: &str) -> bool {
    let t = t.strip_prefix('-').unwrap_or(t);
    t.chars().next().is_some_and(|c| c.is_ascii_digit())
}

/// A canonical decimal integer (`100`, `-1`), the only numeric form whose spelling is its value.
fn plain_integer(t: &str) -> bool {
    let d = t.strip_prefix('-').unwrap_or(t);
    !d.is_empty() && d.chars().all(|c| c.is_ascii_digit()) && (d == "0" || !d.starts_with('0'))
}

#[cfg(test)]
mod tests {
    //! Codex's tests for `bash.rs` and the PowerShell lowering fixture, ported.
    use super::*;

    fn parse_seq(src: &str) -> Option<Vec<Vec<String>>> {
        bash_words(src)
    }

    fn v(items: &[&[&str]]) -> Vec<Vec<String>> {
        items.iter().map(|c| c.iter().map(|s| s.to_string()).collect()).collect()
    }

    #[test]
    fn accepts_single_simple_command() {
        assert_eq!(parse_seq("ls -1").unwrap(), v(&[&["ls", "-1"]]));
    }

    #[test]
    fn accepts_multiple_commands_with_allowed_operators() {
        let cmds = parse_seq("ls && pwd; echo 'hi there' | wc -l").unwrap();
        assert_eq!(cmds, v(&[&["ls"], &["pwd"], &["echo", "hi there"], &["wc", "-l"]]));
    }

    #[test]
    fn extracts_double_and_single_quoted_strings() {
        assert_eq!(parse_seq("echo \"hello world\"").unwrap(), v(&[&["echo", "hello world"]]));
        assert_eq!(parse_seq("echo 'hi there'").unwrap(), v(&[&["echo", "hi there"]]));
    }

    #[test]
    fn accepts_double_quoted_strings_with_newlines() {
        assert_eq!(parse_seq("git commit -m \"line1\nline2\"").unwrap(), v(&[&["git", "commit", "-m", "line1\nline2"]]));
    }

    #[test]
    fn accepts_mixed_quote_concatenation() {
        assert_eq!(parse_seq(r#"echo "/usr"'/'"local"/bin"#).unwrap(), v(&[&["echo", "/usr/local/bin"]]));
        assert_eq!(parse_seq(r#"echo '/usr'"/"'local'/bin"#).unwrap(), v(&[&["echo", "/usr/local/bin"]]));
    }

    #[test]
    fn rejects_double_quoted_strings_with_expansions() {
        assert!(parse_seq(r#"echo "hi ${USER}""#).is_none());
        assert!(parse_seq(r#"echo "$HOME""#).is_none());
    }

    #[test]
    fn accepts_numbers_as_words() {
        assert_eq!(parse_seq("echo 123 456").unwrap(), v(&[&["echo", "123", "456"]]));
    }

    #[test]
    fn rejects_parentheses_and_subshells() {
        assert!(parse_seq("(ls)").is_none());
        assert!(parse_seq("ls || (pwd && echo hi)").is_none());
    }

    #[test]
    fn rejects_redirections_and_unsupported_operators() {
        assert!(parse_seq("ls > out.txt").is_none());
        assert!(parse_seq("echo hi & echo bye").is_none());
    }

    #[test]
    fn rejects_command_and_process_substitutions_and_expansions() {
        assert!(parse_seq("echo $(pwd)").is_none());
        assert!(parse_seq("echo `pwd`").is_none());
        assert!(parse_seq("echo $HOME").is_none());
        assert!(parse_seq("echo \"hi $USER\"").is_none());
    }

    #[test]
    fn rejects_runtime_expansion_in_plain_words() {
        for script in [
            "find . -{delete,print}",
            "rg --pre{=,=sh} pattern payload.sh",
            "find . -del*",
            "find . -delet?",
            "find . -delet[e]",
            r"find . -de\lete",
            "echo ~",
            "echo ~HOME",
            "echo HEAD~1",
            "echo HEAD^",
            "echo file~",
            "echo =sh",
            "echo foo^bar",
            "echo foo#bar",
            "l* -l",
        ] {
            for shell in ["bash", "zsh"] {
                for flag in ["-c", "-lc"] {
                    let command = [shell, flag, script].map(str::to_owned);
                    assert_eq!(shell_lc_plain_commands(&command), None, "{command:?}");
                }
            }
        }
    }

    #[test]
    fn preserves_quoted_literals() {
        for (script, expected) in [
            (r#"rg -g"*.py" pattern"#, vec!["rg", "-g*.py", "pattern"]),
            (r#"echo "\n""#, vec!["echo", r"\n"]),
            (r#"echo "~HOME" 'HEAD~1' "HEAD^" 'foo#bar' "=sh" 'file~'"#, vec!["echo", "~HOME", "HEAD~1", "HEAD^", "foo#bar", "=sh", "file~"]),
            (r#"echo -"{a,b}" '*?[]~^#=\\'"#, vec!["echo", "-{a,b}", r"*?[]~^#=\\"]),
        ] {
            let expected = vec![expected.into_iter().map(str::to_owned).collect::<Vec<_>>()];
            assert_eq!(bash_words(script), Some(expected), "{script:?}");
        }
    }

    #[test]
    fn rejects_double_quoted_escapes() {
        for script in [r#"echo "\$HOME\`\"\\\n""#, "find . \"-de\\\nlete\"", r#"echo "\\""#] {
            assert_eq!(parse_seq(script), None, "{script:?}");
            for shell in ["bash", "zsh"] {
                let command = [shell, "-lc", script].map(str::to_owned);
                assert_eq!(shell_lc_plain_commands(&command), None, "{command:?}");
            }
        }
    }

    #[test]
    fn rejects_variable_assignment_prefix() {
        assert!(parse_seq("FOO=bar ls").is_none());
    }

    #[test]
    fn rejects_trailing_operator_parse_error() {
        assert!(parse_seq("ls &&").is_none());
    }

    #[test]
    fn rejects_empty_command_position_with_leading_operator() {
        assert!(parse_seq("&& ls").is_none());
    }

    #[test]
    fn rejects_empty_command_position_with_double_separator() {
        assert!(parse_seq("ls ;; pwd").is_none());
    }

    #[test]
    fn rejects_empty_command_position_with_empty_pipeline_segment() {
        assert!(parse_seq("ls | | wc").is_none());
    }

    #[test]
    fn parse_zsh_lc_plain_commands() {
        let command = vec!["zsh".to_string(), "-lc".to_string(), "ls".to_string()];
        assert_eq!(shell_lc_plain_commands(&command).unwrap(), v(&[&["ls"]]));
    }

    #[test]
    fn accepts_concatenated_flag_and_value() {
        assert_eq!(parse_seq("rg -n \"foo\" -g\"*.py\"").unwrap(), v(&[&["rg", "-n", "foo", "-g*.py"]]));
    }

    #[test]
    fn accepts_concatenated_flag_with_single_quotes() {
        assert_eq!(parse_seq("grep -n 'pattern' -g'*.txt'").unwrap(), v(&[&["grep", "-n", "pattern", "-g*.txt"]]));
    }

    #[test]
    fn rejects_concatenation_with_variable_substitution() {
        assert!(parse_seq("rg -g\"$VAR\" pattern").is_none());
        assert!(parse_seq("rg -g\"${VAR}\" pattern").is_none());
    }

    #[test]
    fn rejects_concatenation_with_command_substitution() {
        assert!(parse_seq("rg -g\"$(pwd)\" pattern").is_none());
        assert!(parse_seq("rg -g\"$(echo '*.py')\" pattern").is_none());
    }

    #[derive(serde::Deserialize)]
    struct FixtureCase {
        name: String,
        script: String,
        expected: Option<Vec<Vec<String>>>,
    }

    /// Codex's PowerShell lowering fixture (outputs captured from the PowerShell 7 AST parser;
    /// rare forms recorded as unsupported).
    #[test]
    fn lowers_compact_literal_fixture() {
        let cases: Vec<FixtureCase> = serde_json::from_str(include_str!("fixtures/codex_powershell_lowering.json")).expect("fixture");
        assert_eq!(cases.len(), 68);
        let mut bad = Vec::new();
        for case in cases {
            let got = powershell_words(&case.script);
            if got != case.expected {
                bad.push(format!("{}: {:?} -> {got:?}, want {:?}", case.name, case.script, case.expected));
            }
        }
        assert!(bad.is_empty(), "{}", bad.join("\n"));
    }

    #[test]
    fn rejects_requires_directives() {
        assert_eq!(powershell_words("#requires -Modules Evil\nGet-Location"), None);
    }
}
