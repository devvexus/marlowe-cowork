//! A bash / POSIX sh parser, enough to find every simple command a line runs.
//!
//! Handles quoting (`'...'`, `"..."`, `$'...'`, backslash), comments, `;` `&` `&&` `||` `|` `|&`
//! and newlines, redirections (`>`, `>>`, `&>`, `2>&1`, `<`, `<<<`, here-docs), subshells and
//! groups, `$(...)`, backticks, `<(...)`/`>(...)` (parsed recursively), `$((...))`, `${...}`, and
//! the compound keywords (`if`/`while`/`for`/function bodies are walked; `case` is refused).
//! Assignments before a command (`GIT_DIR=x git ...`) are kept apart from its words.

use super::{parse_at, Cmd, Dialect, Redirect, Unparseable, Word};

pub(crate) fn parse(src: &str, depth: u8) -> Result<Vec<Cmd>, Unparseable> {
    let mut p = Parser { s: src.chars().collect(), i: 0, depth, out: Vec::new(), cur: Cmd::new(Dialect::Bash), word: None, redirect: None, heredocs: Vec::new(), heredoc_cmd: Vec::new(), empty_condition: false };
    p.run()?;
    Ok(p.out)
}

/// A word being built.
#[derive(Default)]
struct WordBuf {
    text: String,
    dynamic: bool,
    quoted: bool,
    /// The text before the first quote or expansion (for `NAME=value` detection).
    plain_prefix: Option<usize>,
}

impl WordBuf {
    fn mark_special(&mut self) {
        if self.plain_prefix.is_none() {
            self.plain_prefix = Some(self.text.len());
        }
    }
}

struct Pending {
    delim: String,
    strip_tabs: bool,
    quoted: bool,
}

struct Parser {
    s: Vec<char>,
    i: usize,
    depth: u8,
    out: Vec<Cmd>,
    cur: Cmd,
    word: Option<WordBuf>,
    /// The operator of a redirection whose target word comes next.
    redirect: Option<&'static str>,
    /// Here-docs whose bodies start after the current line.
    heredocs: Vec<Pending>,
    /// For each pending here-doc, the index in `out` (or `usize::MAX` for `cur`) of its command.
    heredoc_cmd: Vec<usize>,
    /// After `if`/`elif`/`while`/`until` with no command yet: `then`/`do` here is a syntax error.
    empty_condition: bool,
}

impl Parser {
    fn peek(&self, off: usize) -> Option<char> {
        self.s.get(self.i + off).copied()
    }

    fn word(&mut self) -> &mut WordBuf {
        self.word.get_or_insert_with(WordBuf::default)
    }

