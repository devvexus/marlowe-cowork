//! What a finished team task hands the user: the files its agents made, and its whole result.
//!
//! The run's own journal is not what the user asked for; the report an agent wrote is. A run in a
//! git workspace leaves its files in the run's worktree (merging them is an approval, offered, never
//! a condition of delivery); a run with no git workspace leaves them in its scratch folder, and they
//! are copied to `<home>\output\<run>` so the result outlives the sandbox and has one known place.
//! A return value too long for the relay is written whole to `result.md` beside the journal.

use std::path::{Path, PathBuf};

use marlowe_proto::{DiffSummary, RunId, SandboxKind};
use serde_json::Value;

use crate::sandbox::{safe_name, short_dir, Sandbox};

/// Most files listed for one run.
const MAX_FILES: usize = 60;
/// Deepest folder searched in a scratch sandbox.
const MAX_DEPTH: usize = 6;
/// Folders that are never output.
const SKIP_DIRS: [&str; 4] = ["node_modules", "target", "__pycache__", "venv"];

/// `<home>\output\<name>-<short run id>`: where a scratch run's files are kept.
pub fn output_dir(home: &Path, run: &RunId, name: &str) -> PathBuf {
    let short = short_dir(run.as_str());
    let short: String = short.chars().take(10).collect();
    home.join("output").join(format!("{}-{short}", safe_name(name)))
}

/// Files under `root` (relative, forward slashes), newest names last, bounded.
fn walk(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut dirs = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = dirs.pop() {
        let mut entries: Vec<_> = std::fs::read_dir(&dir).into_iter().flatten().flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let name = e.file_name().to_string_lossy().into_owned();
            let path = e.path();
            if path.is_dir() {
                if depth < MAX_DEPTH && !name.starts_with('.') && !SKIP_DIRS.contains(&name.as_str()) {
                    dirs.push((path, depth + 1));
                }
            } else if let Ok(rel) = path.strip_prefix(root) {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
            if out.len() >= MAX_FILES {
                return out;
            }
        }
    }
    out.sort();
    out
}

fn copy_into(from: &Path, to: &Path) -> bool {
    to.parent().is_none_or(|p| std::fs::create_dir_all(p).is_ok()) && std::fs::copy(from, to).is_ok()
}

/// The absolute paths of what the run's agents made. In a worktree, the files its diff names
/// (they stay there until the user merges); in a scratch folder, the files copied to the run's
/// output folder (the copies are what is named).
pub fn made_files(home: &Path, run: &RunId, name: &str, sandbox: &Sandbox, diff: Option<&DiffSummary>) -> Vec<String> {
    match sandbox.kind {
        SandboxKind::Worktree => diff
            .map(|d| d.files.iter().map(|f| sandbox.root.join(f)).filter(|p| p.is_file()).map(|p| p.display().to_string()).take(MAX_FILES).collect())
            .unwrap_or_default(),
        SandboxKind::Scratch => {
            let files = walk(&sandbox.root);
            if files.is_empty() {
                return Vec::new();
            }
            let dest = output_dir(home, run, name);
            files
                .iter()
                .filter(|rel| copy_into(&sandbox.root.join(rel), &dest.join(rel)))
                .map(|rel| dest.join(rel).display().to_string())
                .collect()
        }
        SandboxKind::None => Vec::new(),
    }
}

/// Writes `value` whole to `<dir>\result.md` (text as it is, anything else as JSON) when it is
/// longer than `shown` characters, and returns the file.
pub fn save_long_value(dir: &Path, value: &Value, shown: usize) -> Option<PathBuf> {
    let text = match value {
        Value::String(s) => s.clone(),
        other => serde_json::to_string_pretty(other).ok()?,
    };
    if text.chars().count() <= shown {
        return None;
    }
    let path = dir.join(if value.is_string() { "result.md" } else { "result.json" });
    std::fs::create_dir_all(dir).ok()?;
    std::fs::write(&path, text).ok()?;
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("marlowe-deliver-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn scratch(root: &Path) -> Sandbox {
        Sandbox { kind: SandboxKind::Scratch, root: root.to_path_buf(), origin: None, branch: None, base: None, repo_digest: None, frozen: vec![], target_dir: None }
    }

    #[test]
    fn a_scratch_runs_files_are_copied_to_its_output_folder_and_named_in_full() {
        let (home, root) = (temp("h1"), temp("r1"));
        std::fs::create_dir_all(root.join("research")).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join("research").join("kimi.md"), "# Report").unwrap();
        std::fs::write(root.join(".git").join("x"), "no").unwrap();
        let run = RunId("0123456789abcdef0123456789abcdef".into());
        let files = made_files(&home, &run, "Web Research", &scratch(&root), None);
        assert_eq!(files.len(), 1, "{files:?}");
        let path = PathBuf::from(&files[0]);
        assert!(path.starts_with(home.join("output")) && path.ends_with("research/kimi.md") || path.ends_with("research\\kimi.md"), "{files:?}");
        assert!(path.is_absolute() && std::fs::read_to_string(&path).unwrap() == "# Report");
        assert!(root.join("research").join("kimi.md").is_file(), "the sandbox keeps its own copy");
        assert!(made_files(&home, &run, "x", &scratch(&temp("r1b")), None).is_empty());
    }

    #[test]
    fn a_worktree_runs_new_files_are_named_where_they_are() {
        let (home, root) = (temp("h2"), temp("r2"));
        std::fs::create_dir_all(root.join("research")).unwrap();
        std::fs::write(root.join("research").join("a.md"), "x").unwrap();
        let sb = Sandbox { kind: SandboxKind::Worktree, ..scratch(&root) };
        let diff = DiffSummary { branch: "crew/x".into(), files: vec!["research/a.md".into(), "gone.md".into()], insertions: 1, deletions: 0 };
        let files = made_files(&home, &RunId("r".into()), "n", &sb, Some(&diff));
        assert_eq!(files.len(), 1, "a path in the diff that is not a file is left out: {files:?}");
        assert!(PathBuf::from(&files[0]).starts_with(&root));
        assert!(!home.join("output").exists(), "nothing is copied for a worktree");
    }

    #[test]
    fn a_long_result_is_written_whole_and_a_short_one_is_not() {
        let dir = temp("r3");
        let long = Value::String("line\n".repeat(1000));
        let path = save_long_value(&dir, &long, 500).unwrap();
        assert!(path.ends_with("result.md") && std::fs::read_to_string(&path).unwrap().len() == 5000);
        assert!(save_long_value(&dir, &Value::String("short".into()), 500).is_none());
        let obj = serde_json::json!({"k": "v".repeat(900)});
        assert!(save_long_value(&dir, &obj, 500).unwrap().ends_with("result.json"));
    }
}
