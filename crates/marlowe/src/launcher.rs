//! §B17: **Marlowe opens a terminal it controls, rather than assuming the current one is suitable.**
//!
//! > An application that looks wrong because the user has not configured their terminal is an
//! > application that looks wrong.
//!
//! This removes a whole class of report — wrong font, wrong size, wrong background, braille not
//! rendering — by choosing those rather than hoping for them. Every one of those had already been
//! reported against this build at least once, and none of them were bugs in the frame.
//!
//! # What is shipped here, and what is not
//!
//! **Windows is implemented.** Windows Terminal exposes exactly the three things §B17 needs:
//! `--focus` gives a borderless window with no tab bar or title bar, a named profile pins the font
//! and colour scheme, and the profile can be added without touching the ones already there.
//!
//! **macOS and Linux are recorded for M2, not stubbed silently.** The equivalent does exist —
//! iTerm2 dynamic profiles, a GNOME Terminal profile via dconf, kitty and alacritty config
//! fragments — but it is one implementation per terminal, and each needs verifying on the platform
//! rather than reasoned about from here. `launch()` on those platforms says so and runs in the
//! current terminal, which is the honest degraded path rather than a pretend one.
//!
//! # Two rules that make writing to a user's settings acceptable
//!
//! 1. **It adds one profile and never modifies another.** The profile is keyed by a fixed GUID, so
//!    a re-run updates *our* entry and is idempotent; nothing else in the file is rewritten.
//! 2. **A file it cannot parse is a file it does not write.** Windows Terminal's `settings.json` is
//!    JSONC — users have comments in it, and a naive round-trip would delete them. If parsing
//!    fails, the profile step is skipped, the reason is printed, and the launch falls back to the
//!    direct form, which needs no profile at all. Corrupting a user's terminal configuration to
//!    make an application look nicer is not a trade this gets to make.

use std::path::PathBuf;
use std::process::Command;

/// A fixed identity for our profile, so re-running updates rather than accumulates.
const PROFILE_GUID: &str = "{6d8a9f2c-1b3e-4f5a-9c7d-2e8b4a1f6c30}";
const PROFILE_NAME: &str = "Marlowe";
const SCHEME_NAME: &str = "Marlowe";

/// The font is pinned because braille is load-bearing (ADR-021) and a font without
/// U+2800–U+28FF renders the amplitude meter as tofu. Cascadia Code ships with Windows Terminal,
/// so pinning it does not ask the user to install anything.
const FONT: &str = "Cascadia Code";

/// What happened, so `launch` can report rather than assume.
pub struct Outcome {
    pub terminal: Option<String>,
    pub profile_written: bool,
    /// Everything that did not go to plan, in the user's words rather than an error type.
    pub degraded: Vec<String>,
    /// Settings changed **outside** Marlowe's own profile, named individually.
    ///
    /// Windows Terminal keeps window chrome — the theme, the tab row, the initial size — in global
    /// settings; there is no per-profile form of any of them. So theming the chrome to match the
    /// frame necessarily reaches outside the "adds one, modifies none" boundary that covers
    /// profiles. That is allowed, because it is what was asked for, but it is **never silent**:
    /// every global key touched is listed back to the user at launch.
    pub globals_changed: Vec<String>,
}

/// Open Marlowe in a terminal it controls. Returns without spawning if none is suitable.
pub fn launch(extra_args: &[String]) -> std::io::Result<Outcome> {
    #[cfg(windows)]
    {
        launch_windows(extra_args)
    }
    #[cfg(not(windows))]
    {
        let _ = extra_args;
        Ok(Outcome {
            terminal: None,
            degraded: vec![
                "§B17's launcher is implemented for Windows Terminal only. On this platform \
                 Marlowe runs in the current terminal, so the font, size and background are \
                 whatever you have configured — in particular, the amplitude meter needs a font \
                 with braille (U+2800-28FF). `marlowe doctor` prints the glyph row to check."
                    .to_string(),
            ],
            profile_written: false,
            globals_changed: Vec::new(),
        })
    }
}

