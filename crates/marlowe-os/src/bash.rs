//! Which `bash` the harness runs. On Windows that is Git Bash, found the same way for the `bash`
//! tool, background jobs, monitors, hooks, `!cmd` pre-exec lines and `marlowe doctor`. It is never
//! the WSL launcher (`%SystemRoot%\System32\bash.exe`, or the `WindowsApps` alias) that sits first
//! on a default PATH: that runs the command inside a Linux distribution, not on this machine.
//! `MARLOWE_BASH` overrides the search. Elsewhere it is `bash` from PATH.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The bash to run, looked up once per process. `None` on Windows when Git Bash is not installed
/// (callers say so rather than falling back to PATH, which would find the WSL launcher).
pub fn git_bash() -> Option<PathBuf> {
    static FOUND: OnceLock<Option<PathBuf>> = OnceLock::new();
    FOUND
        .get_or_init(|| {
            if cfg!(windows) {
                find_git_bash(&|k| std::env::var_os(k), &|p| p.is_file())
            } else {
                Some(PathBuf::from("bash"))
            }
        })
        .clone()
}

/// True for the WSL launcher: a `bash.exe` under the Windows directory or in a `WindowsApps`
/// folder. Compared without case, as Windows paths are.
pub fn is_wsl_launcher(path: &Path, system_root: Option<&Path>) -> bool {
    let lower = |p: &Path| p.to_string_lossy().replace('/', "\\").to_ascii_lowercase();
    let p = lower(path);
    let windows = system_root.map(lower).unwrap_or_else(|| r"c:\windows".to_string());
    let windows = windows.trim_end_matches('\\');
    p.starts_with(&format!("{windows}\\")) || p.contains("\\windowsapps\\")
}