    fn run(&mut self) -> Result<(), Unparseable> {
        while let Some(c) = self.peek(0) {
            match c {
                ' ' | '\t' | '\r' => {
                    self.finish_word()?;
                    self.i += 1;
                }
                '\n' => {
                    self.finish_word()?;
                    self.i += 1;
                    self.finish_cmd()?;
                    self.read_heredocs()?;
                }
                '\\' => {
                    match self.peek(1) {
                        Some('\n') => self.i += 2,
                        Some(n) => {
                            self.word().quoted = true;
                            self.word().mark_special();
                            self.word().text.push(n);
                            self.i += 2;
                        }
                        None => {
                            self.word().text.push('\\');
                            self.i += 1;
                        }
                    }
                }
                '#' if self.word.is_none() => {
                    while self.peek(0).is_some_and(|c| c != '\n') {
                        self.i += 1;
                    }
                }
                ';' => {
                    self.finish_word()?;
                    self.i += if self.peek(1) == Some(';') { 2 } else { 1 };
                    self.finish_cmd()?;
                }
                '&' => {
                    if self.peek(1) == Some('>') {
                        // `&>file` / `&>>file`: both streams.
                        self.finish_word()?;
                        let append = self.peek(2) == Some('>');
                        self.i += if append { 3 } else { 2 };
                        self.redirect = Some(if append { ">>" } else { ">" });
                    } else {
                        self.finish_word()?;
                        self.i += if self.peek(1) == Some('&') { 2 } else { 1 };
                        self.finish_cmd()?;
                    }
                }
                '|' => {
                    self.finish_word()?;
                    self.i += if matches!(self.peek(1), Some('|') | Some('&')) { 2 } else { 1 };
                    self.finish_cmd()?;
                }
                '>' | '<' => self.redirection(c)?,
                '(' => {
                    if self.word.is_some() {
                        // `f()` function header or `=(` array: the paren ends the word.
                        self.finish_word()?;
                    }
                    if self.cur.words.is_empty() && self.cur.assigns.is_empty() {
                        // Subshell: its body is parsed inline as further commands.
                        self.i += 1;
                        self.finish_cmd()?;
                    } else if self.cur.assigns.is_empty() && self.peek(1) == Some(')') {
                        // `name()` starts a function definition; the body follows.
                        self.i += 2;
                        self.cur.words.clear();
                    } else {
                        // `arr=(a b c)` or similar: read the parenthesised list as words.
                        self.i += 1;
                        self.finish_cmd()?;
                    }
                }
                ')' => {
                    self.finish_word()?;
                    self.i += 1;
                    self.finish_cmd()?;
                }
                '\'' => {
                    self.i += 1;
                    let start = self.i;
                    while self.peek(0).is_some_and(|c| c != '\'') {
                        self.i += 1;
                    }
                    if self.peek(0).is_none() {
                        return Err(Unparseable("unclosed single quote".into()));
                    }
                    let lit: String = self.s[start..self.i].iter().collect();
                    self.i += 1;
                    let w = self.word();
                    w.quoted = true;
                    w.mark_special();
                    w.text.push_str(&lit);
                }
                '"' => {
                    self.i += 1;
                    self.double_quoted()?;
                }
                '`' => {
                    self.i += 1;
                    self.backtick()?;
                }
                '$' => self.dollar(false)?,
                _ => {
                    self.word().text.push(c);
                    self.i += 1;
                }
            }
        }
        self.finish_word()?;
        if self.redirect.is_some() {
            return Err(Unparseable("redirection with no target".into()));
        }
        self.finish_cmd()?;
        if !self.heredocs.is_empty() {
            // A here-doc on the last line with no body.
            self.heredocs.clear();
            self.heredoc_cmd.clear();
        }
        Ok(())
    }

    fn redirection(&mut self, c: char) -> Result<(), Unparseable> {
        // A word of digits right before the operator is a file descriptor.
        if let Some(w) = &self.word {
            if !w.quoted && !w.text.is_empty() && w.text.chars().all(|d| d.is_ascii_digit()) {
                self.word = None;
            } else {
                self.finish_word()?;
            }
        }
        if self.peek(1) == Some('(') {
            // Process substitution `<(cmd)` / `>(cmd)`.
            self.i += 2;
            let inner = self.balanced(')')?;
            self.out.extend(parse_at(Dialect::Bash, &inner, self.depth + 1)?);
            let w = self.word();
            w.dynamic = true;
            w.mark_special();
            return Ok(());
        }
        if c == '<' {
            match (self.peek(1), self.peek(2)) {
                (Some('<'), Some('<')) => {
                    // Here-string: the next word is stdin.
                    self.i += 3;
                    self.redirect = Some("<<<");
                }
                (Some('<'), _) => {
                    self.i += 2;
                    let strip = self.peek(0) == Some('-');
                    if strip {
                        self.i += 1;
                    }
                    self.redirect = Some(if strip { "<<-" } else { "<<" });
                }
                (Some('&'), _) | (Some('>'), _) => {
                    self.i += 2;
                    self.skip_fd_target();
                }
                _ => {
                    self.i += 1;
                    self.redirect = Some("<");
                }
            }
            return Ok(());
        }
        // `>`, `>>`, `>|`, `>&N`
        self.i += 1;
        let mut op: &'static str = ">";
        match self.peek(0) {
            Some('>') => {
                self.i += 1;
                op = ">>";
            }
            Some('|') => self.i += 1,
            Some('&') => {
                self.i += 1;
                // `>&2`, `>&-`: a descriptor, not a file. `>&file` is a file in bash.
                let save = self.i;
                self.skip_fd_target();
                if self.i != save {
                    return Ok(());
                }
            }
            _ => {}
        }
        self.redirect = Some(op);
        Ok(())
    }