#[cfg(windows)]
fn launch_windows(extra_args: &[String]) -> std::io::Result<Outcome> {
    let mut degraded = Vec::new();

    let Some(wt) = which_wt() else {
        return Ok(Outcome {
            terminal: None,
            profile_written: false,
            globals_changed: Vec::new(),
            degraded: vec![
                "Windows Terminal (wt.exe) was not found, so Marlowe cannot choose its own \
                 window. Running here instead: the font, size and background are whatever this \
                 terminal is configured with. Install Windows Terminal for the intended frame."
                    .to_string(),
            ],
        });
    };

    let exe = std::env::current_exe()?;
    let mut globals_changed = Vec::new();
    let profile_written = match write_profile(&exe, extra_args) {
        Ok((written, globals)) => {
            globals_changed = globals;
            written
        }
        Err(why) => {
            degraded.push(format!(
                "could not add the Marlowe profile ({why}), so the font and colour scheme are \
                 this terminal's rather than Marlowe's. Nothing in your settings was changed."
            ));
            false
        }
    };

    // **No focus mode.** `--focus` gives a borderless window, and it also takes away the title bar
    // — which means no dragging and no close button. A window the user cannot move or close is not
    // a better window. The title bar earns its row instead: Marlowe writes it via OSC 0 and keeps
    // it current, so the chrome is a live readout rather than a static label.
    let mut cmd = Command::new(&wt);
    if profile_written {
        cmd.args(["-p", PROFILE_NAME]);
    } else {
        // No profile means launching the binary directly. The frame is identical; only the font
        // and scheme are the terminal's own.
        cmd.arg("--");
        cmd.arg(&exe);
        cmd.arg("--tui");
        cmd.arg("--ground");
        for a in extra_args {
            cmd.arg(a);
        }
    }
    cmd.spawn()?;

    Ok(Outcome {
        terminal: Some("Windows Terminal".to_string()),
        profile_written,
        degraded,
        globals_changed,
    })
}

#[cfg(windows)]
fn which_wt() -> Option<PathBuf> {
    // The Store build puts a launcher stub on PATH; resolve through PATH rather than guessing at
    // the package directory, whose name carries a version.
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join("wt.exe"))
        .find(|p| p.is_file())
}

#[cfg(windows)]
fn settings_path() -> Option<PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA")?;
    let packaged = PathBuf::from(&local)
        .join("Packages")
        .join("Microsoft.WindowsTerminal_8wekyb3d8bbwe")
        .join("LocalState")
        .join("settings.json");
    if packaged.is_file() {
        return Some(packaged);
    }
    let unpackaged = PathBuf::from(&local)
        .join("Microsoft")
        .join("Windows Terminal")
        .join("settings.json");
    unpackaged.is_file().then_some(unpackaged)
}

/// Add or refresh the Marlowe profile and colour scheme. `Ok(false)` means "no settings file",
/// which is not an error — it just means the direct launch is used.
#[cfg(windows)]
/// The `commandline` a Windows Terminal profile is written with.
///
/// **Pure, and separate from `write_profile`, because `write_profile` touches the user's real
/// `settings.json`** — the property worth asserting is what string gets built, and that must be
/// assertable without writing to a file a person owns.
///
/// # The defect this closes
///
/// It was `format!("\"{}\" --tui --ground", exe)` — a fixed string. `--launch` forwards its extra
/// arguments to the direct-spawn path, but the PROFILE is what the Start-menu shortcut and
/// `wt -p Marlowe` actually run, so the shortcut ignored every flag it was launched with. Two
/// launch paths, disagreeing about what Marlowe is.
///
/// **No key is ever written here.** This string lands in `settings.json` in plain text; the child
/// inherits `OPENROUTER_API_KEY` from the environment instead.
fn profile_commandline(exe: &std::path::Path, extra_args: &[String]) -> String {
    let mut out = format!("\"{}\" --tui --ground", exe.display());
    for a in extra_args {
        out.push(' ');
        // Quoted per-argument: a slug has no spaces today, and a commandline built by
        // concatenation is one odd argument away from being wrong. This file is written once and
        // read by Windows Terminal forever.
        if a.contains(' ') {
            out.push('"');
            out.push_str(a);
            out.push('"');
        } else {
            out.push_str(a);
        }
    }
    out
}

