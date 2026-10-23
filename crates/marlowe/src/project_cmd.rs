//! `marlowe project`: inspect projects from their files, and answer or stop them through the
//! daemon. The reading commands need no daemon: a project is `project.json` under
//! `<home>\projects\<id>\`, so the state can be checked before any screen exists.

use std::path::Path;
use std::time::Duration;

use marlowe_project::{overview, paint, paint_overview, view_tree, ProjectState, ProjectStore, Registry};
use marlowe_proto::{paths, Event, Request};

use crate::crew_cmd::{connect, reply, send};

const OP_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_WIDTH: usize = 100;

/// The text `marlowe help` and a wrong command print.
pub const PROJECT_USAGE: &str = "\
usage: marlowe project [list [--json] | show <id> | view <id> [--width N] | overview
                       | card <id> | answer <id> <approve|keep_researching|steer|cancel> [text...]
                       | checkin <id> | cancel <id> | reopen <id> <stage>]
<id> is the project id or a unique prefix of it (`list` shows them).
`show` prints the project's JSON as saved; `view` draws it with the same renderer every screen uses;
`card` prints the open plan or checkin card. `answer`, `checkin`, `cancel` and `reopen` go through the daemon.";

type CmdResult = Result<i32, String>;

fn store(home: &Path) -> Result<ProjectStore, String> {
    let root = home.join("projects");
    std::fs::create_dir_all(&root).map_err(|e| format!("cannot create {}: {e}", root.display()))?;
    ProjectStore::open(&root, &paths::cowork_db(home)).map_err(|e| e.to_string())
}

/// The project an id or a unique prefix of it names.
fn resolve(store: &ProjectStore, arg: Option<&String>) -> Result<String, String> {
    let want = arg.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).ok_or_else(|| PROJECT_USAGE.to_string())?;
    if store.exists(&want) {
        return Ok(want);
    }
    let rows = store.list(500).map_err(|e| e.to_string())?;
    let found: Vec<&str> = rows.iter().map(|r| r.id.as_str()).filter(|id| id.starts_with(&want) || id.trim_start_matches("p_").starts_with(&want)).collect();
    match found.as_slice() {
        [one] => Ok((*one).to_string()),
        [] => Err(format!("no project {want}. `marlowe project list` shows them.")),
        many => Err(format!("{want} matches {} projects: {}", many.len(), many.join(", "))),
    }
}

/// `marlowe project ...`.
pub fn run(home: &Path, args: &[String]) -> CmdResult {
    let sub = args.first().map(String::as_str).unwrap_or("list");
    let rest = args.get(1..).unwrap_or_default();
    match sub {
        "list" => list(home, rest.iter().any(|a| a == "--json")),
        "show" => {
            let s = store(home)?;
            let id = resolve(&s, rest.first())?;
            let st = s.load(&id).map_err(|e| e.to_string())?;
            println!("{}", serde_json::to_string_pretty(&st).map_err(|e| e.to_string())?);
            Ok(0)
        }
        "view" => {
            let s = store(home)?;
            let id = resolve(&s, rest.first())?;
            let width = rest.iter().position(|a| a == "--width").and_then(|i| rest.get(i + 1)).and_then(|w| w.parse().ok()).unwrap_or(DEFAULT_WIDTH);
            print!("{}", view_text(&s.load(&id).map_err(|e| e.to_string())?, width));
            Ok(0)
        }
        "overview" => {
            let s = store(home)?;
            print!("{}", overview_text(&s)?);
            Ok(0)
        }
        "card" => {
            let s = store(home)?;
            let id = resolve(&s, rest.first())?;
            let st = s.load(&id).map_err(|e| e.to_string())?;
            match st.open_gate() {
                Some(g) => {
                    println!("{}", serde_json::to_string_pretty(&serde_json::json!({"gate": g.id, "kind": g.kind.as_str(), "options": g.options, "card": g.payload})).map_err(|e| e.to_string())?);
                    Ok(0)
                }
                None => Err(format!("{id} has no open card ({})", st.status.as_str())),
            }
        }
        "answer" => {
            let s = store(home)?;
            let id = resolve(&s, rest.first())?;
            let choice = rest.get(1).cloned().ok_or_else(|| PROJECT_USAGE.to_string())?;
            let text = rest.get(2..).filter(|t| !t.is_empty()).map(|t| t.join(" "));
            let st = s.load(&id).map_err(|e| e.to_string())?;
            let gate = st.open_gate().map(|g| g.id.clone()).ok_or_else(|| format!("{id} has no open card ({})", st.status.as_str()))?;
            ask(home, Request::AnswerProjectGate { project: id.clone(), gate, choice: choice.clone(), text }, &format!("{id}: {choice}"))
        }
        "checkin" => {
            let s = store(home)?;
            let id = resolve(&s, rest.first())?;
            ask(home, Request::CheckinProject { project: id.clone() }, &format!("{id}: a checkin is requested"))
        }
        "cancel" => {
            let s = store(home)?;
            let id = resolve(&s, rest.first())?;
            ask(home, Request::CancelProject { project: id.clone() }, &format!("{id}: cancelled"))
        }
        "reopen" => {
            let s = store(home)?;
            let id = resolve(&s, rest.first())?;
            let stage = rest.get(1).cloned().ok_or_else(|| PROJECT_USAGE.to_string())?;
            ask(home, Request::ReopenProject { project: id.clone(), phase: stage.clone() }, &format!("{id}: reopened from {stage}"))
        }
        _ => Err(PROJECT_USAGE.to_string()),
    }
}

