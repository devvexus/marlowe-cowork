//! `marlowe export trajectories`: select (opt-in project + keep rule), convert, scrub, gate,
//! then write `sft.jsonl`, `dpo.jsonl`, a dataset card and `manifest.json` — or, on a dry run,
//! only report what would be written.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use marlowe_crew::Run;
use marlowe_model::Message;
use marlowe_proto::CrewRunStatus;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::config::ExportConfig;
use super::hf::{self, HfOptions};
use super::scrub::{ScrubInputs, Scrubber, Tally, SCRUB_VERSION};
use super::source;

pub const FORMAT: &str = "marlowe-trajectories/1";
pub const SFT_FILE: &str = "sft.jsonl";
pub const DPO_FILE: &str = "dpo.jsonl";
pub const MANIFEST_FILE: &str = "manifest.json";
pub const CARD_FILE: &str = "README.md";

#[derive(Debug, Clone, Default)]
pub struct Options {
    pub out: Option<PathBuf>,
    pub dry_run: bool,
    /// Narrow to these opted-in projects (each must already be opted in).
    pub only: Vec<String>,
    pub include_tainted: bool,
    pub no_thinking: bool,
    pub since_ms: Option<i64>,
}

/// What an export found and did (or would do).
#[derive(Debug, Clone, Default)]
pub struct Report {
    pub sessions_seen: u64,
    pub runs_seen: u64,
    pub sessions_kept: u64,
    pub runs_kept: u64,
    /// Sources not exported, by reason.
    pub skipped: BTreeMap<&'static str, u64>,
    pub sft: u64,
    pub dpo: u64,
    pub dpo_kinds: BTreeMap<&'static str, u64>,
    /// Records the gate refused (a detector still fired after scrubbing).
    pub dropped: u64,
    pub dropped_kinds: BTreeMap<String, u64>,
    /// Tool names an exported SFT record calls that the built-in registry has no schema for (MCP,
    /// memory and crew tools, retired tools), counted once per record.
    pub tools_unknown: u64,
    pub tally: Tally,
    pub bytes: u64,
    pub out: Option<PathBuf>,
    pub dry_run: bool,
}

impl Report {
    fn skip(&mut self, why: &'static str) {
        *self.skipped.entry(why).or_default() += 1;
    }
}

/// One kept trajectory before conversion.
struct Source {
    kind: &'static str,
    id: String,
    step_key: Option<String>,
    role: Option<String>,
    project: String,
    keep: &'static str,
    created_ms: i64,
    messages: Vec<Message>,
}

/// A crew run counts as kept once its work was accepted: merged into the workspace (build) or
/// its report saved there (research). The supervisor records that in the run's note.
pub fn crew_kept(run: &Run) -> bool {
    run.status == CrewRunStatus::Done
        && run.note.as_deref().is_some_and(|n| n.starts_with("merged:") || n.starts_with("report saved to"))
}

fn short_hash(parts: &[&str]) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p.as_bytes());
        h.update([0u8]);
    }
    hex(&h.finalize()[..8])
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn date(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_default()
}

