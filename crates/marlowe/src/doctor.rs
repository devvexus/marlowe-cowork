//! `marlowe doctor`: what this machine has and what is missing. Never prints a secret. Owned by
//! track K from M4.6 (U13).

use std::path::{Path, PathBuf};
use std::time::Duration;

use marlowe_brains::{api_key, detect_ollama, KeyringStore, OLLAMA_ADDR};
use marlowe_model::ProviderKind;

use crate::daemon_ctl;

fn on_path(names: &[&str]) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .flat_map(|d| names.iter().map(move |n| d.join(n)))
        .find(|p| p.is_file())
}

fn line(ok: bool, what: &str, detail: impl std::fmt::Display) {
    println!("{} {what:<14} {detail}", if ok { "ok  " } else { "MISS" });
}

pub fn run(home: &Path) -> i32 {
    println!("home           {}", home.display());
    match daemon_ctl::live(home) {
        Some(i) => line(true, "daemon", format!("running on port {}", i.port)),
        None => line(false, "daemon", "not running (starts on demand)"),
    }
    let ollama = detect_ollama(OLLAMA_ADDR, Duration::from_millis(800));
    line(ollama, "ollama", if ollama { OLLAMA_ADDR } else { "not reachable" });
    let key = api_key(ProviderKind::OpenRouter, &KeyringStore::new()).is_some();
    line(key, "openrouter key", if key { "present" } else { "none (env or Library)" });

    let wt = on_path(&["wt.exe"]);
    line(wt.is_some(), "windows term.", wt.map(|p| p.display().to_string()).unwrap_or_else(|| "wt.exe not found".into()));
    let ps = on_path(&["powershell.exe", "pwsh.exe"]);
    line(ps.is_some(), "powershell", ps.map(|p| p.display().to_string()).unwrap_or_default());
    let py = on_path(&["python.exe", "python3.exe", "python", "python3"]);
    line(py.is_some(), "python", py.map(|p| p.display().to_string()).unwrap_or_default());
    let bash = on_path(&["bash.exe", "bash"]);
    line(bash.is_some(), "bash", bash.map(|p| p.display().to_string()).unwrap_or_default());

    match crossterm::terminal::size() {
        Ok((w, h)) => line(w >= 120 && h >= 30, "terminal", format!("{w}x{h} (needs 120x30)")),
        Err(_) => line(false, "terminal", "no terminal attached"),
    }
    println!("braille        \u{2801}\u{2803}\u{2807}\u{280f}\u{281f}\u{283f}\u{287f}\u{28ff}  (blanks mean the font lacks U+2800-28FF)");
    0
}
