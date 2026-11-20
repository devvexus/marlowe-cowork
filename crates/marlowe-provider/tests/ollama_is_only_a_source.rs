//! Ollama is a source of model files and nothing else: no code in this crate talks to Ollama's
//! server (`/api/chat`, `/api/generate`, `/api/show`, `/api/tags`, `/api/ps`), runs the `ollama`
//! CLI, or touches Ollama's processes or settings. Every request goes to Marlowe's own engine.
//!
//! The provider has no Ollama endpoint at all, so a behavioural test could only show that one
//! request did not go there; this reads every source file, the way `spawn_audit` does, so a new
//! call cannot creep back in unseen.

use std::path::{Path, PathBuf};

/// What a line that talks to Ollama would contain.
const FORBIDDEN: &[&str] = &[
    "\"/api/chat\"",
    "\"/api/generate\"",
    "\"/api/show\"",
    "\"/api/tags\"",
    "\"/api/ps\"",
    "Command::new(\"ollama",
    "\"ollama app.exe\"",
    "OLLAMA_KV_CACHE_TYPE",
    "OLLAMA_NUM_PARALLEL",
    "OLLAMA_FLASH_ATTENTION",
    "setx",
    "taskkill",
];

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).expect("the source folder reads").flatten() {
        let p = e.path();
        if p.is_dir() {
            rust_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

#[test]
fn nothing_in_the_provider_talks_to_ollama_or_runs_it() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_files(&src, &mut files);
    assert!(files.len() > 10, "the walk found the sources ({})", files.len());
    let mut hits = Vec::new();
    for f in files {
        let text = std::fs::read_to_string(&f).unwrap();
        for (n, line) in text.lines().enumerate() {
            let code = line.trim_start();
            if code.starts_with("//") {
                continue;
            }
            if let Some(bad) = FORBIDDEN.iter().find(|b| code.contains(*b)) {
                hits.push(format!("{}:{}: {bad}", f.display(), n + 1));
            }
        }
    }
    assert!(hits.is_empty(), "Ollama is a model source only:\n{}", hits.join("\n"));
}
