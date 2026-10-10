//! A PowerShell parser, enough to find every command a line runs.
//!
//! Handles `'...'` (with `''`), `"..."` (backtick escapes, `$var`, `$(...)`), here-strings
//! (`@'...'@`, `@"..."@`), backtick escapes and line continuation, `#` and `<# #>` comments,
//! `;` `|` `&&` `||` and newlines, the call operator (`& 'git' push`), dot-sourcing, `-and` /
//! `-or` between pipelines, `$(...)`, `@(...)`, `(...)` and `{...}` script blocks (parsed
//! recursively), redirections (`>`, `>>`, `2>`, `*>`, `2>&1`), and assignments
//! (`$env:GIT_DIR = 'x'`, `$x = git push`: the right-hand side is a command).

use super::{parse_at, Cmd, Dialect, Redirect, Unparseable, Word};

/// The cmdlet a built-in alias of Windows PowerShell stands for (`del` -> `remove-item`), lower
/// case, so a rule written for the cmdlet also reads its aliases. Some (`curl`, `wget`, `sc`)
/// are gone in pwsh 7, where they run other programs: only deny and ask rules use this.
pub fn cmdlet_of_alias(name: &str) -> Option<&'static str> {
    Some(match name {
        "ac" => "add-content",
        "cat" | "gc" | "type" => "get-content",
        "cd" | "chdir" | "sl" => "set-location",
        "clc" => "clear-content",
        "copy" | "cp" | "cpi" => "copy-item",
        "cpp" => "copy-itemproperty",
        "curl" | "iwr" | "wget" => "invoke-webrequest",
        "del" | "erase" | "rd" | "ri" | "rm" | "rmdir" => "remove-item",
        "dir" | "gci" | "ls" => "get-childitem",
        "echo" | "write" => "write-output",
        "epcsv" => "export-csv",
        "gcm" => "get-command",
        "gi" => "get-item",
        "gl" | "pwd" => "get-location",
        "gp" => "get-itemproperty",
        "icm" => "invoke-command",
        "iex" => "invoke-expression",
        "ii" => "invoke-item",
        "ipmo" => "import-module",
        "irm" => "invoke-restmethod",
        "kill" | "spps" => "stop-process",
        "mi" | "move" | "mv" => "move-item",
        "mp" => "move-itemproperty",
        "nal" => "new-alias",
        "ni" => "new-item",
        "popd" => "pop-location",
        "pushd" => "push-location",
        "r" | "ihy" => "invoke-history",
        "ren" | "rni" => "rename-item",
        "rp" => "remove-itemproperty",
        "rv" => "remove-variable",
        "sal" => "set-alias",
        "saps" | "start" => "start-process",
        "sc" => "set-content",
        "set" | "sv" => "set-variable",
        "si" => "set-item",
        "sls" => "select-string",
        "sp" => "set-itemproperty",
        "tee" => "tee-object",
        _ => return None,
    })
}

pub(crate) fn parse(src: &str, depth: u8) -> Result<Vec<Cmd>, Unparseable> {
    let mut p = Parser { s: src.chars().collect(), i: 0, depth, out: Vec::new(), cur: Cmd::new(Dialect::PowerShell), word: None, redirect: None };
    p.run()?;
    Ok(p.out)
}

#[derive(Default)]
struct WordBuf {
    text: String,
    dynamic: bool,
    quoted: bool,
}

struct Parser {
    s: Vec<char>,
    i: usize,
    depth: u8,
    out: Vec<Cmd>,
    cur: Cmd,
    word: Option<WordBuf>,
    redirect: Option<&'static str>,
}

impl Parser {
    fn peek(&self, off: usize) -> Option<char> {
        self.s.get(self.i + off).copied()
    }

    fn word(&mut self) -> &mut WordBuf {
        self.word.get_or_insert_with(WordBuf::default)
    }

    fn nested(&mut self, inner: &str) -> Result<(), Unparseable> {
        let cmds = parse_at(Dialect::PowerShell, inner, self.depth + 1)?;
        self.out.extend(cmds);
        Ok(())
    }