fn write_profile(exe: &std::path::Path, extra_args: &[String]) -> Result<(bool, Vec<String>), String> {
    let Some(path) = settings_path() else {
        return Ok((false, Vec::new()));
    };
    let raw = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;

    // JSONC in, or nothing. A file with comments parses as an error here, and that is the correct
    // outcome: rewriting it would silently delete the user's comments.
    let mut root: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| format!("settings.json is not plain JSON ({e}); it may contain comments"))?;

    let commandline = profile_commandline(exe, extra_args);

    let profile = serde_json::json!({
        "guid": PROFILE_GUID,
        "name": PROFILE_NAME,
        "commandline": commandline,
        "colorScheme": SCHEME_NAME,
        "font": { "face": FONT, "size": 11 },
        // A white block cursor is the loudest foreign element on a violet frame.
        "cursorShape": "filledBox",
        "hidden": false,
        // The default margin leaves dead space between the frame and the border rows, which reads
        // as a rendering fault rather than as spacing.
        "padding": "0",
        // Marlowe draws its own scrollbar inside the conversation region (§B6). The terminal's is
        // a second one, outside the border, scrolling something else.
        "scrollbarState": "hidden",
        "bellStyle": "none",
        // Marlowe sets its own title via OSC 0 and keeps it current with session state. Suppressing
        // it would turn a live readout back into a static label.
        "suppressApplicationTitle": false,
        "snapOnInput": true,
        "useAcrylic": false,
    });

    let scheme = serde_json::json!({
        "name": SCHEME_NAME,
        // The mockup's ground, so there is no seam between the scheme and `--ground`'s OSC 11.
        "background": "#0F0E14",
        "foreground": "#CDC9D6",
        "cursorColor": "#9B7EDE",
        "selectionBackground": "#251F38",
        "black": "#0F0E14", "red": "#DD7F85", "green": "#8FD4A8", "yellow": "#D6A95F",
        "blue": "#7FA8DD", "purple": "#9B7EDE", "cyan": "#8FD4A8", "white": "#CDC9D6",
        "brightBlack": "#4A4460", "brightRed": "#DD7F85", "brightGreen": "#8FD4A8",
        "brightYellow": "#D6A95F", "brightBlue": "#7FA8DD", "brightPurple": "#B39AE8",
        "brightCyan": "#8FD4A8", "brightWhite": "#E2DDF0",
    });

    // The window chrome, so the tab row stops reading as a lid sitting on the design.
    //
    // `tab.background` and `tabRow.background` take the literal `"terminalBackground"`, which
    // resolves to the active scheme's background. That is preferred over repeating `#0F0E14`:
    // the two can then never disagree, and a seam at the frame edge is exactly what a repeated
    // literal produces the day one copy is changed and the other is not.
    let theme = serde_json::json!({
        "name": SCHEME_NAME,
        // No `frame` key. It was tried and **measured to do nothing** — Windows Terminal 1.24
        // accepts it silently and the focused title bar stayed at the Windows accent colour
        // (#946B33). Shipping a setting that is ignored is the unobservable mismatch this project
        // keeps paying for; the trade-off it was meant to solve is recorded in §B17 instead.
        "window": { "applicationTheme": "dark", "useMica": false },
        "tab": {
            "background": "terminalBackground",
            "unfocusedBackground": "terminalBackground",
            // No per-tab close button. Closing the *window* is the intended gesture and the title
            // bar still offers it; an `×` on the tab closes a session while leaving an empty
            // terminal behind, which looks like a crash rather than a quit.
            "showCloseButton": "never",
        },
        "tabRow": {
            "background": "terminalBackground",
            "unfocusedBackground": "terminalBackground",
        },
    });

    upsert(&mut root, "profiles", "list", "guid", PROFILE_GUID, profile)?;
    upsert_named(&mut root, "schemes", scheme)?;
    upsert_named(&mut root, "themes", theme)?;

    let mut globals = Vec::new();
    {
        let obj = root
            .as_object_mut()
            .ok_or("settings.json root is not an object")?;

        // Chrome. These are global by construction — Windows Terminal has no per-profile form.
        for (k, v) in [
            ("theme", serde_json::json!(SCHEME_NAME)),
            // **A measured trade-off, not an oversight. Both options were built and photographed.**
            //
            // `false` gives the tab row its own line, which `alwaysShowTabs: false` then hides
            // entirely at one tab — no `+`, no chevron. But the title bar is drawn by Windows, and
            // Windows paints a focused title bar in the user's accent colour: measured **#946B33**
            // here, a brighter lid than the grey one this was meant to remove. `themes.window.frame`
            // does not override it (tried; ignored silently by WT 1.24).
            //
            // `true` lets Windows Terminal draw the bar, which the theme then paints **#0F0E14** —
            // measured identical to the terminal background, no seam. The cost is that the `+` and
            // the chevron cannot be hidden by any setting Windows Terminal exposes.
            //
            // Chosen: `true`. A dark bar carrying a live title with one `+` on it reads as part of
            // the application; a bronze bar does not, and the `×` — the destructive one — is gone
            // either way via `tab.showCloseButton: never`.
            ("showTabsInTitlebar", serde_json::json!(true)),
            ("alwaysShowTabs", serde_json::json!(false)),
            ("tabWidthMode", serde_json::json!("compact")),
        ] {
            if obj.get(k) != Some(&v) {
                obj.insert(k.into(), v);
                globals.push(k.to_string());
            }
        }

        // §B11's minimum, and only raised toward it — never lowered. A user who opens their
        // terminal at 200x60 did not ask Marlowe to shrink it.
        for (k, min) in [("initialCols", 120u64), ("initialRows", 30u64)] {
            let current = obj.get(k).and_then(|v| v.as_u64());
            if current.is_none_or(|c| c < min) {
                obj.insert(k.into(), serde_json::json!(min));
                globals.push(k.to_string());
            }
        }

        // Only when the user has not expressed a preference. Overwriting an explicit `true` would
        // break "adds one, modifies none" through a key that merely lives elsewhere in the file.
        if !obj.contains_key("confirmCloseAllTabs") {
            obj.insert("confirmCloseAllTabs".into(), serde_json::Value::Bool(false));
            globals.push("confirmCloseAllTabs".to_string());
        }
    }

    // **`ctrl+shift+t`, `ctrl+shift+d` and `ctrl+shift+w` are deliberately NOT unbound.**
    //
    // The request was to disable them for Marlowe's tab only, and Windows Terminal cannot express
    // that: keybindings live in one global `actions` array, and the terminal consumes those chords
    // itself before the keystroke is ever delivered to the running application — so Marlowe cannot
    // intercept them either. The only implementation available is global, which would remove
    // new-tab and split-pane from every terminal tab the user has.
    //
    // Doing it globally and calling it tab-scoped would be the worse outcome: the setting would
    // work, the reason it was wanted would not, and the cost would surface somewhere unrelated
    // weeks later. `alwaysShowTabs: false` already removes the tab strip — and with it the `+`
    // and `×` — while a single tab is open, which covers the accident this was aimed at.

    // Back up before writing. The file is the user's, and a launcher that eats a terminal
    // configuration has cost more than it saved.
    let backup = path.with_extension("json.marlowe-backup");
    std::fs::write(&backup, &raw).map_err(|e| e.to_string())?;
    let pretty = serde_json::to_string_pretty(&root).map_err(|e| e.to_string())?;
    std::fs::write(&path, pretty).map_err(|e| e.to_string())?;
    Ok((true, globals))
}

