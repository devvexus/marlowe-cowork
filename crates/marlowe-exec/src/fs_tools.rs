//! read, write, edit, glob, grep.

use std::io::Read;
use std::path::{Path, PathBuf};

use marlowe_agent::ToolOutcome;
use serde_json::Value;

use crate::paths;
use crate::text::*;
use crate::{str_arg, BuiltinTools};

/// Context for one filesystem call.
pub(crate) struct Fs<'a> {
    pub tools: &'a BuiltinTools,
    /// The workspace root, if any; relative paths resolve against it.
    pub root: Option<&'a Path>,
}

enum Loaded {
    Whole(String),
    Capped(String),
    NotText { bytes: u64 },
}

fn load(path: &Path) -> std::io::Result<Loaded> {
    let mut bytes = Vec::new();
    let taken = std::fs::File::open(path)?.take(MAX_READ_BYTES + 1).read_to_end(&mut bytes)?;
    let capped = taken as u64 > MAX_READ_BYTES;
    if capped {
        bytes.truncate(MAX_READ_BYTES as usize);
    }
    Ok(match String::from_utf8(bytes) {
        Ok(s) if capped => Loaded::Capped(s),
        Ok(s) => Loaded::Whole(s),
        Err(e) => {
            let valid = e.utf8_error().valid_up_to();
            if capped && valid > 0 {
                let raw = e.into_bytes();
                Loaded::Capped(String::from_utf8_lossy(&raw[..valid]).into_owned())
            } else {
                Loaded::NotText { bytes: taken as u64 }
            }
        }
    })
}