    fn run(&mut self) -> Result<(), Unparseable> {
        while let Some(c) = self.peek(0) {
            match c {
                ' ' | '\t' | '\r' | ',' => {
                    self.finish_word()?;
                    self.i += 1;
                }
                '\n' => {
                    self.finish_word()?;
                    self.i += 1;
                    self.finish_cmd()?;
                }
                '`' => match self.peek(1) {
                    Some('\n') => self.i += 2,
                    Some('\r') if self.peek(2) == Some('\n') => self.i += 3,
                    Some(n) => {
                        let w = self.word();
                        w.quoted = true;
                        w.text.push(n);
                        self.i += 2;
                    }
                    None => self.i += 1,
                },
                '#' if self.word.is_none() => {
                    while self.peek(0).is_some_and(|c| c != '\n') {
                        self.i += 1;
                    }
                }
                '<' if self.peek(1) == Some('#') => {
                    self.i += 2;
                    loop {
                        match (self.peek(0), self.peek(1)) {
                            (Some('#'), Some('>')) => {
                                self.i += 2;
                                break;
                            }
                            (Some(_), _) => self.i += 1,
                            (None, _) => return Err(Unparseable("unclosed <# comment".into())),
                        }
                    }
                }
                ';' => {
                    self.finish_word()?;
                    self.i += 1;
                    self.finish_cmd()?;
                }
                '|' => {
                    self.finish_word()?;
                    self.i += if self.peek(1) == Some('|') { 2 } else { 1 };
                    self.finish_cmd()?;
                }
                '&' => {
                    self.finish_word()?;
                    if self.peek(1) == Some('&') {
                        self.i += 2;
                        self.finish_cmd()?;
                    } else if self.cur.words.is_empty() {
                        // The call operator: the next word is the command to run.
                        self.cur.invoked = true;
                        self.i += 1;
                    } else {
                        // `cmd &`: a background job.
                        self.i += 1;
                        self.finish_cmd()?;
                    }
                }
                '>' => self.redirection()?,
                '<' => return Err(Unparseable("`<` is reserved in PowerShell".into())),
                '\'' => {
                    self.i += 1;
                    let lit = self.single_quoted()?;
                    let w = self.word();
                    w.quoted = true;
                    w.text.push_str(&lit);
                }
                '"' => {
                    self.i += 1;
                    self.double_quoted('"')?;
                }
                '@' if matches!(self.peek(1), Some('\'') | Some('"')) && self.here_string_start() => {
                    self.here_string()?;
                }
                '@' if self.peek(1) == Some('(') => {
                    self.i += 2;
                    let inner = self.balanced('(', ')')?;
                    self.nested(&inner)?;
                    let w = self.word();
                    w.dynamic = true;
                    w.text.push_str("@(...)");
                }
                '@' if self.peek(1) == Some('{') => {
                    // A hashtable literal: its values may be subexpressions.
                    self.i += 2;
                    let inner = self.balanced('{', '}')?;
                    self.nested(&inner)?;
                    let w = self.word();
                    w.dynamic = true;
                    w.text.push_str("@{...}");
                }
                '$' => self.dollar()?,
                '(' => {
                    self.i += 1;
                    let inner = self.balanced('(', ')')?;
                    self.nested(&inner)?;
                    let w = self.word();
                    w.dynamic = true;
                    w.text.push_str("(...)");
                }
                '{' => {
                    self.i += 1;
                    let inner = self.balanced('{', '}')?;
                    self.nested(&inner)?;
                    // A script block is a value; kept as a dynamic word so `& { ... }` and
                    // `Invoke-Command -ScriptBlock { ... }` stay visible as commands.
                    self.finish_word()?;
                    self.cur.words.push(Word { text: "{...}".into(), dynamic: true, quoted: false });
                }
                ')' | '}' => {
                    // Unbalanced closer at top level.
                    return Err(Unparseable(format!("unbalanced `{c}`")));
                }
                '[' if self.word.is_none() => {
                    // A type literal `[System.IO.File]` (possibly followed by `::Method(...)`).
                    self.i += 1;
                    let inner = self.balanced('[', ']')?;
                    let w = self.word();
                    w.text.push('[');
                    w.text.push_str(&inner);
                    w.text.push(']');
                }
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
        self.finish_cmd()
    }

    fn redirection(&mut self) -> Result<(), Unparseable> {
        // A stream number (`2>`, `*>`) right before the operator.
        if let Some(w) = &self.word {
            if !w.quoted && (w.text == "*" || (!w.text.is_empty() && w.text.chars().all(|d| d.is_ascii_digit()))) {
                self.word = None;
            } else {
                self.finish_word()?;
            }
        }
        self.i += 1;
        let mut op: &'static str = ">";
        if self.peek(0) == Some('>') {
            self.i += 1;
            op = ">>";
        }
        if self.peek(0) == Some('&') {
            // `2>&1`: a stream merge.
            self.i += 1;
            while self.peek(0).is_some_and(|c| c.is_ascii_digit()) {
                self.i += 1;
            }
            return Ok(());
        }
        self.redirect = Some(op);
        Ok(())
    }

    fn single_quoted(&mut self) -> Result<String, Unparseable> {
        let mut lit = String::new();
        loop {
            match self.peek(0) {
                None => return Err(Unparseable("unclosed single quote".into())),
                Some('\'') if self.peek(1) == Some('\'') => {
                    lit.push('\'');
                    self.i += 2;
                }
                Some('\'') => {
                    self.i += 1;
                    return Ok(lit);
                }
                Some(c) => {
                    lit.push(c);
                    self.i += 1;
                }
            }
        }
    }

    /// The body of an expandable string up to `close` (a `"`), or of a here-string when
    /// `close` is `@`.
    fn double_quoted(&mut self, close: char) -> Result<(), Unparseable> {
        self.word().quoted = true;
        loop {
            match self.peek(0) {
                None => return Err(Unparseable("unclosed double quote".into())),
                Some('"') if close == '"' && self.peek(1) == Some('"') => {
                    self.word().text.push('"');
                    self.i += 2;
                }
                Some('"') if close == '"' => {
                    self.i += 1;
                    return Ok(());
                }
                Some('\n') if close == '@' && self.peek(1) == Some('"') && self.peek(2) == Some('@') => {
                    self.i += 3;
                    return Ok(());
                }
                Some('`') => {
                    if let Some(n) = self.peek(1) {
                        self.word().text.push(n);
                    }
                    self.i += 2;
                }
                Some('$') => self.dollar()?,
                Some(c) => {
                    self.word().text.push(c);
                    self.i += 1;
                }
            }
        }
    }

    fn here_string_start(&self) -> bool {
        // `@'` or `@"` followed by the end of the line.
        let mut j = self.i + 2;
        while self.s.get(j).is_some_and(|c| *c == ' ' || *c == '\t' || *c == '\r') {
            j += 1;
        }
        matches!(self.s.get(j), Some('\n'))
    }

    fn here_string(&mut self) -> Result<(), Unparseable> {
        let quote = self.peek(1).unwrap_or('\'');
        self.i += 2;
        while self.peek(0).is_some_and(|c| c != '\n') {
            self.i += 1;
        }
        self.i += 1;
        if quote == '"' {
            return self.double_quoted('@');
        }
        let mut lit = String::new();
        loop {
            match (self.peek(0), self.peek(1), self.peek(2)) {
                (None, _, _) => return Err(Unparseable("unclosed here-string".into())),
                (Some('\n'), Some('\''), Some('@')) => {
                    self.i += 3;
                    break;
                }
                (Some(c), _, _) => {
                    lit.push(c);
                    self.i += 1;
                }
            }
        }
        let w = self.word();
        w.quoted = true;
        w.text.push_str(&lit);
        Ok(())
    }

    fn dollar(&mut self) -> Result<(), Unparseable> {
        match self.peek(1) {
            Some('(') => {
                self.i += 2;
                let inner = self.balanced('(', ')')?;
                self.nested(&inner)?;
                let w = self.word();
                w.dynamic = true;
                w.text.push_str("$(...)");
            }
            Some('{') => {
                self.i += 2;
                let inner = self.balanced('{', '}')?;
                let w = self.word();
                w.dynamic = true;
                w.text.push_str(&format!("${{{inner}}}"));
            }
            Some(n) if n.is_alphanumeric() || "_?^:$".contains(n) => {
                self.i += 1;
                let mut name = String::from("$");
                while let Some(c) = self.peek(0).filter(|c| c.is_alphanumeric() || *c == '_' || *c == ':' || *c == '?' || *c == '^') {
                    name.push(c);
                    self.i += 1;
                    if c == '?' || c == '^' {
                        break;
                    }
                }
                let w = self.word();
                w.dynamic = !name.eq_ignore_ascii_case("$null") && !name.eq_ignore_ascii_case("$true") && !name.eq_ignore_ascii_case("$false");
                w.text.push_str(&name);
            }
            _ => {
                self.word().text.push('$');
                self.i += 1;
            }
        }
        Ok(())
    }

    fn balanced(&mut self, open: char, close: char) -> Result<String, Unparseable> {
        let start = self.i;
        let mut level = 1usize;
        while let Some(c) = self.peek(0) {
            match c {
                '`' => self.i += 1,
                '\'' => {
                    self.i += 1;
                    while let Some(d) = self.peek(0) {
                        if d == '\'' {
                            if self.peek(1) == Some('\'') {
                                self.i += 1;
                            } else {
                                break;
                            }
                        }
                        self.i += 1;
                    }
                }
                '"' => {
                    self.i += 1;
                    while let Some(d) = self.peek(0) {
                        if d == '`' {
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
        let word = Word { text: w.text, dynamic: w.dynamic, quoted: w.quoted };
        if let Some(op) = self.redirect.take() {
            self.cur.redirects.push(Redirect { op, target: word });
            return Ok(());
        }
        if !word.quoted {
            let lower = word.text.to_ascii_lowercase();
            // Pipeline chain operators of the expression grammar: split conservatively.
            if matches!(lower.as_str(), "-and" | "-or" | "-xor") {
                return self.finish_cmd();
            }
            if self.cur.words.is_empty()
                && matches!(
                    lower.as_str(),
                    "if" | "elseif" | "else" | "foreach" | "for" | "while" | "do" | "until" | "switch" | "try" | "catch"
                        | "finally" | "trap" | "return" | "throw" | "exit" | "param" | "begin" | "process" | "end"
                        | "filter" | "-not" | "!"
                )
            {
                return Ok(());
            }
        }
        self.cur.words.push(word);
        Ok(())
    }

    fn finish_cmd(&mut self) -> Result<(), Unparseable> {
        if self.redirect.is_some() {
            return Err(Unparseable("redirection with no target".into()));
        }
        let mut cmd = std::mem::replace(&mut self.cur, Cmd::new(Dialect::PowerShell));
        // `function Name { ... }`: the body was parsed already.
        if cmd.words.first().is_some_and(|w| !w.quoted && w.text.eq_ignore_ascii_case("function")) {
            cmd.words.clear();
        }
        // Assignments: `$x = cmd`, `$env:X += 'y'`. The target is recorded; the right-hand side
        // is a command of its own.
        if cmd.words.len() >= 2
            && cmd.words[0].text.starts_with('$')
            && !cmd.words[1].quoted
            && matches!(cmd.words[1].text.as_str(), "=" | "+=" | "-=" | "*=" | "/=" | "??=")
        {
            let name = cmd.words[0].text.trim_start_matches('$').to_string();
            let rest: Vec<Word> = cmd.words.drain(2..).collect();
            cmd.words.clear();
            let value = rest.first().cloned().unwrap_or_default();
            cmd.assigns.push((name, value));
            // Only a bare word starts a command; a literal value does not run anything.
            if rest.first().is_some_and(|w| !w.quoted && !w.dynamic && !w.text.starts_with('[') && w.text.parse::<f64>().is_err()) {
                let mut run = Cmd::new(Dialect::PowerShell);
                run.words = rest;
                self.out.push(cmd);
                self.out.push(run);
                return Ok(());
            }
        }
        if cmd.is_empty() {
            return Ok(());
        }
        self.out.push(cmd);
        Ok(())
    }
}
