//! The things the client does on this machine for the reducer: the status-line command, the file
//! walk behind `@`, clipboard and file images, writing an export, finding instruction files,
//! and handing the terminal to an editor.
//!
//! Every child starts through `marlowe_os::child` (scrubbed environment, Job Object). The run loop
//! calls [`spawn`] for each queued [`Effect`]; the answer returns on the same channel as the
//! daemon's events.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use marlowe_os::child::{self, ChildSpec};

use crate::attach;
use crate::coding::{Effect, ImageSource, InstructionFile, LocalEvent};
use crate::state::ClientEvent;

/// The status-line command gets this long.
pub const STATUSLINE_TIMEOUT: Duration = Duration::from_secs(3);
/// Files the `@` picker indexes at most.
pub const INDEX_FILES: usize = 50_000;
/// The walk gives up after this long and keeps what it has.
pub const INDEX_TIME: Duration = Duration::from_secs(5);

/// Runs `effect` on its own thread. `OpenEditor` is the run loop's: it owns the terminal.
pub fn spawn(effect: Effect, tx: Sender<ClientEvent>) {
    let _ = std::thread::Builder::new().name("marlowe-tui-local".into()).spawn(move || {
        if let Some(ev) = run(effect) {
            let _ = tx.send(ClientEvent::Local(ev));
        }
    });
}

/// Runs an effect to completion. `None` for the ones that are not a worker's.
pub fn run(effect: Effect) -> Option<LocalEvent> {
    Some(match effect {
        Effect::Statusline { command, input, cwd } => {
            LocalEvent::Statusline(run_statusline(&command, &input, cwd.as_deref().map(Path::new), STATUSLINE_TIMEOUT))
        }
        Effect::IndexFiles { root } => {
            let files = index_files(Path::new(&root), INDEX_FILES, INDEX_TIME);
            LocalEvent::FileIndex { root, files }
        }
        Effect::ClipboardImage => LocalEvent::Image { source: ImageSource::Clipboard, result: clipboard_image() },
        Effect::ReadImage { path } => {
            let result = read_image(Path::new(&path));
            LocalEvent::Image { source: ImageSource::Path(path), result }
        }
        Effect::WriteFile { path, text } => {
            let p = Path::new(&path);
            let result = p
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|()| std::fs::write(p, text))
                .map_err(|e| e.to_string());
            LocalEvent::Written { path, result }
        }
        Effect::FindInstructions { workspace } => LocalEvent::Instructions(find_instructions(workspace.as_deref().map(Path::new))),
        Effect::ReadFile { path } => {
            let result = read_text(Path::new(&path), READ_CAP);
            LocalEvent::FileText { path, result }
        }
        Effect::ListReports => LocalEvent::Reports(crate::report::find_reports(&crate::report::report_dirs())),
        Effect::ReadReport { path } => {
            let result = read_text(Path::new(&path), READ_CAP);
            LocalEvent::ReportText { path, result }
        }
        Effect::OpenEditor { .. } => return None,
    })
}

/// Largest text file the client reads for a panel.
pub const READ_CAP: u64 = 2 * 1024 * 1024;

/// A text file's contents, at most `cap` bytes (the rest is cut at a character).
pub fn read_text(path: &Path, cap: u64) -> Result<String, String> {
    let mut bytes = Vec::new();
    std::fs::File::open(path).and_then(|f| f.take(cap).read_to_end(&mut bytes)).map_err(|e| e.to_string())?;
    Ok(String::from_utf8_lossy(&bytes).trim_start_matches('\u{feff}').to_string())
}

// ---- status line -----------------------------------------------------------------------------------

fn shell_command(command: &str) -> Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let mut c = Command::new("cmd.exe");
        c.args(["/D", "/S", "/C"]).raw_arg(format!("\"{command}\""));
        c
    }
    #[cfg(not(windows))]
    {
        let mut c = Command::new("sh");
        c.arg("-c").arg(command);
        c
    }
}