/// Gathers the kept trajectories.
fn select(conn: &rusqlite::Connection, cfg: &ExportConfig, opts: &Options, rep: &mut Report) -> Result<Vec<Source>, String> {
    let marks = source::good_marks(conn)?;
    let mut out = Vec::new();
    let project_ok = |ws: Option<&str>, rep: &mut Report| -> Option<String> {
        let Some(ws) = ws.filter(|w| !w.trim().is_empty()) else {
            rep.skip("no_project");
            return None;
        };
        let Some(p) = cfg.project_of(ws) else {
            rep.skip("not_opted_in");
            return None;
        };
        if !opts.only.is_empty() && !opts.only.iter().any(|o| super::config::norm_path(o) == super::config::norm_path(p)) {
            rep.skip("not_selected");
            return None;
        }
        Some(p.to_string())
    };
    for s in source::sessions(conn)? {
        rep.sessions_seen += 1;
        let Some(project) = project_ok(s.workspace.as_deref(), rep) else { continue };
        let Some(mark) = marks.get(&s.id) else {
            rep.skip("not_kept");
            continue;
        };
        if opts.since_ms.is_some_and(|t| s.updated_ms < t) {
            rep.skip("before_since");
            continue;
        }
        if s.tainted && !opts.include_tainted {
            rep.skip("tainted");
            continue;
        }
        let mut messages = source::turns(conn, &s.id, mark.upto)?;
        hf::trim_to_answer(&mut messages);
        if messages.is_empty() {
            rep.skip("empty");
            continue;
        }
        rep.sessions_kept += 1;
        out.push(Source {
            kind: "conductor",
            id: s.id.clone(),
            step_key: None,
            role: None,
            project,
            keep: "good",
            created_ms: s.created_ms,
            messages,
        });
    }
    for run in source::crew_runs(conn)? {
        rep.runs_seen += 1;
        let ws = run.workspace.as_ref().map(|p| p.to_string_lossy().to_string());
        let Some(project) = project_ok(ws.as_deref(), rep) else { continue };
        if !crew_kept(&run) {
            rep.skip("not_kept");
            continue;
        }
        if opts.since_ms.is_some_and(|t| run.updated_ms < t) {
            rep.skip("before_since");
            continue;
        }
        let steps = source::crew_steps(conn, run.id.as_str())?;
        if (run.tainted || steps.iter().any(|s| s.tainted)) && !opts.include_tainted {
            rep.skip("tainted");
            continue;
        }
        let mut any = false;
        for step in steps {
            let mut messages = step.messages;
            hf::trim_to_answer(&mut messages);
            if messages.is_empty() {
                continue;
            }
            any = true;
            out.push(Source {
                kind: "crew",
                id: run.id.as_str().to_string(),
                step_key: Some(step.key),
                role: Some(step.role),
                project: project.clone(),
                keep: "merged",
                created_ms: run.created_ms,
                messages,
            });
        }
        if any {
            rep.runs_kept += 1;
        } else {
            rep.skip("empty");
        }
    }
    Ok(out)
}

/// The gate: every string in `v` must be clean, and the serialized line must hold no known secret.
fn gate(sc: &Scrubber, v: &Value, line: &str) -> Result<(), Vec<&'static str>> {
    fn walk(sc: &Scrubber, v: &Value, hits: &mut Vec<&'static str>) {
        match v {
            Value::String(s) => hits.extend(sc.findings(s)),
            Value::Array(a) => a.iter().for_each(|x| walk(sc, x, hits)),
            Value::Object(o) => o.iter().for_each(|(k, x)| {
                hits.extend(sc.findings(k));
                // Tool-call arguments are a JSON string: check what a reader would decode.
                if k == "arguments" {
                    if let Some(parsed) = x.as_str().and_then(|s| serde_json::from_str::<Value>(s).ok()) {
                        walk(sc, &parsed, hits);
                    }
                }
                walk(sc, x, hits)
            }),
            _ => {}
        }
    }
    let mut hits = Vec::new();
    walk(sc, v, &mut hits);
    if sc.leaks_known(line) {
        hits.push("known_secret");
    }
    if hits.is_empty() {
        Ok(())
    } else {
        hits.sort();
        hits.dedup();
        Err(hits)
    }
}

/// Lines to write, per file.
#[derive(Default)]
struct Lines {
    sft: Vec<(String, String)>,
    dpo: Vec<(String, String)>,
}

fn emit(sc: &Scrubber, v: Value, id: String, into: &mut Vec<(String, String)>, rep: &mut Report) -> bool {
    let line = serde_json::to_string(&v).expect("record serializes");
    match gate(sc, &v, &line) {
        Ok(()) => {
            rep.bytes += line.len() as u64 + 1;
            into.push((id, line));
            true
        }
        Err(kinds) => {
            rep.dropped += 1;
            for k in kinds {
                *rep.dropped_kinds.entry(k.to_string()).or_default() += 1;
            }
            false
        }
    }
}

