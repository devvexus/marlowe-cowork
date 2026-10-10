//! A `cmd.exe` parser for `cmd /c ...` lines and `.cmd`/`.bat` scripts: `"..."` quoting, `^`
//! escapes, `&` `&&` `||` `|` and newlines, `(...)` blocks, redirections, `%VAR%` / `!VAR!`
//! expansion (dynamic), `@` echo suppression, `rem` / `::` comments, and `set NAME=value`.

use super::{Cmd, Dialect, Redirect, Unparseable, Word};

pub(crate) fn parse(src: &str, _depth: u8) -> Result<Vec<Cmd>, Unparseable> {
    let s: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let mut cur = Cmd::new(Dialect::Cmd);
    let mut word: Option<Word> = None;
    let mut redirect: Option<&'static str> = None;
    let mut i = 0;

    fn finish_word(word: &mut Option<Word>, cur: &mut Cmd, redirect: &mut Option<&'static str>) {
        let Some(w) = word.take() else { return };
        if let Some(op) = redirect.take() {
            cur.redirects.push(Redirect { op, target: w });
            return;
        }
        if cur.words.is_empty() && !w.quoted {
            let lower = w.text.to_ascii_lowercase();
            if matches!(lower.as_str(), "if" | "else" | "do" | "not") {
                return;
            }
        }
        cur.words.push(w);
    }
    fn finish_cmd(cur: &mut Cmd, out: &mut Vec<Cmd>) {
        let mut cmd = std::mem::replace(cur, Cmd::new(Dialect::Cmd));
        if let Some(first) = cmd.words.first() {
            let lower = first.text.to_ascii_lowercase();
            if lower == "rem" || lower.starts_with("::") {
                return;
            }
            // `for %i in (...) do cmd`: the header runs nothing.
            if lower == "for" {
                cmd.words.clear();
            }
            // `set NAME=value` is an assignment.
            if lower == "set" && cmd.words.len() >= 2 {
                let rest: Vec<String> = cmd.words[1..].iter().map(|w| w.text.clone()).collect();
                let joined = rest.join(" ");
                let joined = joined.trim_start_matches("/a ").trim_start_matches("/A ").trim_start_matches("/p ").trim_start_matches("/P ");
                if let Some((name, value)) = joined.split_once('=') {
                    cmd.assigns.push((name.trim_matches('"').to_string(), Word::lit(value.trim_matches('"'))));
                    cmd.words.clear();
                }
            }
        }
        if !cmd.is_empty() {
            out.push(cmd);
        }
    }

    while i < s.len() {
        let c = s[i];
        match c {
            ' ' | '\t' | '\r' | ',' | ';' => {
                finish_word(&mut word, &mut cur, &mut redirect);
                i += 1;
            }
            '\n' => {
                finish_word(&mut word, &mut cur, &mut redirect);
                finish_cmd(&mut cur, &mut out);
                i += 1;
            }
            '^' => {
                if let Some(n) = s.get(i + 1) {
                    if *n != '\n' {
                        let w = word.get_or_insert_with(Word::default);
                        w.text.push(*n);
                        w.quoted = true;
                    }
                }
                i += 2;
            }
            '@' if word.is_none() && cur.words.is_empty() => i += 1,
            '"' => {
                let w = word.get_or_insert_with(Word::default);
                w.quoted = true;
                i += 1;
                while i < s.len() && s[i] != '"' {
                    if s[i] == '%' || s[i] == '!' {
                        w.dynamic |= expansion(&s, i);
                    }
                    w.text.push(s[i]);
                    i += 1;
                }
                if i >= s.len() {
                    return Err(Unparseable("unclosed double quote".into()));
                }
                i += 1;
            }
            '&' | '|' => {
                finish_word(&mut word, &mut cur, &mut redirect);
                if redirect.is_some() {
                    return Err(Unparseable("redirection with no target".into()));
                }
                i += if s.get(i + 1) == Some(&c) { 2 } else { 1 };
                finish_cmd(&mut cur, &mut out);
            }
            '(' if word.is_none() => {
                finish_cmd(&mut cur, &mut out);
                i += 1;
            }
            ')' => {
                finish_word(&mut word, &mut cur, &mut redirect);
                finish_cmd(&mut cur, &mut out);
                i += 1;
            }
            '>' | '<' => {
                if let Some(w) = &word {
                    if !w.quoted && w.text.len() == 1 && w.text.chars().all(|d| d.is_ascii_digit()) {
                        word = None;
                    } else {
                        finish_word(&mut word, &mut cur, &mut redirect);
                    }
                }
                let append = c == '>' && s.get(i + 1) == Some(&'>');
                i += if append { 2 } else { 1 };
                if s.get(i) == Some(&'&') {
                    i += 1;
                    while i < s.len() && s[i].is_ascii_digit() {
                        i += 1;
                    }
                    continue;
                }
                redirect = Some(match (c, append) {
                    ('>', true) => ">>",
                    ('>', false) => ">",
                    _ => "<",
                });
            }
            '%' | '!' => {
                let w = word.get_or_insert_with(Word::default);
                w.dynamic |= expansion(&s, i);
                w.text.push(c);
                i += 1;
            }
            _ => {
                word.get_or_insert_with(Word::default).text.push(c);
                i += 1;
            }
        }
    }
    finish_word(&mut word, &mut cur, &mut redirect);
    if redirect.is_some() {
        return Err(Unparseable("redirection with no target".into()));
    }
    finish_cmd(&mut cur, &mut out);
    Ok(out)
}

/// Whether `%NAME%` / `!NAME!` / `%1` starts at `i`.
fn expansion(s: &[char], i: usize) -> bool {
    let c = s[i];
    match s.get(i + 1) {
        Some(d) if c == '%' && (d.is_ascii_digit() || *d == '*' || *d == '~') => true,
        Some(_) => s[i + 1..].iter().take(64).position(|x| *x == c).is_some_and(|n| n > 0),
        None => false,
    }
}