/// Runs `command` through the shell with `input` on stdin; the first line it prints, with escape
/// sequences removed. A command that fails, prints nothing or runs over `timeout` is an error.
pub fn run_statusline(command: &str, input: &str, cwd: Option<&Path>, timeout: Duration) -> Result<String, String> {
    let mut cmd = shell_command(command);
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    let mut spec = ChildSpec::new();
    if let Some(d) = cwd.filter(|d| d.is_dir()) {
        spec = spec.with_cwd(d);
    }
    let mut proc = child::spawn(&mut cmd, &spec).map_err(|e| e.to_string())?;
    if let Some(mut stdin) = proc.child_mut().stdin.take() {
        let input = input.to_string();
        std::thread::spawn(move || {
            let _ = stdin.write_all(input.as_bytes());
        });
    }
    let mut stdout = proc.child_mut().stdout.take();
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(o) = stdout.as_mut() {
            let _ = o.take(64 * 1024).read_to_end(&mut buf);
        }
        buf
    });
    let started = Instant::now();
    let code = loop {
        match proc.child_mut().try_wait() {
            Ok(Some(s)) => break s.code().unwrap_or(-1),
            Ok(None) if started.elapsed() >= timeout => {
                proc.kill_tree();
                let _ = proc.child_mut().wait();
                return Err("timed out".into());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => return Err(e.to_string()),
        }
    };
    let out = reader.join().unwrap_or_default();
    if code != 0 {
        return Err(format!("exited {code}"));
    }
    let text = String::from_utf8_lossy(&out);
    let line = strip_ansi(text.lines().find(|l| !strip_ansi(l).trim().is_empty()).unwrap_or(""));
    Ok(line.trim().to_string())
}

/// `ESC [ ... final` and `ESC ] ... BEL/ST` removed. The palette is locked, so a status line's own
/// colours are not shown.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match it.peek() {
            Some('[') => {
                it.next();
                for n in it.by_ref() {
                    if ('@'..='~').contains(&n) {
                        break;
                    }
                }
            }
            Some(']') => {
                it.next();
                while let Some(n) = it.next() {
                    if n == '\u{7}' {
                        break;
                    }
                    if n == '\u{1b}' {
                        it.next();
                        break;
                    }
                }
            }
            Some(_) => {
                it.next();
            }
            None => {}
        }
    }
    out
}

// ---- `@` -------------------------------------------------------------------------------------------