/// The Windows search, with the environment and the file check passed in so tests can stage it.
/// Order: `MARLOWE_BASH` (when it names a file that is not the WSL launcher), the usual install
/// roots (machine-wide, 32-bit, per-user), then a Git install found through `git.exe` on PATH
/// (scoop, a custom prefix), then a `bash.exe` on PATH that is not the WSL launcher.
pub fn find_git_bash(env: &dyn Fn(&str) -> Option<OsString>, exists: &dyn Fn(&Path) -> bool) -> Option<PathBuf> {
    let system_root = env("SystemRoot").map(PathBuf::from);
    let not_wsl = |p: &Path| !is_wsl_launcher(p, system_root.as_deref());

    if let Some(explicit) = env("MARLOWE_BASH").map(PathBuf::from) {
        if exists(&explicit) && not_wsl(&explicit) {
            return Some(explicit);
        }
    }

    let in_root = |root: &Path| -> Option<PathBuf> {
        [r"bin\bash.exe", r"usr\bin\bash.exe"].iter().map(|rel| root.join(rel)).find(|c| exists(c))
    };

    let mut roots: Vec<PathBuf> = Vec::new();
    for var in ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)", "LOCALAPPDATA"] {
        if let Some(v) = env(var) {
            roots.push(PathBuf::from(&v).join("Git"));
            roots.push(PathBuf::from(&v).join("Programs").join("Git"));
        }
    }
    roots.push(PathBuf::from(r"C:\Program Files\Git"));
    if let Some(found) = roots.iter().find_map(|r| in_root(r)) {
        return Some(found);
    }

    let path_dirs: Vec<PathBuf> = env("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    // git.exe lives in <root>\cmd, <root>\bin or <root>\mingw64\bin; bash is under <root>.
    for dir in &path_dirs {
        if !exists(&dir.join("git.exe")) {
            continue;
        }
        let found = dir.ancestors().skip(1).take(2).find_map(|root| in_root(root).filter(|c| not_wsl(c)));
        if found.is_some() {
            return found;
        }
    }
    path_dirs.iter().map(|d| d.join("bash.exe")).find(|c| exists(c) && not_wsl(c))
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};

    fn search(vars: &[(&str, &str)], files: &[&str]) -> Option<PathBuf> {
        let vars: HashMap<String, OsString> = vars.iter().map(|(k, v)| (k.to_string(), OsString::from(v))).collect();
        let files: HashSet<String> = files.iter().map(|f| f.to_ascii_lowercase()).collect();
        find_git_bash(&|k| vars.get(k).cloned(), &|p| files.contains(&p.to_string_lossy().to_ascii_lowercase()))
    }

    const WSL: &str = r"C:\Windows\System32\bash.exe";
    const ALIAS: &str = r"C:\Users\m\AppData\Local\Microsoft\WindowsApps\bash.exe";

    #[test]
    fn the_wsl_launcher_first_on_path_is_passed_over_for_git_bash() {
        let path = r"C:\Windows\System32;C:\Users\m\AppData\Local\Microsoft\WindowsApps;C:\Program Files\Git\cmd";
        let got = search(
            &[("SystemRoot", r"C:\Windows"), ("ProgramFiles", r"C:\Program Files"), ("PATH", path)],
            &[WSL, ALIAS, r"C:\Program Files\Git\cmd\git.exe", r"C:\Program Files\Git\bin\bash.exe"],
        );
        assert_eq!(got, Some(PathBuf::from(r"C:\Program Files\Git\bin\bash.exe")));
    }

    #[test]
    fn a_git_install_outside_the_usual_roots_is_found_through_git_on_path() {
        let path = r"C:\Windows\System32;D:\scoop\apps\git\current\cmd";
        let got = search(
            &[("SystemRoot", r"C:\Windows"), ("ProgramFiles", r"C:\Program Files"), ("PATH", path)],
            &[WSL, r"D:\scoop\apps\git\current\cmd\git.exe", r"D:\scoop\apps\git\current\usr\bin\bash.exe"],
        );
        assert_eq!(got, Some(PathBuf::from(r"D:\scoop\apps\git\current\usr\bin\bash.exe")));
        // git.exe in mingw64\bin: the root is two levels up.
        let got = search(
            &[("SystemRoot", r"C:\Windows"), ("PATH", r"E:\tools\git\mingw64\bin")],
            &[r"E:\tools\git\mingw64\bin\git.exe", r"E:\tools\git\bin\bash.exe"],
        );
        assert_eq!(got, Some(PathBuf::from(r"E:\tools\git\bin\bash.exe")));
    }

    #[test]
    fn with_only_the_wsl_launcher_there_is_no_bash() {
        let path = r"C:\Windows\System32;C:\Users\m\AppData\Local\Microsoft\WindowsApps";
        assert_eq!(search(&[("SystemRoot", r"C:\Windows"), ("PATH", path)], &[WSL, ALIAS]), None);
        // Even when the system directory is spelled another way, or SystemRoot is unset.
        assert_eq!(search(&[("PATH", r"c:/windows/system32")], &[r"c:/windows/system32\bash.exe"]), None);
    }

    #[test]
    fn marlowe_bash_wins_unless_it_is_missing_or_the_wsl_launcher() {
        let base = [("SystemRoot", r"C:\Windows"), ("ProgramFiles", r"C:\Program Files")];
        let git = r"C:\Program Files\Git\bin\bash.exe";
        let mut vars = base.to_vec();
        vars.push(("MARLOWE_BASH", r"D:\msys64\usr\bin\bash.exe"));
        assert_eq!(search(&vars, &[git, r"D:\msys64\usr\bin\bash.exe"]), Some(PathBuf::from(r"D:\msys64\usr\bin\bash.exe")));
        assert_eq!(search(&vars, &[git]), Some(PathBuf::from(git)), "a missing override falls back to the search");
        let mut vars = base.to_vec();
        vars.push(("MARLOWE_BASH", WSL));
        assert_eq!(search(&vars, &[git, WSL]), Some(PathBuf::from(git)), "the override cannot pick the WSL launcher");
    }

    #[test]
    fn a_per_user_git_install_is_found() {
        let got = search(
            &[("SystemRoot", r"C:\Windows"), ("LOCALAPPDATA", r"C:\Users\m\AppData\Local")],
            &[r"C:\Users\m\AppData\Local\Programs\Git\bin\bash.exe"],
        );
        assert_eq!(got, Some(PathBuf::from(r"C:\Users\m\AppData\Local\Programs\Git\bin\bash.exe")));
    }

    #[test]
    fn on_this_machine_bash_is_never_the_wsl_launcher() {
        if let Some(p) = git_bash() {
            let root = std::env::var_os("SystemRoot").map(PathBuf::from);
            assert!(!is_wsl_launcher(&p, root.as_deref()), "{}", p.display());
        }
    }
}