    /// Skips a descriptor target (`2`, `-`) of `>&` / `<&`.
    fn skip_fd_target(&mut self) {
        while self.peek(0).is_some_and(|c| c.is_ascii_digit()) {
            self.i += 1;
        }
        if self.peek(0) == Some('-') {
            self.i += 1;
        }
    }

    fn double_quoted(&mut self) -> Result<(), Unparseable> {
        {
            let w = self.word();
            w.quoted = true;
            w.mark_special();
        }
        loop {
            match self.peek(0) {
                None => return Err(Unparseable("unclosed double quote".into())),
                Some('"') => {
                    self.i += 1;
                    return Ok(());
                }
                Some('\\') => {
                    match self.peek(1) {
                        Some(n @ ('"' | '\\' | '$' | '`')) => {
                            self.word().text.push(n);
                            self.i += 2;
                        }
                        Some('\n') => self.i += 2,
                        _ => {
                            self.word().text.push('\\');
                            self.i += 1;
                        }
                    }
                }
                Some('`') => {
                    self.i += 1;
                    self.backtick()?;
                }
                Some('$') => self.dollar(true)?,
                Some(c) => {
                    self.word().text.push(c);
                    self.i += 1;
                }
            }
        }
    }

    fn backtick(&mut self) -> Result<(), Unparseable> {
        let mut inner = String::new();
        loop {
            match self.peek(0) {
                None => return Err(Unparseable("unclosed backtick".into())),
                Some('`') => {
                    self.i += 1;
                    break;
                }
                Some('\\') if matches!(self.peek(1), Some('`' | '\\' | '$')) => {
                    inner.push(self.peek(1).unwrap_or('\\'));
                    self.i += 2;
                }
                Some(c) => {
                    inner.push(c);
                    self.i += 1;
                }
            }
        }
        self.out.extend(parse_at(Dialect::Bash, &inner, self.depth + 1)?);
        let w = self.word();
        w.dynamic = true;
        w.mark_special();
        w.text.push_str("$(...)");
        Ok(())
    }