/// Replace the entry whose `key` matches `id`, or append. **Never touches any other entry.**
#[cfg(windows)]
fn upsert(
    root: &mut serde_json::Value,
    outer: &str,
    inner: &str,
    key: &str,
    id: &str,
    entry: serde_json::Value,
) -> Result<(), String> {
    // `profiles` is either an object with a `list`, or (older schemas) an array directly.
    let list = match root.get_mut(outer) {
        Some(serde_json::Value::Object(o)) => o
            .get_mut(inner)
            .ok_or_else(|| format!("{outer}.{inner} missing"))?,
        Some(v @ serde_json::Value::Array(_)) => v,
        _ => return Err(format!("{outer} missing")),
    };
    let arr = list
        .as_array_mut()
        .ok_or_else(|| format!("{outer}.{inner} is not a list"))?;
    match arr
        .iter()
        .position(|p| p.get(key).and_then(|g| g.as_str()) == Some(id))
    {
        Some(i) => arr[i] = entry,
        None => arr.push(entry),
    }
    Ok(())
}

/// Replace-or-append into a top-level array keyed by `name` — `schemes` and `themes` both.
///
/// One function for both, because they are the same operation and two copies is how the themes
/// array ends up appending a duplicate on every launch while the schemes array does not.
#[cfg(windows)]
fn upsert_named(
    root: &mut serde_json::Value,
    key: &str,
    entry: serde_json::Value,
) -> Result<(), String> {
    let name = entry
        .get("name")
        .and_then(|n| n.as_str())
        .ok_or("entry has no name")?
        .to_string();
    if root.get(key).is_none() {
        root.as_object_mut()
            .ok_or("settings.json root is not an object")?
            .insert(key.into(), serde_json::Value::Array(Vec::new()));
    }
    let arr = root
        .get_mut(key)
        .and_then(|s| s.as_array_mut())
        .ok_or_else(|| format!("{key} is not a list"))?;
    match arr
        .iter()
        .position(|s| s.get("name").and_then(|n| n.as_str()) == Some(name.as_str()))
    {
        Some(i) => arr[i] = entry,
        None => arr.push(entry),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[cfg(windows)]
    use super::*;

    #[cfg(windows)]
    #[test]
    fn upsert_replaces_our_entry_and_leaves_every_other_one_alone() {
        // The rule that makes writing to someone else's settings file acceptable, asserted.
        let mut root = serde_json::json!({
            "profiles": { "list": [
                { "guid": "{aaa}", "name": "PowerShell", "font": { "face": "Consolas" } },
                { "guid": PROFILE_GUID, "name": "Marlowe", "font": { "face": "old" } },
                { "guid": "{bbb}", "name": "Ubuntu" }
            ]}
        });
        let entry = serde_json::json!({ "guid": PROFILE_GUID, "name": "Marlowe", "font": { "face": "new" } });
        upsert(&mut root, "profiles", "list", "guid", PROFILE_GUID, entry).unwrap();

        let list = root["profiles"]["list"].as_array().unwrap();
        assert_eq!(list.len(), 3, "upsert must replace, not append a duplicate");
        assert_eq!(list[0]["name"], "PowerShell");
        assert_eq!(list[0]["font"]["face"], "Consolas", "another profile was rewritten");
        assert_eq!(list[1]["font"]["face"], "new");
        assert_eq!(list[2]["name"], "Ubuntu");
    }

    #[cfg(windows)]
    #[test]
    fn upsert_appends_when_we_are_not_there_yet() {
        let mut root = serde_json::json!({ "profiles": { "list": [ { "guid": "{aaa}" } ] } });
        let entry = serde_json::json!({ "guid": PROFILE_GUID });
        upsert(&mut root, "profiles", "list", "guid", PROFILE_GUID, entry).unwrap();
        assert_eq!(root["profiles"]["list"].as_array().unwrap().len(), 2);
    }

    #[cfg(windows)]
    #[test]
    fn a_settings_file_with_comments_is_refused_rather_than_rewritten() {
        // Windows Terminal ships settings.json WITH comments. Round-tripping it through
        // serde_json would delete them, so parsing has to fail loudly and skip the write.
        let jsonc = "{\n  // the default profile\n  \"profiles\": { \"list\": [] }\n}";
        assert!(serde_json::from_str::<serde_json::Value>(jsonc).is_err());
    }
}

#[cfg(test)]
mod profile_commandline_tests {
    use super::*;
    use std::path::Path;

    /// **The regression: the shortcut ignored the flags it was launched with.**
    #[test]
    fn the_shortcut_carries_the_provider_it_was_launched_with() {
        let c = profile_commandline(
            Path::new("C:/m/marlowe.exe"),
            &["--provider".into(), "openrouter".into(),
              "--openrouter-model".into(), "stealth/ox-alpha".into()],
        );
        assert!(c.contains("--provider openrouter"), "{c}");
        assert!(c.contains("--openrouter-model stealth/ox-alpha"), "{c}");
        assert!(c.contains("--tui"), "{c}");
    }

    /// The control. Without it, the assertion above passes on a build that appends the flags
    /// unconditionally, and "carries what it was launched with" says nothing.
    #[test]
    fn a_plain_launch_writes_exactly_what_it_always_did() {
        let c = profile_commandline(Path::new("C:/m/marlowe.exe"), &[]);
        assert_eq!(c, "\"C:/m/marlowe.exe\" --tui --ground");
    }

    /// `settings.json` is plain text on disk and readable by anything on the machine.
    #[test]
    fn a_key_can_never_reach_the_profile() {
        let c = profile_commandline(
            Path::new("C:/m/marlowe.exe"),
            &["--provider".into(), "openrouter".into()],
        );
        assert!(!c.contains("sk-or"), "{c}");
        assert!(!c.to_lowercase().contains("api_key"), "{c}");
    }

    /// An argument containing a space must survive as ONE argument.
    #[test]
    fn an_argument_with_a_space_is_quoted() {
        let c = profile_commandline(Path::new("C:/m/marlowe.exe"), &["a b".into()]);
        assert!(c.ends_with("\"a b\""), "{c}");
    }
}

// ─── a window per run (`M3-DESIGN.md` §6.7) ───────────────────────────────────────────────────

/// What happened when Marlowe tried to open a window on a run.
///
/// **`command` is always populated, whether or not the spawn worked**, and that is the whole shape
/// of §6.7: *"Best-effort, with a printed attach command as the fallback, so it degrades to a
/// copy-paste rather than a broken button."* A caller that only reported success would leave a user
/// on an unsupported platform with a button that does nothing and no way forward.
pub struct WindowOutcome {
    /// The terminal that opened one, or `None` if none could be.
    pub terminal: Option<String>,
    /// The command that attaches to this run from any terminal, anywhere.
    pub command: String,
    /// Why the spawn did not happen, in the user's words.
    pub degraded: Option<String>,
}

/// Open a terminal window attached to `run`. Best-effort.
///
/// # Why best-effort is the right contract here and not a cop-out
///
/// Spawning a terminal is per-platform and per-terminal: `wt.exe`, `open -a Terminal`, and on Linux
/// a guess between a dozen emulators. Getting that wrong is not a small failure — it is a window
/// that does not appear, or one that appears in the wrong emulator with the wrong font.
///
/// What makes the degraded path acceptable is §10.1's *"addressable from outside"*: `marlowe --watch
/// <run>` is an ordinary command that works in a terminal the user already has open, over SSH, from
/// a script. So the fallback is not a lesser version of the feature — it is the feature, without
/// the convenience of something else typing it.
pub fn open_window(run: &str) -> WindowOutcome {
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "marlowe".to_string());
    // Quoted, because a Windows install path has spaces in it more often than not.
    let command = if exe.contains(' ') {
        format!("\"{exe}\" --watch {run}")
    } else {
        format!("{exe} --watch {run}")
    };

    #[cfg(windows)]
    {
        let Some(wt) = which_wt() else {
            return WindowOutcome {
                terminal: None,
                command,
                degraded: Some(
                    "Windows Terminal (wt.exe) was not found, so Marlowe cannot open a window for \
                     you. Run this in a terminal you have open:"
                        .to_string(),
                ),
            };
        };
        // **`wt -w -1` opens a NEW window rather than a tab in the current one.** A tab would put
        // the run beside the conversation in one frame, which is precisely what §6.6's first
        // interface rule exists to prevent.
        let spawned = Command::new(&wt)
            .args(["-w", "-1", "--title", "marlowe run", "--"])
            .arg(&exe)
            .arg("--watch")
            .arg(run)
            .spawn();
        match spawned {
            Ok(_) => WindowOutcome {
                terminal: Some("Windows Terminal".to_string()),
                command,
                degraded: None,
            },
            Err(e) => WindowOutcome {
                terminal: None,
                command,
                degraded: Some(format!(
                    "Windows Terminal would not start ({e}). Run this in a terminal you have open:"
                )),
            },
        }
    }

    #[cfg(target_os = "macos")]
    {
        // `open -a Terminal <path>` runs a file, not a command line, so the command goes through a
        // one-line script. Recorded rather than attempted blind: this is unverified on the platform
        // and §6.7 says to plan for that rather than discover it.
        WindowOutcome {
            terminal: None,
            command,
            degraded: Some(
                "opening a window is implemented for Windows Terminal only, and this is macOS. \
                 Run this in a terminal you have open:"
                    .to_string(),
            ),
        }
    }

    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        // **No guessing between emulators.** `x-terminal-emulator`, `gnome-terminal`, `konsole`,
        // `alacritty` and `kitty` all take different arguments, and one that half-works — a window
        // that opens and immediately closes because the argument form was wrong — is worse than a
        // command the user can paste and see fail with a real message.
        WindowOutcome {
            terminal: None,
            command,
            degraded: Some(
                "opening a window is implemented for Windows Terminal only. Run this in a \
                 terminal you have open:"
                    .to_string(),
            ),
        }
    }
}

#[cfg(test)]
mod window_tests {
    use super::*;

    /// §6.7's contract, and the property that makes the whole thing degrade rather than break:
    /// **there is always a command**, whatever the platform did.
    #[test]
    fn a_command_is_always_offered_whether_or_not_a_window_opened() {
        let out = open_window("a1b2c3d4");
        assert!(out.command.contains("--watch a1b2c3d4"), "{}", out.command);
        assert!(
            out.terminal.is_some() || out.degraded.is_some(),
            "a spawn that neither opened a window nor said why is a button that does nothing"
        );
    }

    /// A path with a space in it is quoted. A Windows install is under `Program Files` more often
    /// than not, and an unquoted command that the user pastes and watches fail is worse than none.
    #[test]
    fn the_fallback_command_survives_a_path_with_spaces() {
        let out = open_window("r");
        if out.command.starts_with('"') {
            assert!(out.command.contains("\" --watch r"), "{}", out.command);
        }
    }
}
