//! Where local models come from. Marlowe's engine runs a GGUF from any of three places, and no
//! model server need run for any of them:
//!
//! | Source | Where | Ref |
//! |---|---|---|
//! | Ollama | the store's manifests and blobs (`OLLAMA_MODELS`, default `~/.ollama/models`) | `ollama/<name>`, as always |
//! | Hugging Face | the hub cache, `models--<org>--<name>/snapshots/<rev>/**/*.gguf` (`HF_HUB_CACHE`, `HF_HOME`) | `ollama/gguf/hf/<org>/<name>/<file>` |
//! | a folder | each path in `[models] dirs` of `settings.toml`, searched recursively; a path may be one file | `ollama/gguf/<file>` |
//!
//! A file model's ref is a stable name for the file, not its path (the path moves when a snapshot
//! does); the sources are searched for it when it is resolved. The `mtp-*.gguf` and `mmproj-*.gguf`
//! sidecars are never models of their own: they ride with the model they belong to
//! ([`crate::llama::eligible`]). The hub's `blobs` folder holds the same bytes under hash names and
//! is never read.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};

use crate::ollama_store;

/// A file model's id begins with this; an Ollama name never does.
pub const FILE_PREFIX: &str = "gguf/";

/// Files looked at per scan, so a disk of everything cannot make the picker hang.
const MAX_FILES: usize = 5000;
/// Folders deep a scan goes.
const MAX_DEPTH: usize = 8;

/// Where a model was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Ollama,
    HuggingFace,
    Folder,
}

impl Source {
    /// On the wire (`marlowe_model::Origin::source`).
    pub fn id(self) -> &'static str {
        match self {
            Source::Ollama => "ollama",
            Source::HuggingFace => "huggingface",
            Source::Folder => "folder",
        }
    }

    /// What the picker calls it.
    pub fn label(self) -> &'static str {
        match self {
            Source::Ollama => "From Ollama",
            Source::HuggingFace => "From Hugging Face",
            Source::Folder => "From folder",
        }
    }
}

/// A model found on disk, before its file is read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// The id in a model ref: an Ollama name, or `gguf/...`.
    pub id: String,
    pub source: Source,
    /// The name shown: the Ollama name, or the file's.
    pub display: String,
    /// An Ollama manifest, or the model file (its first shard).
    pub path: PathBuf,
    /// The Hugging Face repo, or the folder the file is in.
    pub detail: Option<String>,
    /// The model file's bytes; 0 for Ollama (read when it is resolved).
    pub size: u64,
}

/// The places models are read from. Clones share the model folders, so a folder added later
/// reaches the engine, the provider and the planner at once.
#[derive(Debug, Clone)]
pub struct Sources {
    ollama: PathBuf,
    hf: Vec<PathBuf>,
    dirs: Arc<RwLock<Vec<PathBuf>>>,
}

/// Only Ollama's store: no hub cache, no folders.
impl From<PathBuf> for Sources {
    fn from(ollama: PathBuf) -> Self {
        Sources::new(ollama, Vec::new(), Vec::new())
    }
}

impl From<&Path> for Sources {
    fn from(ollama: &Path) -> Self {
        ollama.to_path_buf().into()
    }
}

impl Sources {
    pub fn new(ollama: PathBuf, hf: Vec<PathBuf>, dirs: Vec<PathBuf>) -> Self {
        Self { ollama, hf, dirs: Arc::new(RwLock::new(dirs)) }
    }

    /// This machine's: Ollama's store and the hub cache from the environment, no folders yet.
    pub fn detect() -> Self {
        Self::new(ollama_store::store_root(), hf_hubs(), Vec::new())
    }

    /// The process-wide set every provider, engine and planner built by the binary shares; the
    /// binary fills in the folders from `settings.toml`.
    pub fn shared() -> Sources {
        static SHARED: OnceLock<Sources> = OnceLock::new();
        SHARED.get_or_init(Sources::detect).clone()
    }

    pub fn ollama_root(&self) -> &Path {
        &self.ollama
    }

    pub fn hub_caches(&self) -> &[PathBuf] {
        &self.hf
    }