/// [`emit`] for a row built only from strings that already passed the gate.
fn emit_checked(sc: &Scrubber, v: Value, id: String, into: &mut Vec<(String, String)>, rep: &mut Report) -> bool {
    let line = serde_json::to_string(&v).expect("record serializes");
    if sc.leaks_known(&line) {
        rep.dropped += 1;
        *rep.dropped_kinds.entry("known_secret".into()).or_default() += 1;
        return false;
    }
    rep.bytes += line.len() as u64 + 1;
    into.push((id, line));
    true
}

/// Where the `tools` schemas come from, for the manifest.
fn tools_source() -> String {
    format!("registry@{}", env!("CARGO_PKG_VERSION"))
}

/// The `tools` list for a record: every tool its messages call, in first-call order, with the
/// built-in registry's description and JSON schema (scrubbed like everything else). A name the
/// registry does not hold is listed by name only; the second value counts those.
fn tools_for(sc: &Scrubber, msgs: &[Message], tally: &mut Tally) -> (Vec<Value>, u64) {
    let table = marlowe_exec::specs::table();
    let mut names: Vec<&str> = Vec::new();
    for c in msgs.iter().flat_map(|m| m.tool_calls()) {
        if !names.contains(&c.name.as_str()) {
            names.push(&c.name);
        }
    }
    let mut unknown = 0;
    let tools = names
        .into_iter()
        .map(|name| match table.get(name) {
            Some(def) => json!({
                "type": "function",
                "function": {
                    "name": sc.scrub(name, tally),
                    "description": sc.scrub(&def.spec.description, tally),
                    "parameters": sc.scrub_value(&def.spec.input_schema, tally),
                }
            }),
            None => {
                unknown += 1;
                json!({ "type": "function", "function": { "name": sc.scrub(name, tally) } })
            }
        })
        .collect();
    (tools, unknown)
}

fn build(sc: &Scrubber, sources: &[Source], opts: &Options, rep: &mut Report) -> Lines {
    let mut lines = Lines::default();
    let hf_opts = HfOptions { thinking: !opts.no_thinking };
    for s in sources {
        let mut tally = Tally::default();
        let conv = hf::convert(&s.messages, sc, hf_opts, &mut tally);
        let (tools, unknown_tools) = tools_for(sc, &s.messages, &mut tally);
        let key = s.step_key.as_deref().unwrap_or("");
        let id = short_hash(&[s.kind, &s.id, key, &s.messages.len().to_string()]);
        let project = sc.scrub(Path::new(&s.project).file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default().as_str(), &mut tally);
        let mut rec = serde_json::Map::new();
        rec.insert("id".into(), json!(id));
        rec.insert("source".into(), json!(s.kind));
        rec.insert("keep".into(), json!(s.keep));
        rec.insert("project".into(), json!(project));
        if let Some(r) = &s.role {
            rec.insert("role".into(), json!(r));
        }
        if let Some(k) = &s.step_key {
            rec.insert("step_key".into(), json!(k));
        }
        rec.insert("date".into(), json!(date(s.created_ms)));
        rec.insert("messages".into(), Value::Array(conv.messages.clone()));
        rec.insert("tools".into(), Value::Array(tools.clone()));
        let pairs = hf::step_pairs(&s.messages);
        if emit(sc, Value::Object(rec), id.clone(), &mut lines.sft, rep) {
            rep.sft += 1;
            rep.tools_unknown += unknown_tools;
        } else {
            // Its pairs are cut from the same messages: they go with it.
            rep.dropped += pairs.len() as u64;
            rep.tally.merge(&tally);
            continue;
        }
        for (n, pair) in pairs.iter().enumerate() {
            let Some((prompt, chosen, rejected)) = hf::dpo_row(&conv, pair) else { continue };
            let pid = short_hash(&[&id, "dpo", &n.to_string()]);
            let row = json!({
                "id": pid,
                "source": s.kind,
                "project": project,
                "kind": pair.kind,
                "prompt": prompt,
                "chosen": chosen,
                "rejected": rejected,
                "tools": tools,
            });
            // Every string here passed the gate in the SFT row; the line check is what is new.
            if emit_checked(sc, row, pid.clone(), &mut lines.dpo, rep) {
                rep.dpo += 1;
                *rep.dpo_kinds.entry(pair.kind).or_default() += 1;
            }
        }
        rep.tally.merge(&tally);
    }
    lines
}

