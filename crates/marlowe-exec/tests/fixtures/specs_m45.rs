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
            "List files under a directory, optionally filtered. `pattern` supports `*` and `?` \
             (within one path segment), `**` (any depth, so `**/Cargo.toml` finds every one, the \
             top-level file included) and `{a,b}`; a pattern without `/` matches file names \
             anywhere beneath, one with `/` matches the path relative to `path`. Build output and \
             VCS directories are skipped.",
            json!({
                "path": {"type": "string", "description": "Directory to list; default is the workspace."},
                "pattern": {"type": "string", "description": "e.g. \"*.rs\", \"src/*.rs\" or \"**/Cargo.toml\"."}
            }),
            &[],
        ),
        spec(
            "grep",
            "Search file contents with a regular expression (Rust regex syntax; start with `(?i)` \
             for case-insensitive). Returns `path:line:text` for matches and `path-line-text` for \
             context lines.",
            json!({
                "pattern": {"type": "string"},
                "path": {"type": "string", "description": "Directory to search; default is the workspace."},
                "glob": {"type": "string", "description": "Only search files matching this, e.g. \"*.rs\"."},
                "context": {"type": "integer", "description": "Lines of context around each match (0-20)."}
            }),
            &["pattern"],
        ),
        spec(
            "bash",
            "Run a command in bash (Git Bash on Windows). stdin is closed; output is bounded; the \
             command is killed at the timeout (default 120000 ms).",
            json!({
                "command": {"type": "string"},
                "cwd": {"type": "string", "description": "Working directory; default is the workspace."},
                "timeout_ms": {"type": "integer"}
            }),
            &["command"],
        ),
        spec(
            "powershell",
            "Run a command in Windows PowerShell (-NoProfile -NonInteractive). stdin is closed; \
             output is bounded; the command is killed at the timeout (default 120000 ms).",
            json!({
                "command": {"type": "string"},
                "cwd": {"type": "string"},
                "timeout_ms": {"type": "integer"}
            }),
            &["command"],
        ),
        spec(
            "python",
            "Run Python with the user's interpreter: either an inline snippet (`code`) or a script \
             file (`path`). Give exactly one. Output is bounded; killed at the timeout.",
            json!({
                "code": {"type": "string", "description": "Inline source to run."},
                "path": {"type": "string", "description": "Script file to run."},
                "args": {"type": "array", "items": {"type": "string"}, "description": "Command-line arguments."},
                "cwd": {"type": "string"},
                "timeout_ms": {"type": "integer"}
            }),
            &[],
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