impl Fs<'_> {
    fn path(&self, args: &Value, key: &str) -> Result<PathBuf, ToolOutcome> {
        match str_arg(args, key) {
            Some(p) if !p.trim().is_empty() => Ok(paths::resolve(self.root, p)),
            _ => Err(ToolOutcome::error(format!("`{key}` is required"))),
        }
    }

    fn shown(&self, abs: &Path) -> String {
        paths::display(self.root, abs)
    }

    pub fn read(&self, args: &Value) -> ToolOutcome {
        let abs = match self.path(args, "path") {
            Ok(p) => p,
            Err(e) => return e,
        };
        let shown = self.shown(&abs);
        let (mut text, capped) = match load(&abs) {
            Ok(Loaded::Whole(s)) => (s, false),
            Ok(Loaded::Capped(s)) => (s, true),
            Ok(Loaded::NotText { bytes }) => {
                return ToolOutcome::ok(
                    format!("{shown} is not UTF-8 text ({bytes} bytes), so its bytes were not returned."),
                    format!("binary · {bytes} B"),
                );
            }
            Err(e) => return ToolOutcome::error(format!("cannot read {shown}: {e}")),
        };
        let total_lines = count_lines(&text);
        let whole_file_tokens =
            estimate_tokens_of(text.len() + total_lines * (LINE_NUMBER_WIDTH + 1));
        let mut first_line = 1usize;
        if let Some(range) = str_arg(args, "range") {
            match slice_lines(&text, range) {
                Ok((sliced, _)) if sliced.is_empty() && total_lines > 0 => {
                    return ToolOutcome::error(format!(
                        "`range` \"{range}\" selected no lines: the file has {total_lines}. Ask \
                         for a range inside 1-{total_lines}, or omit `range` for the whole file."
                    ));
                }
                Ok((sliced, start)) => {
                    text = sliced;
                    first_line = start;
                }
                Err(why) => return ToolOutcome::error(why),
            }
        }
        let expensive = whole_file_tokens > self.tools.context_tokens / 4;
        let selected = count_lines(&text);
        let mut kept = selected;
        if selected > READ_WINDOW_LINES {
            text = take_lines(&text, READ_WINDOW_LINES);
            kept = READ_WINDOW_LINES;
        }
        let mut text = number_lines(&text, first_line);
        if text.len() > READ_WINDOW_BYTES {
            let (cut, lines) = cut_to_whole_lines(&text, READ_WINDOW_BYTES);
            text = cut;
            kept = lines;
        }
        if capped {
            text.push_str(&format!(
                "\n\n[the harness stopped reading at {MAX_READ_BYTES} bytes. The file is longer \
                 than this and the text above is a prefix.]"
            ));
        }
        if kept < selected {
            let last = first_line + kept - 1;
            let next_end = (last + READ_WINDOW_LINES).min(total_lines);
            let cost = if expensive {
                format!(
                    " The whole file is about {whole_file_tokens} tokens; to find something \
                     specific, `grep` searches inside files and returns `path:line:text`."
                )
            } else {
                String::new()
            };
            text.push_str(&format!(
                "\n\n[showing lines {first_line}-{last} of {total_lines}. Continue with range \
                 \"{}-{next_end}\".{cost}]",
                last + 1
            ));
        }
        let summary = format!("{kept} lines · {} B", text.len());
        ToolOutcome::ok(text, summary)
    }

    pub fn write(&self, args: &Value) -> ToolOutcome {
        let abs = match self.path(args, "path") {
            Ok(p) => p,
            Err(e) => return e,
        };
        let Some(content) = str_arg(args, "content") else {
            return ToolOutcome::error("`content` is required");
        };
        let shown = self.shown(&abs);
        let _guard = self.tools.writes.lock().unwrap_or_else(|e| e.into_inner());
        let existing = std::fs::read_to_string(&abs).unwrap_or_default();
        if let Some(parent) = abs.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return ToolOutcome::error(format!("cannot create the directory for {shown}: {e}"));
            }
        }
        if let Err(e) = std::fs::write(&abs, content) {
            return ToolOutcome::error(format!("cannot write {shown}: {e}"));
        }
        let (added, removed) = (count_lines(content), count_lines(&existing));
        let mut body = format!("wrote {shown} (+{added} -{removed})");
        if added >= NUMBERED_CONTENT_LINES && strip_line_numbers(content).is_some() {
            body.push_str(&format!(
                "\nthe file was written, and its lines carry `read`'s line-number prefix \
                 ({LINE_NUMBER_WIDTH} characters then a tab). If that came from pasting a `read` \
                 result, the numbers are now IN the file — write it again without them."
            ));
        }
        ToolOutcome::ok(body, format!("+{added} −{removed}"))
    }

    pub fn edit(&self, args: &Value) -> ToolOutcome {
        let abs = match self.path(args, "path") {
            Ok(p) => p,
            Err(e) => return e,
        };
        let Some(content) = str_arg(args, "content") else {
            return ToolOutcome::error("`content` is required");
        };
        let Some(replacing) = str_arg(args, "replacing") else {
            return ToolOutcome::error(
                "`replacing` is required: `edit` replaces one exact snippet. To create a file or \
                 replace all of it, use `write` instead.",
            );
        };
        if replacing.is_empty() {
            return ToolOutcome::error(
                "`replacing` is empty, which would match at the very start of the file and insert \
                 `content` there rather than replace anything. Give the exact snippet to replace, \
                 or use `write` to replace the whole file.",
            );
        }
        let shown = self.shown(&abs);
        let _guard = self.tools.writes.lock().unwrap_or_else(|e| e.into_inner());
        let existing = match std::fs::read_to_string(&abs) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return ToolOutcome::error(format!(
                    "{shown} does not exist. Use `write` with `path` and `content` to create it."
                ))
            }
            Err(e) => return ToolOutcome::error(format!("cannot read {shown}: {e}")),
        };
        if existing.is_empty() {
            return ToolOutcome::error(
                "the file is empty, so there is no snippet to replace. Use `write` with `path` \
                 and `content` to fill it.",
            );
        }
        let Some(at) = existing.find(replacing) else {
            return ToolOutcome::error(replacing_miss(&existing, replacing));
        };
        let sites: Vec<usize> =
            existing.match_indices(replacing).map(|(i, _)| line_at(&existing, i)).collect();
        if sites.len() > 1 {
            let shown_sites = sites
                .iter()
                .take(MAX_EDIT_SITES_NAMED)
                .map(usize::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            let rest = if sites.len() > MAX_EDIT_SITES_NAMED {
                format!(" and {} more", sites.len() - MAX_EDIT_SITES_NAMED)
            } else {
                String::new()
            };
            return ToolOutcome::error(format!(
                "`replacing` occurs {} times, at lines {shown_sites}{rest}. `edit` changes ONE \
                 snippet, so it must appear exactly once — add a neighbouring line to make it \
                 unique, or edit each site in a separate call. The file is unchanged.",
                sites.len()
            ));
        }
        if count_lines(content) >= NUMBERED_CONTENT_LINES && strip_line_numbers(content).is_some()
        {
            return ToolOutcome::error(format!(
                "`content` carries `read`'s line-number prefix ({LINE_NUMBER_WIDTH} characters \
                 then a tab in front of every line). Writing that would put the numbers in the \
                 source, so it is refused — strip the prefixes and call `edit` again. If you \
                 really do mean to write numbered text, `write` does that and says so."
            ));
        }
        let mut next = String::with_capacity(existing.len() + content.len());
        next.push_str(&existing[..at]);
        next.push_str(content);
        next.push_str(&existing[at + replacing.len()..]);
        if let Err(e) = std::fs::write(&abs, next) {
            return ToolOutcome::error(format!("cannot write {shown}: {e}"));
        }
        let (added, removed) = (count_lines(content), count_lines(replacing));
        ToolOutcome::ok(format!("edited {shown} (+{added} -{removed})"), format!("+{added} −{removed}"))
    }

    pub fn glob(&self, args: &Value) -> ToolOutcome {
        let base = match str_arg(args, "path") {
            Some(_) => match self.path(args, "path") {
                Ok(p) => p,
                Err(e) => return e,
            },
            None => paths::resolve(self.root, "."),
        };
        let pattern = str_arg(args, "pattern").unwrap_or("*");
        if base.is_file() {
            return ToolOutcome::error(
                "`path` is a file, and `glob` lists a DIRECTORY. Pass the directory that contains \
                 it — the file you named is already the answer.",
            );
        }
        let keep = |p: &Path| matches_glob(pattern, p, self.root);
        let walk = collect(&base, WALK_FILE_CAP, &keep);
        let mut hits: Vec<String> = walk.files.iter().map(|f| self.shown(f)).collect();
        hits.sort();
        let found = hits.len();
        let mut listing = String::new();
        if found == 0 {
            listing = if walk.seen == 0 {
                format!("no files under this path at all (`{pattern}` was not the reason)")
            } else {
                format!("{} file(s) are under this path and none matched `{pattern}`", walk.seen)
            };
        }
        let mut listed = 0usize;
        for hit in &hits {
            if listing.len() + hit.len() + 1 > READ_WINDOW_BYTES {
                break;
            }
            if !listing.is_empty() {
                listing.push('\n');
            }
            listing.push_str(hit);
            listed += 1;
        }
        if listed < found {
            listing.push_str(&format!(
                "\n[{listed} of {found} matching paths are listed; the rest did not fit. Narrow \
                 `pattern`, or list a subdirectory.]"
            ));
        }
        if walk.truncated {
            listing.push_str(&format!(
                "\n[enumeration stopped at {WALK_FILE_CAP} files; there may be more under this \
                 path than are listed]"
            ));
        }
        listing.push_str(&skipped_notice(&walk.skipped));
        ToolOutcome::ok(listing, format!("{found} paths"))
    }

    pub fn grep(&self, args: &Value) -> ToolOutcome {
        const PARAMS: [&str; 4] = ["pattern", "path", "glob", "context"];
        if let Some(obj) = args.as_object() {
            if let Some(bad) = obj.keys().find(|k| !PARAMS.contains(&k.as_str())) {
                return ToolOutcome::error(format!(
                    "`{bad}` is not a parameter of `grep`, and the search was NOT run with it \
                     ignored. `grep` takes exactly these: {}. To ignore case, start `pattern` \
                     with `(?i)`. To restrict which files are opened, use `glob`, e.g. `glob: \
                     \"*.rs\"`.",
                    PARAMS.join(", ")
                ));
            }
        }
        let Some(pattern) = str_arg(args, "pattern") else {
            return ToolOutcome::error("`pattern` is required");
        };
        if pattern.is_empty() {
            return ToolOutcome::error(
                "`pattern` is empty, which matches every line of every file. Give the regular \
                 expression to search for, or use `glob` to list files.",
            );
        }
        let re = match regex::RegexBuilder::new(pattern)
            .size_limit(REGEX_SIZE_LIMIT)
            .dfa_size_limit(REGEX_SIZE_LIMIT)
            .build()
        {
            Ok(re) => re,
            Err(e) => {
                return ToolOutcome::error(format!(
                    "`pattern` is not a valid regular expression, and it was NOT searched for as \
                     plain text instead: {e}\nTo search for text that CONTAINS one of \
                     `. * + ? ( ) [ ] {{ }} | ^ $ \\`, put a backslash before each one."
                ))
            }
        };
        let context: usize = match args.get("context") {
            None | Some(Value::Null) => 0,
            Some(v) => {
                let n = match v {
                    Value::Number(n) => n.as_i64(),
                    Value::String(s) => s.trim().parse::<i64>().ok(),
                    _ => None,
                };
                match n {
                    Some(n) if (0..=MAX_GREP_CONTEXT).contains(&n) => n as usize,
                    Some(n) => {
                        return ToolOutcome::error(format!(
                            "`context` is {n}, and it must be a whole number from 0 to \
                             {MAX_GREP_CONTEXT}. It was NOT reduced to fit."
                        ))
                    }
                    None => {
                        return ToolOutcome::error(format!(
                            "`context` must be a whole number from 0 to {MAX_GREP_CONTEXT}."
                        ))
                    }
                }
            }
        };
        let file_glob = match args.get("glob") {
            None | Some(Value::Null) => None,
            Some(Value::String(g)) if !g.is_empty() => Some(g.as_str()),
            _ => {
                return ToolOutcome::error(
                    "`glob` is empty, which matches no file at all. Give a pattern like `*.rs`, \
                     or omit `glob` to search every file.",
                )
            }
        };
        let base = match str_arg(args, "path") {
            Some(_) => match self.path(args, "path") {
                Ok(p) => p,
                Err(e) => return e,
            },
            None => paths::resolve(self.root, "."),
        };
        if base.is_file() {
            return ToolOutcome::error(
                "`path` is a file, and `grep` searches a DIRECTORY. Pass the directory that \
                 contains it, or use `read` to look at this one file.",
            );
        }
        let keep = |p: &Path| file_glob.is_none_or(|g| matches_glob(g, p, self.root));
        let walk = collect(&base, WALK_FILE_CAP, &keep);

        let mut emitted: Vec<String> = Vec::new();
        let mut emitted_bytes = 0usize;
        let mut per_file: Vec<(String, usize)> = Vec::new();
        let mut total = 0usize;
        let mut scanned = 0usize;
        let mut not_text = 0usize;
        let mut part_read: Vec<String> = Vec::new();
        let mut budget_spent = false;

        for candidate in &walk.files {
            let rel = self.shown(candidate);
            let text = match load(candidate) {
                Ok(Loaded::Whole(s)) => s,
                Ok(Loaded::Capped(s)) => {
                    part_read.push(rel.clone());
                    s
                }
                Ok(Loaded::NotText { .. }) => {
                    not_text += 1;
                    continue;
                }
                Err(_) => continue,
            };
            scanned += 1;
            let lines: Vec<&str> = text.lines().collect();
            let matched: Vec<usize> =
                lines.iter().enumerate().filter(|(_, l)| re.is_match(l)).map(|(i, _)| i).collect();
            if matched.is_empty() {
                continue;
            }
            total += matched.len();
            per_file.push((rel.clone(), matched.len()));
            if budget_spent {
                continue;
            }
            let mut wanted: Vec<(usize, bool)> = Vec::new();
            let mut next = 0usize;
            for &m in &matched {
                let from = m.saturating_sub(context).max(next);
                let to = (m + context).min(lines.len().saturating_sub(1));
                for i in from..=to {
                    wanted.push((i, matched.binary_search(&i).is_ok()));
                }
                next = to + 1;
            }
            for (i, is_match) in wanted {
                if emitted.len() >= MAX_MATCH_LINES || emitted_bytes >= READ_WINDOW_BYTES {
                    budget_spent = true;
                    break;
                }
                let sep = if is_match { ':' } else { '-' };
                let rendered = format!("{rel}{sep}{}{sep}{}", i + 1, clip_line(lines[i]));
                emitted_bytes += rendered.len() + 1;
                emitted.push(rendered);
            }
        }

        let mut body = emitted.join("\n");
        if total == 0 {
            body = if walk.seen == 0 {
                format!("no files under this path at all (`{pattern}` was not the reason)")
            } else if scanned == 0 {
                format!(
                    "no file was searched: {} file(s) are under this path, and none of them \
                     matched the `glob` or could be read as text",
                    walk.seen
                )
            } else {
                format!("no line matched `{pattern}` in the {scanned} file(s) searched")
            };
        }
        if budget_spent {
            let mut worst = per_file.clone();
            worst.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            worst.truncate(NAMES_IN_A_NOTICE);
            body.push_str(&format!(
                "\n[{total} matches in {} file(s); the first {} lines are shown and the rest were \
                 counted, not returned. Most matches: {}. Tighten `pattern`, set `glob`, or \
                 search a subdirectory to see the rest.]",
                per_file.len(),
                emitted.len(),
                worst.iter().map(|(p, n)| format!("{p} ({n})")).collect::<Vec<_>>().join(", ")
            ));
        }
        if !part_read.is_empty() {
            part_read.sort();
            let n = part_read.len();
            part_read.truncate(NAMES_IN_A_NOTICE);
            let more = if n > NAMES_IN_A_NOTICE {
                format!(" and {} more", n - NAMES_IN_A_NOTICE)
            } else {
                String::new()
            };
            body.push_str(&format!(
                "\n[{n} file(s) were searched only as far as {MAX_READ_BYTES} bytes: {}{more}]",
                part_read.join(", ")
            ));
        }
        if not_text > 0 {
            body.push_str(&format!("\n[{not_text} file(s) were not text and were not searched.]"));
        }
        if walk.truncated {
            body.push_str(&format!(
                "\n[the search stopped after {WALK_FILE_CAP} files and there is more under this \
                 path. Narrow it with `glob`, e.g. `glob: \"*.rs\"`, or search a subdirectory.]"
            ));
        }
        body.push_str(&skipped_notice(&walk.skipped));
        ToolOutcome::ok(body, format!("{total} results · {scanned} files"))
    }
}