fn list(home: &Path, json: bool) -> CmdResult {
    let s = store(home)?;
    let mut rows = s.list(200).map_err(|e| e.to_string())?;
    if rows.is_empty() {
        // The index is a cache of the files: a fresh database over existing projects is rebuilt.
        let _ = s.rebuild_index();
        rows = s.list(200).map_err(|e| e.to_string())?;
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&rows).map_err(|e| e.to_string())?);
        return Ok(0);
    }
    if rows.is_empty() {
        println!("No projects yet.");
    }
    for r in rows {
        println!("{}  {:<17} {:<11} {:<14} {}", r.id, r.status, r.kind, r.phase, r.title);
    }
    Ok(0)
}

/// A project drawn as text by the shared renderer: its title line, the rows of its items.
pub(crate) fn view_text(st: &ProjectState, width: usize) -> String {
    let mut out = format!("{} \"{}\" [{}] {} rev {}\n", st.id, st.title, st.kind, st.status.as_str(), st.rev);
    out.push_str(&paint(&view_tree(st, marlowe_project::ids::now_ms()), width));
    out
}

fn overview_text(s: &ProjectStore) -> Result<String, String> {
    let rows = s.list(200).map_err(|e| e.to_string())?;
    let states: Vec<ProjectState> = rows.iter().filter_map(|r| s.load(&r.id).ok()).collect();
    let refs: Vec<&ProjectState> = states.iter().collect();
    Ok(paint_overview(&overview(&refs, &Registry::builtin(), marlowe_project::ids::now_ms())))
}

/// Sends `req` and waits for the daemon to take it. Requests with no direct reply are
/// confirmed by a round trip (`ListProjects`) that arrives after the daemon handled them.
fn ask(home: &Path, req: Request, done: &str) -> CmdResult {
    let mut c = connect(home)?;
    send(&mut c, &req)?;
    send(&mut c, &Request::ListProjects)?;
    reply(&mut c, OP_TIMEOUT, |e| matches!(e, Event::ProjectList { .. }).then_some(()))?;
    println!("{done}");
    Ok(0)
}

#[cfg(test)]
mod tests {
    use marlowe_project::{parse_plan, ProjectState};
    use serde_json::json;

    use super::*;

    fn home(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("marlowe-projcmd-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn make(home: &Path, id: &str) -> ProjectState {
        let reg = Registry::builtin();
        let plan = parse_plan(
            &json!({
                "objective": "Find the newest open models from one vendor and say what is confirmed.",
                "questions": ["What is the newest model?", "Is a successor announced?"],
                "fields": [{"key": "model_name"}]
            }),
            &reg,
            1_792_756_800_000,
        )
        .unwrap();
        let st = ProjectState::new(id.into(), plan, &reg, 1_792_756_800_000);
        store(home).unwrap().create(&st).unwrap();
        st
    }

    #[test]
    fn an_id_or_a_unique_prefix_names_a_project_and_an_ambiguous_one_says_so() {
        let h = home("resolve");
        make(&h, "p_7f3a91c2");
        make(&h, "p_7f3b0000");
        let s = store(&h).unwrap();
        let arg = |t: &str| Some(t.to_string());
        assert_eq!(resolve(&s, arg("p_7f3a91c2").as_ref()).unwrap(), "p_7f3a91c2");
        assert_eq!(resolve(&s, arg("7f3a").as_ref()).unwrap(), "p_7f3a91c2");
        assert!(resolve(&s, arg("p_7f3").as_ref()).unwrap_err().contains("matches 2 projects"));
        assert!(resolve(&s, arg("zz").as_ref()).unwrap_err().contains("no project zz"));
        assert!(resolve(&s, None).unwrap_err().starts_with("usage: marlowe project"));
    }

    #[test]
    fn view_draws_the_saved_project_through_the_shared_renderer_and_overview_rolls_up() {
        let h = home("view");
        let st = make(&h, "p_7f3a91c2");
        let text = view_text(&st, 100);
        assert!(text.starts_with("p_7f3a91c2 \"") && text.contains("awaiting_plan") && text.contains("What is the newest model?"), "{text}");
        let o = overview_text(&store(&h).unwrap()).unwrap();
        assert!(o.starts_with("1 projects: 1 awaiting_plan") && o.contains("p_7f3a91c2"), "{o}");
    }

    #[test]
    fn show_list_and_card_read_the_files_and_an_unknown_command_prints_the_usage() {
        let h = home("show");
        make(&h, "p_7f3a91c2");
        assert_eq!(run(&h, &["list".into()]).unwrap(), 0);
        assert_eq!(run(&h, &["show".into(), "7f3a".into()]).unwrap(), 0);
        assert!(run(&h, &["card".into(), "7f3a".into()]).unwrap_err().contains("no open card"));
        assert!(run(&h, &["frobnicate".into()]).unwrap_err().contains("usage: marlowe project"));
        assert!(run(&h, &["answer".into(), "7f3a".into(), "approve".into()]).unwrap_err().contains("no open card"));
    }
}
