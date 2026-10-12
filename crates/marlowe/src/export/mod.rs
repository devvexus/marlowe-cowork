//! `marlowe export` (track M): local training data from Marlowe's own work. Off by default; a
//! project opts in with `marlowe export enable <dir>` (`<home>\export.json`). Only kept work is
//! exported: conductor sessions marked `/good` (`daemon_kv` `export.good.<session>`, written by
//! the conductor's `/good` and by `marlowe export good`) and crew runs that were merged. Output is
//! HF JSONL (`sft.jsonl`, step-level `dpo.jsonl`, a dataset card) plus `manifest.json`, written
//! under `<home>\exports\` and never uploaded. Every string goes through the layered scrubber
//! (`scrub/`), and a gate drops any record a detector still fires on. `--dry-run` reports
//! without writing. Design 3.10; acceptance A11 (`scrub/tests.rs`, `tests.rs`).

mod config;
mod hf;
mod machine;
mod scrub;
mod source;
mod trajectories;
#[cfg(test)]
mod tests;

use std::io::Write;
use std::path::Path;

use config::ExportConfig;
use scrub::ScrubInputs;
use source::GoodMark;
use trajectories::Options;

const USAGE: &str = "\
marlowe export status                       opted-in projects and /good marks
marlowe export enable <dir>                 opt a project in (export is off by default)
marlowe export disable <dir>                opt it out again
marlowe export good <session> [--upto N]    keep a conductor session (what /good records);
                                            N stored messages, default all so far
marlowe export ungood <session>             remove that mark
marlowe export trajectories [--dry-run] [--out DIR] [--project DIR]... [--include-tainted]
                            [--no-thinking] [--since YYYY-MM-DD]
                                            write sft.jsonl, dpo.jsonl, README.md and
                                            manifest.json (default <home>\\exports\\...);
                                            local only, nothing is uploaded

Kept: sessions marked /good, crew runs that were merged. Sessions that read web or other
untrusted content are left out unless --include-tainted. Every string is scrubbed (known keys,
secret files, credential shapes, personal data, identity, high-entropy tokens) and a record a
detector still fires on is dropped. Extra terms: \"redact_terms\" / \"allow_terms\" in export.json.
";

pub fn run(home: &Path, args: &[String]) -> Result<i32, String> {
    let cfg = ExportConfig::load(home)?;
    let inputs = machine::gather(home, &cfg);
    let mut out = std::io::stdout().lock();
    run_with(home, args, &inputs, &mut out)
}

