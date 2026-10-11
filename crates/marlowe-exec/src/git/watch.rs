//! The ledger of pushed branches whose CI the Sentinel should watch: `<home>\ci_watch.json`.
//!
//! This side only writes (when a branch is pushed, a PR is opened, or `ci_status{watch}` asks);
//! the Sentinel's CI probe (`marlowe-sentinel/src/ci.rs`) only reads it. The two crates share the
//! file format and nothing else:
//!
//! ```json
//! {"v": 1, "watch": [{"repo": "C:\\work\\proj", "branch": "feat/x", "sha": "<40 hex>",
//!                     "remote": "origin", "slug": "owner/repo", "at_ms": 1760000000000}]}
//! ```
//!
//! One entry per (repo, branch): a newer push replaces the older. Entries older than a day are
//! dropped on write; the probe also ignores anything older than its own horizon.

use std::path::Path;
use std::sync::Mutex;

use serde_json::{json, Value};

/// File name inside the profile home.
pub const FILE: &str = "ci_watch.json";
const MAX_ENTRIES: usize = 64;
const KEEP_MS: i64 = 24 * 3_600_000;

static LEDGER: Mutex<()> = Mutex::new(());

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// One pushed branch to watch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Watch {
    pub repo: String,
    pub branch: String,
    pub sha: String,
    pub remote: String,
    /// `owner/repo` when the remote is a GitHub URL.
    pub slug: Option<String>,
}

/// Records `w` (replacing the repo's entry for that branch). Best effort: the ledger is a
/// convenience, so failures are returned for logging, never raised to the model.
pub fn register(home: &Path, w: &Watch) -> std::io::Result<()> {
    let _guard = LEDGER.lock().unwrap_or_else(|p| p.into_inner());
    let path = home.join(FILE);
    let now = now_ms();
    let mut entries: Vec<Value> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.get("watch").and_then(Value::as_array).cloned())
        .unwrap_or_default();
    entries.retain(|e| {
        let same = e.get("repo").and_then(Value::as_str) == Some(w.repo.as_str())
            && e.get("branch").and_then(Value::as_str) == Some(w.branch.as_str());
        let fresh = e.get("at_ms").and_then(Value::as_i64).is_some_and(|t| now - t < KEEP_MS);
        !same && fresh
    });
    entries.push(json!({
        "repo": w.repo, "branch": w.branch, "sha": w.sha, "remote": w.remote,
        "slug": w.slug, "at_ms": now,
    }));
    if entries.len() > MAX_ENTRIES {
        entries.drain(..entries.len() - MAX_ENTRIES);
    }
    std::fs::create_dir_all(home)?;
    let tmp = home.join(format!("{FILE}.tmp"));
    std::fs::write(&tmp, serde_json::to_vec_pretty(&json!({"v": 1, "watch": entries}))?)?;
    std::fs::rename(&tmp, &path)
}

/// The recorded entries, newest last.
pub fn read(home: &Path) -> Vec<Watch> {
    let Some(v) = std::fs::read_to_string(home.join(FILE)).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) else {
        return Vec::new();
    };
    v.get("watch")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|e| {
                    let s = |k: &str| e.get(k).and_then(Value::as_str).map(str::to_string);
                    Some(Watch {
                        repo: s("repo")?,
                        branch: s("branch")?,
                        sha: s("sha")?,
                        remote: s("remote").unwrap_or_else(|| "origin".into()),
                        slug: s("slug"),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `owner/repo` of a GitHub remote URL (`https://github.com/o/r.git`, `git@github.com:o/r.git`,
/// `ssh://git@github.com/o/r`), else `None`.
pub fn github_slug(url: &str) -> Option<String> {
    let u = url.trim();
    let rest = if let Some(r) = u.strip_prefix("git@") {
        let (host, path) = r.split_once(':')?;
        host.to_ascii_lowercase().contains("github").then_some(path)?
    } else if let Some(i) = u.find("://") {
        let after = &u[i + 3..];
        let after = after.rsplit_once('@').map_or(after, |(_, h)| h);
        let (host, path) = after.split_once('/')?;
        host.to_ascii_lowercase().contains("github").then_some(path)?
    } else {
        return None;
    };
    let rest = rest.trim_matches('/').trim_end_matches(".git");
    let mut parts = rest.split('/');
    let (owner, repo) = (parts.next()?, parts.next()?);
    (!owner.is_empty() && !repo.is_empty() && parts.next().is_none()).then(|| format!("{owner}/{repo}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_from_remote_urls() {
        for (url, want) in [
            ("https://github.com/o/r.git", Some("o/r")),
            ("https://github.com/o/r", Some("o/r")),
            ("https://user:tok@github.com/o/r.git", Some("o/r")),
            ("git@github.com:o/r.git", Some("o/r")),
            ("ssh://git@github.com/o/r.git", Some("o/r")),
            ("https://gitlab.com/o/r.git", None),
            ("C:\\repos\\origin.git", None),
            ("/srv/git/origin.git", None),
            ("https://github.com/o", None),
        ] {
            assert_eq!(github_slug(url).as_deref(), want, "{url}");
        }
    }

    #[test]
    fn register_replaces_the_same_branch_and_keeps_others() {
        let home = std::env::temp_dir().join(format!("marlowe-ciw-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let w = |branch: &str, sha: &str| Watch {
            repo: "C:\\p".into(),
            branch: branch.into(),
            sha: sha.into(),
            remote: "origin".into(),
            slug: Some("o/r".into()),
        };
        register(&home, &w("a", "1")).unwrap();
        register(&home, &w("b", "2")).unwrap();
        register(&home, &w("a", "3")).unwrap();
        let got = read(&home);
        assert_eq!(got.iter().map(|w| (w.branch.as_str(), w.sha.as_str())).collect::<Vec<_>>(), [("b", "2"), ("a", "3")]);
        let _ = std::fs::remove_dir_all(&home);
    }
}