/// Files under `root`, relative with `/`, honouring `.gitignore`, never `.git`.
pub fn index_files(root: &Path, limit: usize, budget: Duration) -> Vec<String> {
    let started = Instant::now();
    let mut files = Vec::new();
    let walker = ignore::WalkBuilder::new(root)
        .hidden(false)
        .require_git(false)
        .git_global(false)
        .filter_entry(|e| e.file_name() != ".git")
        .build();
    for entry in walker.flatten() {
        if files.len() >= limit || started.elapsed() > budget {
            break;
        }
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        if let Ok(rel) = entry.path().strip_prefix(root) {
            files.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
    files.sort();
    files
}

// ---- images ----------------------------------------------------------------------------------------

/// Reads an image file: its MIME type from the name, checked against its first bytes.
pub fn read_image(path: &Path) -> Result<(String, Vec<u8>), String> {
    let name = path.to_string_lossy();
    let mime = attach::image_mime(&name).ok_or("not an image file")?;
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if meta.len() as usize > attach::MAX_IMAGE_BYTES {
        return Err("over 12 MB".into());
    }
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let looks_right = match mime {
        "image/png" => bytes.starts_with(b"\x89PNG"),
        "image/jpeg" => bytes.starts_with(&[0xFF, 0xD8]),
        "image/gif" => bytes.starts_with(b"GIF8"),
        "image/webp" => bytes.len() > 12 && &bytes[8..12] == b"WEBP",
        "image/bmp" => bytes.starts_with(b"BM"),
        _ => true,
    };
    if !looks_right {
        return Err("the file is not the image its name says".into());
    }
    Ok((mime.to_string(), bytes))
}

/// PowerShell that saves the clipboard's image as PNG to `$env:MARLOWE_CLIP_OUT` (exit 0), prints
/// the path of an image file copied in Explorer (exit 4), or finds none (exit 3). CF_DIBV5 and
/// CF_DIB come through `GetImage`; a registered `PNG` format is copied as it is.
pub const CLIPBOARD_SCRIPT: &str = r#"Add-Type -AssemblyName System.Windows.Forms,System.Drawing
$d=[Windows.Forms.Clipboard]::GetDataObject()
if($null -eq $d){exit 3}
if($d.GetDataPresent('PNG')){$s=$d.GetData('PNG');$f=[IO.File]::Create($env:MARLOWE_CLIP_OUT);$s.CopyTo($f);$f.Close();exit 0}
if($d.GetDataPresent([Windows.Forms.DataFormats]::Bitmap) -or $d.GetDataPresent([Windows.Forms.DataFormats]::Dib)){$i=[Windows.Forms.Clipboard]::GetImage();if($i){$i.Save($env:MARLOWE_CLIP_OUT,[Drawing.Imaging.ImageFormat]::Png);exit 0}}
if($d.GetDataPresent([Windows.Forms.DataFormats]::FileDrop)){foreach($p in $d.GetData([Windows.Forms.DataFormats]::FileDrop)){if($p -match '\.(png|jpe?g|gif|webp|bmp)$'){[Console]::Out.Write($p);exit 4}}}
exit 3"#;

#[cfg(windows)]
fn clipboard_image() -> Result<(String, Vec<u8>), String> {
    let out = std::env::temp_dir().join(format!("marlowe-clip-{}.png", std::process::id()));
    let _ = std::fs::remove_file(&out);
    let mut cmd = Command::new("powershell.exe");
    cmd.args(["-NoProfile", "-NonInteractive", "-STA", "-Command", CLIPBOARD_SCRIPT]);
    let spec = ChildSpec::new().with_env([("MARLOWE_CLIP_OUT".to_string(), out.display().to_string())]);
    let limits = child::RunLimits { timeout: Duration::from_secs(10), max_output_bytes: 4096 };
    let r = child::run(cmd, &spec, limits, &|| false).map_err(|e| e.to_string())?;
    match r.code() {
        0 => {
            let bytes = std::fs::read(&out).map_err(|e| e.to_string());
            let _ = std::fs::remove_file(&out);
            Ok(("image/png".to_string(), bytes?))
        }
        4 => read_image(Path::new(String::from_utf8_lossy(&r.stdout).trim())),
        3 => Err(String::new()),
        _ if r.timed_out => Err("the clipboard did not answer".into()),
        n => Err(format!("clipboard helper exited {n}")),
    }
}

#[cfg(not(windows))]
fn clipboard_image() -> Result<(String, Vec<u8>), String> {
    let attempts: [(&str, &[&str]); 2] =
        [("wl-paste", &["--type", "image/png"]), ("xclip", &["-selection", "clipboard", "-t", "image/png", "-o"])];
    for (prog, args) in attempts {
        let mut cmd = Command::new(prog);
        cmd.args(args);
        let limits = child::RunLimits { timeout: Duration::from_secs(5), max_output_bytes: attach::MAX_IMAGE_BYTES };
        if let Ok(r) = child::run(cmd, &ChildSpec::new(), limits, &|| false) {
            if r.code() == 0 && r.stdout.starts_with(b"\x89PNG") {
                return Ok(("image/png".into(), r.stdout));
            }
        }
    }
    Err(String::new())
}

// ---- instruction files -----------------------------------------------------------------------------

const PROJECT_FILES: [&str; 3] = ["MARLOWE.md", "AGENTS.md", "CLAUDE.md"];
const LOCAL_FILES: [&str; 2] = ["MARLOWE.local.md", "CLAUDE.local.md"];

/// The instruction files `/memory` offers: the project's (from the repository root down to the
/// workspace) that exist, `MARLOWE.md` in the workspace even when it does not, and the user's.
pub fn find_instructions(workspace: Option<&Path>) -> Vec<InstructionFile> {
    let mut out = Vec::new();
    let mut add = |path: PathBuf, scope: &str, always: bool| {
        let exists = path.is_file();
        if exists || always {
            out.push(InstructionFile { path: path.display().to_string().replace('\\', "/"), scope: scope.into(), exists });
        }
    };
    if let Some(ws) = workspace {
        // From the repository root (or the workspace itself) down.
        let mut chain: Vec<&Path> = Vec::new();
        for a in ws.ancestors() {
            chain.push(a);
            if a.join(".git").exists() {
                break;
            }
        }
        if chain.len() > 8 || !chain.last().is_some_and(|r| r.join(".git").exists()) {
            chain = vec![ws];
        }
        for dir in chain.iter().rev() {
            for f in PROJECT_FILES {
                add(dir.join(f), "project", *dir == ws && f == "MARLOWE.md");
            }
            for f in LOCAL_FILES {
                add(dir.join(f), "local", false);
            }
        }
    }
    add(marlowe_proto::paths::cowork_home().join("MARLOWE.md"), "user", true);
    if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
        add(Path::new(&home).join(".claude").join("CLAUDE.md"), "user", false);
    }
    out
}

// ---- the editor ------------------------------------------------------------------------------------

/// `$VISUAL`, then `$EDITOR`, else Notepad (Windows) or vi.
pub fn editor_command(env: impl Fn(&str) -> Option<String>) -> String {
    ["VISUAL", "EDITOR"]
        .iter()
        .find_map(|k| env(k).map(|v| v.trim().to_string()).filter(|v| !v.is_empty()))
        .unwrap_or_else(|| if cfg!(windows) { "notepad".into() } else { "vi".into() })
}

/// Splits an editor setting like `code --wait` or `"C:\Program Files\x\ed.exe" -w` into words.
pub fn split_command(s: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut any = false;
    for c in s.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                any = true;
            }
            (None, c) if c.is_whitespace() => {
                if any || !cur.is_empty() {
                    words.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            (None, c) => cur.push(c),
        }
    }
    if any || !cur.is_empty() {
        words.push(cur);
    }
    words
}