/// The command with this machine's scrub inputs supplied (tests pass fakes).
pub(crate) fn run_with(home: &Path, args: &[String], inputs: &ScrubInputs, out: &mut dyn Write) -> Result<i32, String> {
    let w = |out: &mut dyn Write, s: &str| out.write_all(s.as_bytes()).map_err(|e| e.to_string());
    let rest = args.get(1..).unwrap_or_default();
    match args.first().map(String::as_str) {
        None | Some("-h" | "--help" | "help") => {
            w(out, USAGE)?;
            Ok(if args.is_empty() { 1 } else { 0 })
        }
        Some("status") => {
            let cfg = ExportConfig::load(home)?;
            let mut s = String::new();
            if cfg.projects.is_empty() {
                s.push_str("export is off: no project has opted in\n");
            } else {
                s.push_str("opted in:\n");
                for p in &cfg.projects {
                    s.push_str(&format!("  {p}\n"));
                }
            }
            let db = marlowe_proto::paths::cowork_db(home);
            if db.is_file() {
                let conn = source::open(&db)?;
                let marks = source::good_marks(&conn)?;
                s.push_str(&format!("/good marks: {}\n", marks.len()));
            }
            s.push_str(&format!("config: {}\n", config::path(home).display()));
            w(out, &s)?;
            Ok(0)
        }
        Some("enable" | "disable") => {
            let on = args[0] == "enable";
            let [dir] = rest else { return Err(format!("usage: marlowe export {} <dir>", args[0])) };
            let mut cfg = ExportConfig::load(home)?;
            let msg = if on {
                let dir = config::canonical_dir(dir)?;
                if cfg.projects.iter().any(|p| config::norm_path(p) == config::norm_path(&dir)) {
                    format!("{dir} is already opted in\n")
                } else {
                    cfg.projects.push(dir.clone());
                    cfg.save(home)?;
                    format!("opted in: {dir}\n")
                }
            } else {
                let target = config::canonical_dir(dir).unwrap_or_else(|_| dir.clone());
                let before = cfg.projects.len();
                cfg.projects.retain(|p| config::norm_path(p) != config::norm_path(&target));
                if cfg.projects.len() == before {
                    format!("{dir} was not opted in\n")
                } else {
                    cfg.save(home)?;
                    format!("opted out: {target}\n")
                }
            };
            w(out, &msg)?;
            Ok(0)
        }
        Some("good" | "ungood") => {
            let good = args[0] == "good";
            let Some(id) = rest.first() else { return Err(format!("usage: marlowe export {} <session>", args[0])) };
            let mut upto = None;
            let mut it = rest[1..].iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--upto" if good => {
                        let n = it.next().ok_or("--upto needs a number")?;
                        upto = Some(n.parse::<u32>().map_err(|_| format!("--upto {n:?}: not a number"))?);
                    }
                    other => return Err(format!("unknown argument {other:?}")),
                }
            }
            let conn = source::open_rw(&marlowe_proto::paths::cowork_db(home))?;
            let session = source::resolve_session(&conn, id)?;
            if good {
                let count = source::turn_count(&conn, &session)?;
                let kept = upto.unwrap_or(count).min(count);
                source::set_good(&conn, &session, Some(&GoodMark { upto: Some(kept), at_ms: now_ms() }))?;
                w(out, &format!("kept {session} ({kept} message(s))\n"))?;
            } else {
                source::set_good(&conn, &session, None)?;
                w(out, &format!("unmarked {session}\n"))?;
            }
            Ok(0)
        }
        Some("trajectories") => {
            let opts = parse_options(rest)?;
            let cfg = ExportConfig::load(home)?;
            let inputs = ScrubInputs {
                redact_terms: cfg.redact_terms.clone(),
                allow_terms: cfg.allow_terms.clone(),
                ..inputs.clone()
            };
            match trajectories::export(home, &cfg, &inputs, &opts) {
                Ok(rep) => {
                    w(out, &trajectories::render(&rep))?;
                    Ok(0)
                }
                // Off by default is an answer, not a failure of the command.
                Err(e) if cfg.projects.is_empty() => {
                    w(out, &format!("{e}\n"))?;
                    Ok(1)
                }
                Err(e) => Err(e),
            }
        }
        Some(other) => Err(format!("unknown export command {other:?}\n{USAGE}")),
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn parse_options(args: &[String]) -> Result<Options, String> {
    let mut o = Options::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = |name: &str| it.next().cloned().ok_or_else(|| format!("{name} needs a value"));
        match a.as_str() {
            "--dry-run" => o.dry_run = true,
            "--include-tainted" => o.include_tainted = true,
            "--no-thinking" => o.no_thinking = true,
            "--out" => o.out = Some(val("--out")?.into()),
            "--project" => {
                let p = val("--project")?;
                o.only.push(config::canonical_dir(&p).unwrap_or(p));
            }
            "--since" => {
                let d = val("--since")?;
                let day = chrono::NaiveDate::parse_from_str(&d, "%Y-%m-%d")
                    .map_err(|_| format!("--since {d:?}: want YYYY-MM-DD"))?;
                o.since_ms = Some(day.and_hms_opt(0, 0, 0).expect("midnight").and_utc().timestamp_millis());
            }
            other => return Err(format!("unknown argument {other:?}\n{USAGE}")),
        }
    }
    Ok(o)
}