    pub fn dirs(&self) -> Vec<PathBuf> {
        self.dirs.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Replaces the model folders (settings changed).
    pub fn set_dirs(&self, dirs: Vec<PathBuf>) {
        *self.dirs.write().unwrap_or_else(|e| e.into_inner()) = dirs;
    }

    /// Adds a folder or a model file; `false` when it was there already.
    pub fn add_dir(&self, path: PathBuf) -> bool {
        let mut d = self.dirs.write().unwrap_or_else(|e| e.into_inner());
        if d.contains(&path) {
            return false;
        }
        d.push(path);
        true
    }

    /// Every model: Ollama's, then the hub cache's, then the folders'.
    pub fn scan(&self) -> Vec<Found> {
        let mut out: Vec<Found> = ollama_store::list_in(&self.ollama)
            .into_iter()
            .map(|name| Found {
                display: name.clone(),
                id: name,
                source: Source::Ollama,
                path: PathBuf::new(),
                detail: Some(self.ollama.display().to_string()),
                size: 0,
            })
            .collect();
        out.extend(self.files());
        out
    }

    /// The file models only (Hugging Face, then folders).
    pub fn files(&self) -> Vec<Found> {
        let mut budget = MAX_FILES;
        let mut out = Vec::new();
        for hub in &self.hf {
            out.extend(scan_hub(hub, &mut budget));
        }
        let mut folder: Vec<Found> = Vec::new();
        for dir in self.dirs() {
            folder.extend(scan_folder(&dir, &mut budget));
        }
        out.extend(unique_ids(folder));
        out
    }

    /// The file model whose id is `id`.
    pub fn find_file(&self, id: &str) -> Option<Found> {
        id.starts_with(FILE_PREFIX).then(|| self.files().into_iter().find(|f| f.id == id)).flatten()
    }

    /// Where file models are looked for, for an error that names them.
    pub fn describe(&self) -> String {
        let mut places: Vec<String> = self.hf.iter().map(|h| format!("{} (Hugging Face cache)", h.display())).collect();
        places.extend(self.dirs().iter().map(|d| d.display().to_string()));
        if places.is_empty() {
            "no Hugging Face cache or model folder is set".into()
        } else {
            places.join(", ")
        }
    }
}

/// The Hugging Face hub cache: `HF_HUB_CACHE`, else `HF_HOME\hub`, else `~\.cache\huggingface\hub`.
pub fn hf_hubs() -> Vec<PathBuf> {
    hub_caches_with(|k| std::env::var(k).ok().filter(|v| !v.trim().is_empty()))
}

/// [`hf_hubs`] with the environment given.
pub fn hub_caches_with(get: impl Fn(&str) -> Option<String>) -> Vec<PathBuf> {
    if let Some(p) = get("HF_HUB_CACHE").or_else(|| get("HUGGINGFACE_HUB_CACHE")) {
        return vec![PathBuf::from(p)];
    }
    if let Some(h) = get("HF_HOME") {
        return vec![PathBuf::from(h).join("hub")];
    }
    match get("USERPROFILE").or_else(|| get("HOME")) {
        Some(home) => vec![PathBuf::from(home).join(".cache").join("huggingface").join("hub")],
        None => Vec::new(),
    }
}

/// A `.gguf` that is a model, not a sidecar: `mtp-*` and `mmproj*` ride with their model.
pub fn is_model_file(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.ends_with(".gguf") && !n.starts_with("mtp-") && !n.starts_with("mmproj")
}

/// `name-00002-of-00005.gguf`: its shard number and count.
fn shard(name: &str) -> Option<(u32, u32)> {
    let stem = name.strip_suffix(".gguf").or_else(|| name.strip_suffix(".GGUF"))?;
    let (head, count) = stem.rsplit_once("-of-")?;
    let (_, index) = head.rsplit_once('-')?;
    Some((index.parse().ok()?, count.parse().ok()?))
}

/// The model files under `dir`, as full paths (symlinks to files followed, folders not).
fn walk(dir: &Path, depth: usize, budget: &mut usize, out: &mut Vec<PathBuf>) {
    if depth > MAX_DEPTH {
        return;
    }
    let mut entries: Vec<_> = std::fs::read_dir(dir).into_iter().flatten().flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        if *budget == 0 {
            return;
        }
        let path = e.path();
        let Ok(kind) = e.file_type() else { continue };
        if kind.is_dir() {
            walk(&path, depth + 1, budget, out);
        } else if path.file_name().and_then(|n| n.to_str()).is_some_and(is_model_file) && path.is_file() {
            *budget -= 1;
            out.push(path);
        }
    }
}

