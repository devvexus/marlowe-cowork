//! Tool declarations shown to the model.

use marlowe_model::ToolSpec;
use serde_json::{json, Value};

fn spec(name: &str, description: &str, properties: Value, required: &[&str]) -> ToolSpec {
    ToolSpec {
        name: name.to_string(),
        description: description.to_string(),
        input_schema: json!({
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false,
        }),
    }
}

/// The properties every shell tool shares, after its own.
fn shell_props(own: Value) -> Value {
    let mut props = own;
    let shared = json!({
        "cwd": {"type": "string", "description": "Working directory; default is the workspace."},
        "timeout_ms": {"type": "integer", "description": "Kill the command after this long (default 120000, at most 600000)."},
        "run_in_background": {"type": "boolean", "description": "Start it as a background job and return its id at once."},
        "description": {"type": "string", "description": "What the command does, in a few words."},
        "raw": {"type": "boolean", "description": "Cut a long output in the middle instead of choosing the lines about failures."}
    });
    if let (Some(p), Some(s)) = (props.as_object_mut(), shared.as_object()) {
        p.extend(s.clone());
    }
    props
}

pub fn all(with_search: bool) -> Vec<ToolSpec> {
    let mut v = vec![
        spec(
            "read",
            "Read a text file. Returns numbered lines (`cat -n` style: right-aligned number, tab, \
             text). Long files come back in windows; the result says how to continue with `range`. \
             Do not copy the number prefix into `edit`.",
            json!({
                "path": {"type": "string", "description": "File path, relative to the workspace or absolute."},
                "range": {"type": "string", "description": "Optional 1-based inclusive line range, e.g. \"20-60\"."}
            }),
            &["path"],
        ),
        spec(
            "write",
            "Create a file or overwrite it entirely with `content`. Parent directories are created.",
            json!({
                "path": {"type": "string"},
                "content": {"type": "string", "description": "The complete new file content."}
            }),
            &["path", "content"],
        ),
        spec(
            "edit",
            "Replace one exact snippet in an existing file. `replacing` must match byte for byte \
             (including indentation) and appear exactly once; copy it from a `read` without the \
             line-number prefixes.",
            json!({
                "path": {"type": "string"},
                "replacing": {"type": "string", "description": "The exact text to replace."},
                "content": {"type": "string", "description": "The replacement text."}
            }),
            &["path", "replacing", "content"],
        ),
        spec(
            "glob",
            "List files by name pattern, newest first. `pattern` is a gitignore-style glob: `*.rs` matches that \
            name at any depth, `src/*.rs` is relative to `path`, `**/Cargo.toml` matches at every depth (the top \
            level too), `*.{ts,tsx}` alternates, `!x` excludes. Hidden files are listed; files ignored by \
            .gitignore and VCS directories are not. Omit `pattern` to list every file.",
            json!({
                "pattern": {"type": "string", "description": "e.g. \"*.rs\", \"src/**/*.ts\" or \"**/Cargo.toml\"."},
                "path": {"type": "string", "description": "Directory to search; default is the workspace."},
                "head_limit": {"type": "integer", "description": "Most paths to list; default 250, 0 for all."},
                "offset": {"type": "integer", "description": "Skip this many paths first (to page)."}
            }),
            &[],
        ),
        spec(
            "grep",
            "Search file contents with ripgrep (Rust regex syntax). `output_mode` picks the result: \
            files_with_matches (default) lists the paths, newest first; content prints `path:line:text` for \
            matches and `path-line-text` for context lines, `--` between groups; count prints `path:N`. Hidden \
            files are searched; files ignored by .gitignore and VCS directories are not (name an ignored \
            directory as `path` to search it). `path` may be a directory or one file.",
            json!({
                "pattern": {"type": "string", "description": "Regular expression; backslash-escape metacharacters to match them literally."},
                "path": {"type": "string", "description": "Directory or file to search; default is the workspace."},
                "glob": {"type": "string", "description": "Only search files matching this, e.g. \"*.rs\" or \"*.{ts,tsx}\"."},
                "type": {"type": "string", "description": "Only search this ripgrep file type: rust, py, js, ts, go, c, cpp, java, md, json..."},
                "output_mode": {"type": "string", "enum": ["files_with_matches", "content", "count"]},
                "-A": {"type": "integer", "description": "Lines of context after each match (content mode)."},
                "-B": {"type": "integer", "description": "Lines of context before each match (content mode)."},
                "-C": {"type": "integer", "description": "Lines of context around each match (content mode)."},
                "-i": {"type": "boolean", "description": "Ignore case."},
                "-n": {"type": "boolean", "description": "Show line numbers in content mode; default true."},
                "multiline": {"type": "boolean", "description": "Let the pattern span lines (`.` matches newlines)."},
                "head_limit": {"type": "integer", "description": "Most result lines (paths, rows or counts); default 250, 0 for all."},
                "offset": {"type": "integer", "description": "Skip this many result lines first (to page)."}
            }),
            &["pattern"],
        ),
        spec(
            "bash",
            "Run a command in bash (Git Bash on Windows). stdin is closed; the command is killed at the timeout \
             (default 120000 ms, at most 600000). At most 30 KB of output comes back: when there is more, the lines that \
             explain a failure and the end are kept, and `[full output: path, N lines]` names the whole log (read it with \
             `read` or `grep`); `raw` cuts the middle instead. For a server, a watcher or a long build set \
             `run_in_background`: you get a job id at once and are told when it exits.",
            shell_props(json!({"command": {"type": "string"}})),
            &["command"],
        ),
        spec(
            "powershell",
            "Run a command in Windows PowerShell (-NoProfile -NonInteractive). stdin is closed; the command is killed at \
             the timeout (default 120000 ms, at most 600000). At most 30 KB of output comes back, failure lines first, \
             with `[full output: path, N lines]` for the whole log; `raw` cuts the middle instead. `run_in_background` \
             returns a job id at once.",
            shell_props(json!({"command": {"type": "string"}})),
            &["command"],
        ),
        spec(
            "python",
            "Run Python with the user's interpreter: either an inline snippet (`code`) or a script file (`path`). Give \
             exactly one. Killed at the timeout; at most 30 KB of output comes back (see `bash`); `run_in_background` \
             returns a job id at once.",
            shell_props(json!({
                "code": {"type": "string", "description": "Inline source to run."},
                "path": {"type": "string", "description": "Script file to run."},
                "args": {"type": "array", "items": {"type": "string"}, "description": "Command-line arguments."}
            })),
            &[],
        ),
        spec(
            "job_output",
            "Read a background job's output. Without `since` you get what is new since your last read; `since` is a byte \
             offset (the `next since` of an earlier read) for an exact window. `filter` keeps only lines matching a regular \
             expression. The result ends with the job's status and the next offset.",
            json!({
                "id": {"type": "string", "description": "The job id, e.g. \"j3\"."},
                "since": {"type": "integer", "description": "Byte offset to read from."},
                "filter": {"type": "string", "description": "Regular expression; only matching lines are returned."}
            }),
            &["id"],
        ),
        spec(
            "job_watch",
            "Follow a running background job: its new output lines reach you as notices between your steps while you work \
             on something else, until a line matches `until_regex` (then you are told which, and the watch ends) or the job \
             exits. `filter` limits it to matching lines. The job's exit is reported even without a watch.",
            json!({
                "id": {"type": "string", "description": "The job id, e.g. \"j3\"."},
                "until_regex": {"type": "string", "description": "Stop watching at the first line matching this."},
                "filter": {"type": "string", "description": "Only report lines matching this."}
            }),
            &["id"],
        ),
        spec(
            "job_kill",
            "Stop a background job and everything it started.",
            json!({"id": {"type": "string", "description": "The job id, e.g. \"j3\"."}}),
            &["id"],
        ),
        spec(
            "web_fetch",
            "Fetch an https URL and return its main content (HTML as markdown with links, headings, \
             tables and code; PDF, Office, text). Honours robots.txt. Long documents come back in \
             windows; continue with `offset`.",
            json!({
                "url": {"type": "string", "description": "A full https:// URL."},
                "offset": {"type": "integer", "description": "Character offset to continue from."}
            }),
            &["url"],
        ),
        spec(
            "arxiv",
            "Search arXiv. Plain keywords, or field queries (`ti:`, `au:`, `abs:`, `cat:`), or an              arXiv id. Returns titles, authors, dates, ids and abstracts. Results are the authors'              claims, not findings; read the paper with `paper`.",
            json!({
                "query": {"type": "string"},
                "max": {"type": "integer", "description": "Results to return (default 8, at most 25)."}
            }),
            &["query"],
        ),
        spec(
            "paper",
            "Read a paper from an https URL (HTML, PDF or LaTeX source) or an arXiv id (2301.01234; \
             arXiv's HTML is read first, then the source, then the PDF). Returns title, authors when \
             known, numbered sections, the abstract and the text as markdown; long papers come back \
             in windows, continue with `offset`. `part: \"references\"` returns the parsed reference \
             list with DOIs and arXiv ids.",
            json!({
                "url_or_arxiv_id": {"type": "string"},
                "offset": {"type": "integer", "description": "Character offset to continue from."},
                "part": {"type": "string", "enum": ["text", "references"], "description": "What to return (default text)."}
            }),
            &["url_or_arxiv_id"],
        ),
    ];
    if with_search {
        v.push(spec(
            "web_search",
            "Search the web. Returns a short answer and the sources it cites, with URLs; use \
             `web_fetch` to read a source in full.",
            json!({"query": {"type": "string"}}),
            &["query"],
        ));
    }
    v
}