/// A name-only pattern matches the file name; one with `/` matches the root-relative path.
fn matches_glob(pattern: &str, p: &Path, root: Option<&Path>) -> bool {
    if pattern == "*" {
        return true;
    }
    let subject = if pattern.contains('/') {
        match root.and_then(|r| p.strip_prefix(r).ok()) {
            Some(r) => r.to_string_lossy().replace('\\', "/"),
            None => p.to_string_lossy().replace('\\', "/"),
        }
    } else {
        p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    };
    glob_match(pattern, &subject)
}

/// What one directory walk found.
pub(crate) struct Walk {
    pub files: Vec<PathBuf>,
    /// Regular files reached, whether or not `keep` took them.
    pub seen: usize,
    pub skipped: Vec<String>,
    pub truncated: bool,
}

/// Regular files under `base` accepted by `keep`, to a cap, skipping [`WALK_SKIP`] directories
/// (never `base` itself) and not following links.
pub(crate) fn collect(base: &Path, cap: usize, keep: &dyn Fn(&Path) -> bool) -> Walk {
    let mut files = Vec::new();
    let mut seen = 0usize;
    let mut skipped: Vec<String> = Vec::new();
    let mut truncated = false;
    let mut queue = vec![base.to_path_buf()];
    while let Some(dir) = queue.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            if files.len() >= cap {
                truncated = true;
                queue.clear();
                break;
            }
            let path = entry.path();
            let Ok(link_meta) = path.symlink_metadata() else { continue };
            if link_meta.file_type().is_symlink() {
                continue;
            }
            if link_meta.is_dir() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if WALK_SKIP.contains(&name.as_str()) {
                    if !skipped.contains(&name) {
                        skipped.push(name);
                    }
                    continue;
                }
                queue.push(path);
            } else if link_meta.is_file() {
                seen += 1;
                if keep(&path) {
                    files.push(path);
                }
            }
        }
    }
    skipped.sort();
    files.sort();
    Walk { files, seen, skipped, truncated }
}

fn skipped_notice(skipped: &[String]) -> String {
    if skipped.is_empty() {
        return String::new();
    }
    format!(
        "\n[not searched, because they are build output or version control: {}. Name one as \
         `path` to look inside it.]",
        skipped.join(", ")
    )
}