/// Runs an export. Refuses when no project opted in (export is off by default).
pub fn export(home: &Path, cfg: &ExportConfig, inputs: &ScrubInputs, opts: &Options) -> Result<Report, String> {
    if cfg.projects.is_empty() {
        return Err("export is off: no project has opted in (marlowe export enable <dir>)".into());
    }
    for o in &opts.only {
        if !cfg.projects.iter().any(|p| super::config::norm_path(p) == super::config::norm_path(o)) {
            return Err(format!("{o} has not opted in (marlowe export enable <dir>)"));
        }
    }
    let conn = source::open(&marlowe_proto::paths::cowork_db(home))?;
    let mut rep = Report { dry_run: opts.dry_run, ..Report::default() };
    let sources = select(&conn, cfg, opts, &mut rep)?;
    drop(conn);
    let sc = Scrubber::new(inputs);
    let lines = build(&sc, &sources, opts, &mut rep);
    if opts.dry_run {
        return Ok(rep);
    }
    let out = opts.out.clone().unwrap_or_else(|| {
        home.join("exports").join(format!("trajectories-{}", chrono::Local::now().format("%Y%m%d-%H%M%S")))
    });
    if out.exists() && std::fs::read_dir(&out).map(|mut d| d.next().is_some()).unwrap_or(true) {
        return Err(format!("{} already exists and is not empty", out.display()));
    }
    std::fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    let sft = write_lines(&out.join(SFT_FILE), &lines.sft)?;
    let dpo = write_lines(&out.join(DPO_FILE), &lines.dpo)?;
    std::fs::write(out.join(CARD_FILE), card(&rep)).map_err(|e| e.to_string())?;
    let manifest = manifest(cfg, &sc, opts, &rep, &[(SFT_FILE, sft, &lines.sft), (DPO_FILE, dpo, &lines.dpo)]);
    let text = serde_json::to_string_pretty(&manifest).map_err(|e| e.to_string())?;
    std::fs::write(out.join(MANIFEST_FILE), text).map_err(|e| e.to_string())?;
    rep.out = Some(out);
    Ok(rep)
}

/// Writes JSONL; returns (sha256 hex, bytes).
fn write_lines(path: &Path, lines: &[(String, String)]) -> Result<(String, u64), String> {
    let f = std::fs::File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut w = std::io::BufWriter::new(f);
    let mut h = Sha256::new();
    let mut bytes = 0u64;
    for (_, l) in lines {
        for chunk in [l.as_bytes(), b"\n"] {
            w.write_all(chunk).map_err(|e| e.to_string())?;
            h.update(chunk);
            bytes += chunk.len() as u64;
        }
    }
    w.flush().map_err(|e| e.to_string())?;
    Ok((hex(&h.finalize()), bytes))
}

type FileRow<'a> = (&'a str, (String, u64), &'a Vec<(String, String)>);