/// Opens `path` in the editor and waits for it. The caller has given the terminal back.
pub fn run_editor(path: &str) -> Result<(), String> {
    let words = split_command(&editor_command(|k| std::env::var(k).ok()));
    let (prog, args) = words.split_first().ok_or("no editor")?;
    let mut cmd = Command::new(prog);
    cmd.args(args).arg(path).stdin(Stdio::inherit()).stdout(Stdio::inherit()).stderr(Stdio::inherit());
    // A console editor needs its console: no CREATE_NO_WINDOW.
    let spec = ChildSpec::new().with_creation_flags(0);
    let mut proc = child::spawn(&mut cmd, &spec).map_err(|e| format!("{prog}: {e}"))?;
    let status = proc.child_mut().wait().map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{prog} exited {}", status.code().unwrap_or(-1)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("marlowe-tui-local-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn escape_sequences_are_removed_from_a_status_line() {
        assert_eq!(strip_ansi("\u{1b}[32mok\u{1b}[0m 3/4 \u{1b}]8;;http://x\u{7}link\u{1b}]8;;\u{7}"), "ok 3/4 link");
        assert_eq!(strip_ansi("plain"), "plain");
    }

    #[test]
    fn the_status_line_command_gets_json_on_stdin_and_its_first_line_comes_back() {
        let cmd = if cfg!(windows) { "findstr /R ." } else { "cat" };
        let r = run_statusline(cmd, "{\"model\":\"m\"}\n", None, Duration::from_secs(10)).unwrap();
        assert_eq!(r, "{\"model\":\"m\"}");
        let echo = run_statusline("echo hello status", "{}", None, Duration::from_secs(10)).unwrap();
        assert_eq!(echo, "hello status");
    }

    #[test]
    fn a_failing_or_slow_status_line_command_is_an_error_not_a_hang() {
        let fail = if cfg!(windows) { "exit /b 3" } else { "exit 3" };
        assert_eq!(run_statusline(fail, "{}", None, Duration::from_secs(10)), Err("exited 3".into()));
        let slow = if cfg!(windows) { "ping -n 20 127.0.0.1 >nul" } else { "sleep 20" };
        let t = Instant::now();
        assert_eq!(run_statusline(slow, "{}", None, Duration::from_millis(400)), Err("timed out".into()));
        assert!(t.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn the_file_index_follows_gitignore_and_skips_dot_git() {
        let d = tmp("index");
        std::fs::create_dir_all(d.join("src")).unwrap();
        std::fs::create_dir_all(d.join(".git")).unwrap();
        std::fs::create_dir_all(d.join("target")).unwrap();
        std::fs::write(d.join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(d.join("README.md"), "x").unwrap();
        std::fs::write(d.join(".git/config"), "x").unwrap();
        std::fs::write(d.join("target/out.o"), "x").unwrap();
        std::fs::write(d.join(".gitignore"), "target/\n").unwrap();
        let files = index_files(&d, 100, Duration::from_secs(5));
        assert_eq!(files, [".gitignore", "README.md", "src/main.rs"]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_image_file_is_read_when_it_is_what_its_name_says() {
        let d = tmp("img");
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend_from_slice(&[0; 32]);
        std::fs::write(d.join("a.png"), &png).unwrap();
        std::fs::write(d.join("b.png"), b"plain text").unwrap();
        std::fs::write(d.join("c.txt"), b"x").unwrap();
        assert_eq!(read_image(&d.join("a.png")).unwrap().0, "image/png");
        assert!(read_image(&d.join("b.png")).is_err());
        assert!(read_image(&d.join("c.txt")).is_err());
        assert!(read_image(&d.join("missing.png")).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn instruction_files_are_found_from_the_repo_root_down() {
        let d = tmp("instr");
        std::fs::create_dir_all(d.join(".git")).unwrap();
        std::fs::create_dir_all(d.join("sub/dir")).unwrap();
        std::fs::write(d.join("CLAUDE.md"), "root").unwrap();
        std::fs::write(d.join("sub/AGENTS.md"), "sub").unwrap();
        std::fs::write(d.join("sub/dir/MARLOWE.local.md"), "me").unwrap();
        let files = find_instructions(Some(&d.join("sub/dir")));
        let project: Vec<(String, bool)> = files
            .iter()
            .filter(|f| f.scope != "user")
            .map(|f| (f.path.rsplit("/").take(2).collect::<Vec<_>>().join("<"), f.exists))
            .collect();
        assert_eq!(
            project,
            [
                ("CLAUDE.md<".to_string() + d.file_name().unwrap().to_str().unwrap(), true),
                ("AGENTS.md<sub".to_string(), true),
                ("MARLOWE.md<dir".to_string(), false),
                ("MARLOWE.local.md<dir".to_string(), true),
            ]
        );
        assert!(files.iter().any(|f| f.scope == "user" && f.path.ends_with("MARLOWE.md")));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_editor_comes_from_the_environment_and_splits_like_a_shell() {
        let env = |v: &'static str, e: &'static str| move |k: &str| match k {
            "VISUAL" => (!v.is_empty()).then(|| v.to_string()),
            "EDITOR" => (!e.is_empty()).then(|| e.to_string()),
            _ => None,
        };
        assert_eq!(editor_command(env("code --wait", "vim")), "code --wait");
        assert_eq!(editor_command(env("", "vim")), "vim");
        assert!(!editor_command(env("", "")).is_empty());
        assert_eq!(split_command("\"C:\\Program Files\\Ed\\ed.exe\" -w --x"), ["C:\\Program Files\\Ed\\ed.exe", "-w", "--x"]);
        assert_eq!(split_command("code  --wait"), ["code", "--wait"]);
    }

    #[cfg(windows)]
    #[test]
    fn the_clipboard_script_parses() {
        let mut cmd = Command::new("powershell.exe");
        cmd.args(["-NoProfile", "-NonInteractive", "-Command", "$null=[scriptblock]::Create($env:MARLOWE_SCRIPT); exit 0"]);
        let spec = ChildSpec::new().with_env([("MARLOWE_SCRIPT".to_string(), CLIPBOARD_SCRIPT.to_string())]);
        let limits = child::RunLimits { timeout: Duration::from_secs(30), max_output_bytes: 4096 };
        let r = child::run(cmd, &spec, limits, &|| false).unwrap();
        assert_eq!(r.code(), 0, "{}", String::from_utf8_lossy(&r.stderr));
    }
}