/// What a model file is, for the picker: its bytes (all shards) and whether it is the first shard.
fn weigh(path: &Path) -> Option<u64> {
    let name = path.file_name()?.to_str()?;
    let size = std::fs::metadata(path).ok()?.len();
    let Some((index, count)) = shard(name) else { return Some(size) };
    if index != 1 {
        return None;
    }
    let (prefix, _) = name.rsplit_once("-00001-of-").unwrap_or((name, ""));
    let sibling = |n: u32| std::fs::metadata(path.with_file_name(format!("{prefix}-{n:05}-of-{count:05}.gguf"))).map_or(0, |m| m.len());
    Some(size + (2..=count).map(sibling).sum::<u64>())
}

fn stem(path: &Path) -> String {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("model");
    let name = name.strip_suffix(".gguf").or_else(|| name.strip_suffix(".GGUF")).unwrap_or(name);
    match name.rsplit_once("-00001-of-") {
        Some((head, _)) => head.to_string(),
        None => name.to_string(),
    }
}

/// `models--org--name` → `org/name`.
fn repo_of(dir_name: &str) -> Option<String> {
    let rest = dir_name.strip_prefix("models--")?;
    Some(rest.replace("--", "/")).filter(|r| !r.is_empty())
}

fn scan_hub(hub: &Path, budget: &mut usize) -> Vec<Found> {
    let mut repos: Vec<_> = std::fs::read_dir(hub).into_iter().flatten().flatten().collect();
    repos.sort_by_key(|e| e.file_name());
    let mut out = Vec::new();
    for repo in repos {
        let Some(name) = repo.file_name().to_str().and_then(repo_of) else { continue };
        let root = repo.path();
        let mut revs: Vec<_> = std::fs::read_dir(root.join("snapshots")).into_iter().flatten().flatten().collect();
        // The commit `main` points at first, then the newest: one file name is listed once.
        let main = std::fs::read_to_string(root.join("refs").join("main")).ok().map(|s| s.trim().to_string());
        revs.sort_by_key(|e| {
            let is_main = main.as_deref().is_some_and(|m| e.file_name().to_str() == Some(m));
            let modified = e.metadata().and_then(|m| m.modified()).ok();
            (!is_main, std::cmp::Reverse(modified))
        });
        let mut seen: Vec<PathBuf> = Vec::new();
        for rev in revs {
            let mut files = Vec::new();
            walk(&rev.path(), 0, budget, &mut files);
            for file in files {
                let Ok(rel) = file.strip_prefix(rev.path()) else { continue };
                if seen.iter().any(|s| s == rel) {
                    continue;
                }
                seen.push(rel.to_path_buf());
                let Some(size) = weigh(&file) else { continue };
                let rel_id = rel.with_file_name(stem(&file)).components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect::<Vec<_>>().join("/");
                out.push(Found {
                    id: format!("{FILE_PREFIX}hf/{name}/{rel_id}"),
                    source: Source::HuggingFace,
                    display: stem(&file),
                    path: file,
                    detail: Some(name.clone()),
                    size,
                });
            }
        }
    }
    out
}

fn scan_folder(dir: &Path, budget: &mut usize) -> Vec<Found> {
    let mut files = Vec::new();
    if dir.is_file() {
        if dir.file_name().and_then(|n| n.to_str()).is_some_and(is_model_file) {
            files.push(dir.to_path_buf());
        }
    } else {
        walk(dir, 0, budget, &mut files);
    }
    files
        .into_iter()
        .filter_map(|file| {
            let size = weigh(&file)?;
            Some(Found {
                id: format!("{FILE_PREFIX}{}", stem(&file)),
                source: Source::Folder,
                display: stem(&file),
                detail: file.parent().map(|p| p.display().to_string()),
                path: file,
                size,
            })
        })
        .collect()
}