fn manifest(cfg: &ExportConfig, sc: &Scrubber, opts: &Options, rep: &Report, files: &[FileRow<'_>]) -> Value {
    let projects: Vec<Value> = cfg
        .projects
        .iter()
        .map(|p| {
            let name = Path::new(p).file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default();
            json!({ "name": sc.scrub(&name, &mut Tally::default()), "path_hash": short_hash(&[&super::config::norm_path(p)]) })
        })
        .collect();
    json!({
        "format": FORMAT,
        "created": chrono::Local::now().to_rfc3339(),
        "marlowe_version": env!("CARGO_PKG_VERSION"),
        "local_only": true,
        "tools_source": tools_source(),
        "tools_unknown": rep.tools_unknown,
        "keep_rule": "conductor sessions marked /good (up to the mark); crew runs merged or whose report was saved",
        "options": {
            "include_tainted": opts.include_tainted,
            "thinking": !opts.no_thinking,
            "since_ms": opts.since_ms,
            "only": opts.only.len(),
        },
        "projects": projects,
        "sources": {
            "sessions_seen": rep.sessions_seen,
            "sessions_kept": rep.sessions_kept,
            "runs_seen": rep.runs_seen,
            "runs_kept": rep.runs_kept,
            "skipped": rep.skipped,
        },
        "records": {
            "sft": rep.sft,
            "dpo": rep.dpo,
            "dpo_kinds": rep.dpo_kinds,
            "dropped_by_gate": rep.dropped,
            "dropped_kinds": rep.dropped_kinds,
        },
        "scrub": {
            "version": SCRUB_VERSION,
            "layers": sc.describe(),
            "redactions": rep.tally.0,
        },
        "files": files.iter().map(|(name, (sha, bytes), lines)| json!({
            "path": name,
            "sha256": sha,
            "bytes": bytes,
            "records": lines.len(),
            "ids": lines.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    })
}

/// The Hugging Face dataset card: two configs, `sft` and `dpo`.
fn card(rep: &Report) -> String {
    format!(
        "---\nconfigs:\n- config_name: sft\n  data_files: {SFT_FILE}\n- config_name: dpo\n  data_files: {DPO_FILE}\n---\n\n\
         # Marlowe trajectories\n\nExported locally by `marlowe export trajectories` ({FORMAT}). Nothing was uploaded.\n\n\
         - `sft`: {} kept trajectories, `messages` in the transformers tool-calling format (`tool_calls[].function.arguments` is a JSON string).\n\
         - `dpo`: {} step-level preference pairs (`prompt`/`chosen`/`rejected`, conversational).\n\
         - `tools` (both configs): the schema of every tool the row calls, from Marlowe's built-in tool registry, \
         for `apply_chat_template(messages, tools=row[\"tools\"])`. Tools outside the registry (MCP, memory and crew \
         tools) appear by name only ({} in this export). Rows carry no system prompt.\n\n\
         Every string passed the layered scrubber (scrub v{SCRUB_VERSION}); see `manifest.json` for redaction counts and file hashes.\n",
        rep.sft, rep.dpo, rep.tools_unknown
    )
}

/// The human report printed after an export or a dry run.
pub fn render(rep: &Report) -> String {
    let mut s = String::new();
    let head = if rep.dry_run { "dry run: nothing written" } else { "exported" };
    s.push_str(&format!("{head}\n"));
    s.push_str(&format!(
        "  sources   {} session(s) seen, {} kept; {} crew run(s) seen, {} kept\n",
        rep.sessions_seen, rep.sessions_kept, rep.runs_seen, rep.runs_kept
    ));
    if !rep.skipped.is_empty() {
        let parts: Vec<String> = rep.skipped.iter().map(|(k, v)| format!("{k} {v}")).collect();
        s.push_str(&format!("  skipped   {}\n", parts.join(", ")));
    }
    let kinds: Vec<String> = rep.dpo_kinds.iter().map(|(k, v)| format!("{k} {v}")).collect();
    s.push_str(&format!(
        "  records   sft {}, dpo {}{}, ~{} KiB\n",
        rep.sft,
        rep.dpo,
        if kinds.is_empty() { String::new() } else { format!(" ({})", kinds.join(", ")) },
        rep.bytes.div_ceil(1024)
    ));
    if rep.tools_unknown > 0 {
        s.push_str(&format!("  tools     {} call name(s) outside the registry, listed without a schema\n", rep.tools_unknown));
    }
    if rep.dropped > 0 {
        let parts: Vec<String> = rep.dropped_kinds.iter().map(|(k, v)| format!("{k} {v}")).collect();
        s.push_str(&format!("  gate      {} record(s) dropped: {}\n", rep.dropped, parts.join(", ")));
    }
    if rep.tally.0.is_empty() {
        s.push_str("  scrubbed  nothing to redact\n");
    } else {
        let parts: Vec<String> = rep.tally.0.iter().map(|(k, v)| format!("{k} {v}")).collect();
        s.push_str(&format!("  scrubbed  {} redaction(s): {}\n", rep.tally.total(), parts.join(", ")));
    }
    if let Some(out) = &rep.out {
        s.push_str(&format!("  wrote     {}\n", out.display()));
    }
    s
}