    /// At a `$`: substitutions, parameter expansions, ANSI-C strings.
    fn dollar(&mut self, in_quotes: bool) -> Result<(), Unparseable> {
        match (self.peek(1), self.peek(2)) {
            (Some('('), Some('(')) => {
                self.i += 3;
                let inner = self.balanced(')')?;
                if self.peek(0) != Some(')') {
                    // `$( (cmd) )` written without a space: a command substitution after all.
                    self.out.extend(parse_at(Dialect::Bash, &format!("({inner}"), self.depth + 1)?);
                } else {
                    self.i += 1;
                    if inner.contains("$(") || inner.contains('`') {
                        return Err(Unparseable("command substitution inside arithmetic".into()));
                    }
                }
                let w = self.word();
                w.dynamic = true;
                w.mark_special();
                w.text.push_str("$((...))");
            }
            (Some('('), _) => {
                self.i += 2;
                let inner = self.balanced(')')?;
                self.out.extend(parse_at(Dialect::Bash, &inner, self.depth + 1)?);
                let w = self.word();
                w.dynamic = true;
                w.mark_special();
                w.text.push_str("$(...)");
            }
            (Some('{'), _) => {
                self.i += 2;
                let inner = self.balanced('}')?;
                if inner.contains("$(") || inner.contains('`') || inner.contains("<(") {
                    return Err(Unparseable("command substitution inside ${...}".into()));
                }
                let w = self.word();
                w.dynamic = true;
                w.mark_special();
                w.text.push_str(&format!("${{{inner}}}"));
            }
            (Some('\''), _) if !in_quotes => {
                self.i += 2;
                let mut lit = String::new();
                loop {
                    match self.peek(0) {
                        None => return Err(Unparseable("unclosed $'...'".into())),
                        Some('\'') => {
                            self.i += 1;
                            break;
                        }
                        Some('\\') => {
                            let n = self.peek(1).unwrap_or('\\');
                            lit.push(match n {
                                'n' => '\n',
                                't' => '\t',
                                _ => n,
                            });
                            // `\x..`, `\u....` and octal escapes are spelled characters we cannot
                            // see without decoding: refuse rather than guess.
                            if matches!(n, 'x' | 'u' | 'U' | '0'..='7' | 'c') {
                                return Err(Unparseable("escaped characters in $'...'".into()));
                            }
                            self.i += 2;
                        }
                        Some(c) => {
                            lit.push(c);
                            self.i += 1;
                        }
                    }
                }
                let w = self.word();
                w.quoted = true;
                w.mark_special();
                w.text.push_str(&lit);
            }
            (Some('"'), _) if !in_quotes => {
                self.i += 2;
                self.double_quoted()?;
            }
            (Some(n), _) if n.is_ascii_alphanumeric() || "_?#@*$!-".contains(n) => {
                self.i += 1;
                let mut name = String::from("$");
                if n.is_ascii_alphabetic() || n == '_' {
                    while let Some(c) = self.peek(0).filter(|c| c.is_ascii_alphanumeric() || *c == '_') {
                        name.push(c);
                        self.i += 1;
                    }
                } else {
                    name.push(n);
                    self.i += 1;
                }
                let w = self.word();
                w.dynamic = true;
                w.mark_special();
                w.text.push_str(&name);
            }
            _ => {
                self.word().text.push('$');
                self.i += 1;
            }
        }
        Ok(())
    }

    /// The text up to the matching `close` (quotes and nesting respected); leaves the cursor
    /// after it.
    fn balanced(&mut self, close: char) -> Result<String, Unparseable> {
        let open = if close == ')' { '(' } else { '{' };
        let start = self.i;
        let mut level = 1usize;
        while let Some(c) = self.peek(0) {
            match c {
                '\\' => self.i += 1,
                '\'' => {
                    self.i += 1;
                    while self.peek(0).is_some_and(|c| c != '\'') {
                        self.i += 1;
                    }
                }
                '"' => {
                    self.i += 1;
                    while let Some(d) = self.peek(0) {
                        if d == '\\' {
                            self.i += 1;
                        } else if d == '"' {
                            break;
                        }
                        self.i += 1;
                    }
                }
                c if c == open => level += 1,
                c if c == close => {
                    level -= 1;
                    if level == 0 {
                        let inner: String = self.s[start..self.i].iter().collect();
                        self.i += 1;
                        return Ok(inner);
                    }
                }
                _ => {}
            }
            self.i += 1;
        }
        Err(Unparseable(format!("unclosed `{open}`")))
    }