/// Files of one name get ids that name where they are: every one of them carries a short hash of
/// its path, so an id never changes meaning when a same-named file appears or the first is deleted
/// (a saved `gguf/<name>~<hash>` is that file or nothing). A name that is unique stays plain.
fn unique_ids(mut found: Vec<Found>) -> Vec<Found> {
    let ids: Vec<String> = found.iter().map(|f| f.id.clone()).collect();
    for f in &mut found {
        if ids.iter().filter(|id| **id == f.id).count() > 1 {
            f.id = format!("{}~{:08x}", f.id, fnv(&f.path.display().to_string().to_ascii_lowercase()));
        }
    }
    found
}

fn fnv(s: &str) -> u32 {
    s.bytes().fold(0x811c_9dc5u32, |h, b| (h ^ u32::from(b)).wrapping_mul(0x0100_0193))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::llama::testkit::TempDir;

    pub(crate) fn put(path: &Path, bytes: &[u8]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    /// A hub cache with one repo holding a snapshot.
    pub(crate) fn hub_file(hub: &Path, repo: &str, rev: &str, rel: &str, bytes: &[u8]) -> PathBuf {
        let path = hub.join(format!("models--{}", repo.replace('/', "--"))).join("snapshots").join(rev).join(rel);
        put(&path, bytes);
        path
    }

    #[test]
    fn the_hub_cache_follows_the_environment_the_way_hugging_face_does() {
        let env = |pairs: &'static [(&'static str, &'static str)]| move |k: &str| pairs.iter().find(|p| p.0 == k).map(|p| p.1.to_string());
        assert_eq!(hub_caches_with(env(&[("HF_HUB_CACHE", "D:/hub"), ("HF_HOME", "E:/hf")])), vec![PathBuf::from("D:/hub")]);
        assert_eq!(hub_caches_with(env(&[("HF_HOME", "E:/hf")])), vec![PathBuf::from("E:/hf").join("hub")]);
        assert_eq!(
            hub_caches_with(env(&[("USERPROFILE", "C:/Users/m")])),
            vec![PathBuf::from("C:/Users/m").join(".cache").join("huggingface").join("hub")]
        );
        assert!(hub_caches_with(env(&[])).is_empty());
    }

    #[test]
    fn a_hub_cache_lists_each_snapshot_file_once_and_never_the_blobs() {
        let d = TempDir::new();
        let hub = d.path().join("hub");
        hub_file(&hub, "devvexus/Qwen3.8-27B-MS", "aaa", "MS2.7/Qwen3.8-27B-MS2.7.gguf", b"model");
        hub_file(&hub, "devvexus/Qwen3.8-27B-MS", "aaa", "mtp-Qwen3.8-27B-MS.gguf", b"mtp");
        hub_file(&hub, "devvexus/Qwen3.8-27B-MS", "aaa", "mmproj-F16.gguf", b"proj");
        hub_file(&hub, "devvexus/Qwen3.8-27B-MS", "aaa", "README.md", b"x");
        put(&hub.join("models--devvexus--Qwen3.8-27B-MS").join("blobs").join("abc123"), b"model");
        // An older revision of the same file is the same model.
        hub_file(&hub, "devvexus/Qwen3.8-27B-MS", "bbb", "MS2.7/Qwen3.8-27B-MS2.7.gguf", b"older");
        hub_file(&hub, "unsloth/Qwen3-8B-GGUF", "ccc", "Qwen3-8B-Q4_K_M.gguf", b"eight");
        let s = Sources::new(d.path().join("no-ollama"), vec![hub], Vec::new());
        let found = s.scan();
        let ids: Vec<&str> = found.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(ids, ["gguf/hf/devvexus/Qwen3.8-27B-MS/MS2.7/Qwen3.8-27B-MS2.7", "gguf/hf/unsloth/Qwen3-8B-GGUF/Qwen3-8B-Q4_K_M"]);
        assert!(found.iter().all(|f| f.source == Source::HuggingFace));
        assert_eq!((found[0].display.as_str(), found[0].detail.as_deref(), found[0].size), ("Qwen3.8-27B-MS2.7", Some("devvexus/Qwen3.8-27B-MS"), 5));
        assert_eq!(s.find_file(ids[1]).unwrap().size, 5);
        assert_eq!(s.find_file("gguf/hf/nope"), None);
        assert_eq!(s.find_file("qwen3:8b"), None, "an Ollama name is not a file model");
    }

    #[test]
    fn the_snapshot_main_points_at_wins_over_a_newer_one() {
        let d = TempDir::new();
        let hub = d.path().join("hub");
        hub_file(&hub, "o/r", "old", "m.gguf", b"from main");
        std::thread::sleep(std::time::Duration::from_millis(30));
        hub_file(&hub, "o/r", "new", "m.gguf", b"newer branch");
        put(&hub.join("models--o--r").join("refs").join("main"), b"old\n");
        let s = Sources::new(d.path().join("x"), vec![hub], Vec::new());
        assert_eq!(s.files()[0].size, 9);
    }

    #[test]
    fn model_folders_are_searched_recursively_and_sidecars_are_not_models() {
        let d = TempDir::new();
        let dir = d.path().join("models");
        put(&dir.join("a").join("Dusk-27B.gguf"), b"1");
        put(&dir.join("a").join("mtp-Dusk.gguf"), b"2");
        put(&dir.join("a").join("mmproj-Dusk-F16.gguf"), b"3");
        put(&dir.join("b").join("c").join("Other.GGUF"), b"4444");
        put(&dir.join("notes.txt"), b"x");
        let single = d.path().join("loose").join("Single.gguf");
        put(&single, b"55");
        let s = Sources::new(d.path().join("x"), Vec::new(), vec![dir.clone(), single.clone(), d.path().join("missing")]);
        let found = s.files();
        let ids: Vec<&str> = found.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(ids, ["gguf/Dusk-27B", "gguf/Other", "gguf/Single"]);
        assert!(found.iter().all(|f| f.source == Source::Folder));
        assert_eq!(found[0].detail.as_deref(), Some(dir.join("a").display().to_string().as_str()));
        // A folder added later reaches every clone.
        let clone = s.clone();
        assert!(s.add_dir(d.path().join("more")) && !s.add_dir(d.path().join("more")));
        assert_eq!(clone.dirs().len(), 4);
    }

    #[test]
    fn two_files_of_one_name_keep_distinct_stable_ids() {
        let d = TempDir::new();
        put(&d.path().join("one").join("Same.gguf"), b"1");
        put(&d.path().join("two").join("Same.gguf"), b"22");
        let s = Sources::new(d.path().join("x"), Vec::new(), vec![d.path().join("one"), d.path().join("two")]);
        let ids: Vec<String> = s.files().into_iter().map(|f| f.id).collect();
        assert!(ids[0].starts_with("gguf/Same~") && ids[1].starts_with("gguf/Same~") && ids[1] != ids[0], "{ids:?}");
        assert_eq!(s.files().into_iter().map(|f| f.id).collect::<Vec<_>>(), ids, "the same on every scan");
        // With one copy left the name is unique again, and plain.
        std::fs::remove_file(d.path().join("one").join("Same.gguf")).unwrap();
        let after: Vec<String> = s.files().into_iter().map(|f| f.id).collect();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0], "gguf/Same", "a name that is unique again is plain");
    }

    #[test]
    fn a_split_model_is_one_model_sized_by_all_its_shards() {
        let d = TempDir::new();
        put(&d.path().join("Big-00001-of-00003.gguf"), b"1111");
        put(&d.path().join("Big-00002-of-00003.gguf"), b"22");
        put(&d.path().join("Big-00003-of-00003.gguf"), b"3");
        let s = Sources::new(d.path().join("x"), Vec::new(), vec![d.path().to_path_buf()]);
        let found = s.files();
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].id.as_str(), found[0].size), ("gguf/Big", 7));
    }

    #[test]
    fn ollamas_store_comes_first_and_keeps_its_names() {
        let d = TempDir::new();
        let store = d.path().join("store");
        crate::llama::eligible::tests::store(&store, "qwen3.5:9b", serde_json::json!({}), &[(ollama_store::MODEL_MEDIA_TYPE, "gguf")]);
        put(&d.path().join("m").join("Folder.gguf"), b"1");
        let s = Sources::new(store, Vec::new(), vec![d.path().join("m")]);
        let all: Vec<(String, Source)> = s.scan().into_iter().map(|f| (f.id, f.source)).collect();
        assert_eq!(all, [("qwen3.5:9b".to_string(), Source::Ollama), ("gguf/Folder".to_string(), Source::Folder)]);
        assert_eq!(Source::HuggingFace.label(), "From Hugging Face");
    }
}