    fn finish_word(&mut self) -> Result<(), Unparseable> {
        let Some(w) = self.word.take() else { return Ok(()) };
        let word = Word { text: w.text.clone(), dynamic: w.dynamic, quoted: w.quoted };
        if let Some(op) = self.redirect.take() {
            match op {
                "<<" | "<<-" => {
                    self.heredocs.push(Pending { delim: word.text, strip_tabs: op == "<<-", quoted: w.quoted });
                    self.heredoc_cmd.push(usize::MAX);
                }
                "<<<" => {
                    let stdin = self.cur.stdin.get_or_insert_with(String::new);
                    stdin.push_str(&word.text);
                    stdin.push('\n');
                }
                _ => self.cur.redirects.push(Redirect { op, target: word }),
            }
            return Ok(());
        }
        // `NAME=value` before the command is an assignment.
        if self.cur.words.is_empty() {
            let plain = &w.text[..w.plain_prefix.unwrap_or(w.text.len())];
            if let Some(eq) = plain.find('=') {
                let name = plain[..eq].trim_end_matches('+');
                if !name.is_empty()
                    && name.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                    && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                {
                    let value = Word { text: w.text[eq + 1..].to_string(), dynamic: w.dynamic, quoted: w.quoted };
                    self.cur.assigns.push((name.to_string(), value));
                    return Ok(());
                }
            }
        }
        if !word.quoted && self.cur.words.is_empty() {
            match word.text.as_str() {
                // Keywords that start or continue a compound command: the next word is a command.
                "then" | "do" if self.empty_condition => return Err(Unparseable(format!("`{}` with no condition", word.text))),
                "if" | "elif" | "while" | "until" => {
                    self.empty_condition = true;
                    return Ok(());
                }
                "then" | "else" | "do" | "!" | "{" | "time" | "coproc" => {
                    return Ok(());
                }
                "fi" | "done" | "}" | "esac" => {
                    return Ok(());
                }
                "case" => return Err(Unparseable("case statement".into())),
                "select" => return Err(Unparseable("select statement".into())),
                _ => {}
            }
        }
        self.empty_condition = false;
        self.cur.words.push(word);
        Ok(())
    }

    fn finish_cmd(&mut self) -> Result<(), Unparseable> {
        if self.redirect.is_some() {
            return Err(Unparseable("redirection with no target".into()));
        }
        let mut cmd = std::mem::replace(&mut self.cur, Cmd::new(Dialect::Bash));
        // `for x in a b c` / `function name` headers run nothing themselves.
        if let Some(first) = cmd.words.first().filter(|w| !w.quoted) {
            match first.text.as_str() {
                "for" => cmd.words.clear(),
                "function" => cmd.words.clear(),
                "in" => cmd.words.clear(),
                _ => {}
            }
        }
        if cmd.is_empty() {
            return Ok(());
        }
        let idx = self.out.len();
        for slot in self.heredoc_cmd.iter_mut() {
            if *slot == usize::MAX {
                *slot = idx;
            }
        }
        self.out.push(cmd);
        Ok(())
    }

    /// After a newline: the bodies of pending here-docs, in order.
    fn read_heredocs(&mut self) -> Result<(), Unparseable> {
        let pending: Vec<Pending> = std::mem::take(&mut self.heredocs);
        let owners: Vec<usize> = std::mem::take(&mut self.heredoc_cmd);
        for (doc, owner) in pending.into_iter().zip(owners) {
            let mut body = String::new();
            loop {
                if self.peek(0).is_none() {
                    break;
                }
                let start = self.i;
                while self.peek(0).is_some_and(|c| c != '\n') {
                    self.i += 1;
                }
                let line: String = self.s[start..self.i].iter().collect();
                if self.peek(0) == Some('\n') {
                    self.i += 1;
                }
                let cmp = if doc.strip_tabs { line.trim_start_matches('\t') } else { line.as_str() };
                if cmp.trim_end_matches('\r') == doc.delim {
                    break;
                }
                body.push_str(cmp);
                body.push('\n');
            }
            if !doc.quoted && (body.contains("$(") || body.contains('`')) {
                // An unquoted here-doc runs its substitutions.
                let mut probe = String::from(": \"");
                probe.push_str(&body.replace('"', "\\\""));
                probe.push('"');
                self.out.extend(parse_at(Dialect::Bash, &probe, self.depth + 1)?);
            }
            if let Some(cmd) = self.out.get_mut(owner) {
                cmd.stdin.get_or_insert_with(String::new).push_str(&body);
            }
        }
        Ok(())
    }
}
