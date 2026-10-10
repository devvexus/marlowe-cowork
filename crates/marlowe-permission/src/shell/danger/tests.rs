// Copyright 2025 OpenAI (codex), Apache-2.0. Tests from codex-rs/shell-command/src/
// command_safety/{is_dangerous_command.rs, windows_dangerous_commands.rs} at
// 6c0c6759d0617d7670826b61aca0a931bbb6db1f, ported onto Marlowe's types unchanged in substance.

use super::*;

fn vec_str(items: &[&str]) -> Vec<String> {
    items.iter().map(std::string::ToString::to_string).collect()
}

// ---- is_dangerous_command.rs

#[test]
fn rm_rf_is_dangerous() {
    assert_eq!(dangerous_command_match(&vec_str(&["rm", "-rf", "/"])), Some(DangerousCommandMatch::ForcedRm));
}

#[test]
fn rm_f_is_dangerous() {
    assert_eq!(dangerous_command_match(&vec_str(&["rm", "-f", "/"])), Some(DangerousCommandMatch::ForcedRm));
}

#[test]
fn forced_rm_variants_are_dangerous() {
    for command in [
        vec_str(&["/bin/rm", "-fr", "/tmp/example"]),
        vec_str(&["rm", "-r", "-f", "/tmp/example"]),
        vec_str(&["rm", "--force", "/tmp/example"]),
        vec_str(&["rm", "/tmp/example", "-f"]),
        vec_str(&["sudo", "rm", "-rf", "/tmp/example"]),
        vec_str(&["env", "TARGET=/tmp/example", "rm", "-rf", "/tmp/example"]),
    ] {
        assert_eq!(dangerous_command_match(&command), Some(DangerousCommandMatch::ForcedRm), "{command:?}");
    }
}

#[test]
fn deeply_nested_command_wrappers_fail_closed() {
    for (depth, expected) in [
        (MAX_DANGEROUS_COMMAND_WRAPPER_DEPTH, DangerousCommandMatch::ForcedRm),
        (MAX_DANGEROUS_COMMAND_WRAPPER_DEPTH + 1, DangerousCommandMatch::Other),
    ] {
        let command = std::iter::repeat_n("env", depth).chain(["rm", "-rf", "/tmp/example"]).map(str::to_owned).collect::<Vec<_>>();
        assert_eq!(dangerous_command_match(&command), Some(expected));
    }
}

#[test]
fn forced_rm_in_complex_shell_syntax_is_dangerous() {
    for script in [
        "printf x | rm -rf /tmp/example",
        "if test -d /tmp/example; then rm --force /tmp/example; fi",
        "rm -rf \"$TARGET\" >/dev/null",
        "for target in /tmp/a /tmp/b; do rm -r -f \"$target\"; done",
        "echo \"$(rm -rf /tmp/example)\"",
        "bash -c 'rm -rf /tmp/example'",
        "trap 'rm -rf /tmp/example' EXIT",
        "for a in '-C5a25KeRr' '--' '--json' '--bogus'; do HOME=$(mktemp -d) MDE_URL=http://127.0.0.1:1 MDE_TOKEN=x node cli/mde.cjs ls \"$a\" >/tmp/mde-review-out 2>/tmp/mde-review-err; code=$?; printf '%s\\t%s\\t%s\\n' \"$a\" \"$code\" \"$(tr '\\n' ' ' </tmp/mde-review-err)\"; rm -rf \"$HOME\"; done",
    ] {
        let command = vec_str(&["bash", "-lc", script]);
        assert_eq!(dangerous_command_match(&command), Some(DangerousCommandMatch::ForcedRm), "{script}");
    }
}

#[test]
fn non_forced_or_non_literal_rm_is_not_dangerous() {
    for command in [
        vec_str(&["rm", "-r", "/tmp/example"]),
        vec_str(&["rm", "--", "-f"]),
        vec_str(&["bash", "-lc", "echo 'rm -rf /tmp/example'"]),
        vec_str(&["bash", "-lc", "cmd=rm; $cmd -rf /tmp/example"]),
        vec_str(&["bash", "-lc", "if then rm -rf /tmp/example"]),
        vec_str(&["env", "TARGET=/tmp/example", "rm", "-r", "/tmp/example"]),
        vec_str(&["bash", "-lc", "trap 'echo rm -rf /tmp/example' EXIT"]),
    ] {
        assert_eq!(dangerous_command_match(&command), None, "{command:?}");
    }
}

#[test]
fn direct_powershell_words_return_other_match_on_windows() {
    let command = vec_str(&["Remove-Item", "test", "-Force"]);
    let want = cfg!(windows).then_some(DangerousCommandMatch::Other);
    assert_eq!(dangerous_powershell_words_match(&command, DangerousCommandPlatform::host()), want);
}

// ---- windows_dangerous_commands.rs

fn win(items: &[&str]) -> bool {
    is_dangerous_command_windows(&vec_str(items))
}

#[test]
fn powershell_start_process_url_is_dangerous() {
    assert!(win(&["powershell", "-NoLogo", "-Command", "Start-Process 'https://example.com'"]));
}

#[test]
fn powershell_start_process_url_with_trailing_semicolon_is_dangerous() {
    assert!(win(&["powershell", "-Command", "Start-Process('https://example.com');"]));
}

#[test]
fn powershell_start_process_mixed_case_urls_are_dangerous() {
    for script in ["Start-Process('HTTP://example.com');", "Start-Process('hTtPs://example.com');"] {
        assert!(win(&["powershell", "-Command", script]), "{script}");
    }
}

#[test]
fn powershell_start_process_local_is_not_flagged() {
    assert!(!win(&["powershell", "-Command", "Start-Process notepad.exe"]));
}

#[test]
fn cmd_start_with_url_is_dangerous() {
    assert!(win(&["cmd", "/c", "start", "https://example.com"]));
}

#[test]
fn msedge_with_url_is_dangerous() {
    assert!(win(&["msedge.exe", "https://example.com"]));
}

#[test]
fn explorer_with_directory_is_not_flagged() {
    assert!(!win(&["explorer.exe", "."]));
}

#[test]
fn powershell_remove_item_force_is_dangerous() {
    assert!(win(&["powershell", "-Command", "Remove-Item test -Force"]));
}

#[test]
fn powershell_remove_item_recurse_force_is_dangerous() {
    assert!(win(&["powershell", "-Command", "Remove-Item test -Recurse -Force"]));
}

#[test]
fn powershell_ri_alias_force_is_dangerous() {
    assert!(win(&["pwsh", "-Command", "ri test -Force"]));
}

#[test]
fn powershell_remove_item_without_force_is_not_flagged() {
    assert!(!win(&["powershell", "-Command", "Remove-Item test"]));
}

#[test]
fn cmd_del_force_is_dangerous() {
    assert!(win(&["cmd", "/c", "del", "/f", "test.txt"]));
}

#[test]
fn cmd_erase_force_is_dangerous() {
    assert!(win(&["cmd", "/c", "erase", "/f", "test.txt"]));
}

#[test]
fn cmd_del_without_force_is_not_flagged() {
    assert!(!win(&["cmd", "/c", "del", "test.txt"]));
}

#[test]
fn cmd_rd_recursive_is_dangerous() {
    assert!(win(&["cmd", "/c", "rd", "/s", "/q", "test"]));
}

#[test]
fn cmd_rd_without_quiet_is_not_flagged() {
    assert!(!win(&["cmd", "/c", "rd", "/s", "test"]));
}

#[test]
fn cmd_rmdir_recursive_is_dangerous() {
    assert!(win(&["cmd", "/c", "rmdir", "/s", "/q", "test"]));
}

#[test]
fn powershell_remove_item_path_recurse_force_is_dangerous() {
    assert!(win(&["powershell", "-Command", "Remove-Item -Path 'test' -Recurse -Force"]));
}

#[test]
fn powershell_remove_item_force_with_semicolon_is_dangerous() {
    assert!(win(&["powershell", "-Command", "Remove-Item test -Force; Write-Host done"]));
}

#[test]
fn powershell_remove_item_force_inside_block_is_dangerous() {
    assert!(win(&["powershell", "-Command", "if ($true) { Remove-Item test -Force}"]));
}

#[test]
fn powershell_remove_item_force_inside_brackets_is_dangerous() {
    assert!(win(&["powershell", "-Command", "[void]( Remove-Item test -Force)]"]));
}

#[test]
fn cmd_del_path_containing_f_is_not_flagged() {
    assert!(!win(&["cmd", "/c", "del", "C:/foo/bar.txt"]));
}

#[test]
fn cmd_rd_path_containing_s_is_not_flagged() {
    assert!(!win(&["cmd", "/c", "rd", "C:/source"]));
}

#[test]
fn cmd_bypass_chained_del_is_dangerous() {
    assert!(win(&["cmd", "/c", "echo", "hello", "&", "del", "/f", "file.txt"]));
}

#[test]
fn powershell_chained_no_space_is_dangerous() {
    assert!(win(&["powershell", "-Command", "Write-Host hi;Remove-Item -Force C:\\tmp"]));
}

#[test]
fn powershell_comma_separated_is_dangerous() {
    assert!(win(&["powershell", "-Command", "del,-Force,C:\\foo"]));
}

#[test]
fn cmd_echo_del_is_not_dangerous() {
    assert!(!win(&["cmd", "/c", "echo", "del", "/f"]));
}

#[test]
fn cmd_del_single_string_argument_is_dangerous() {
    assert!(win(&["cmd", "/c", "del /f file.txt"]));
}

#[test]
fn cmd_del_chained_single_string_argument_is_dangerous() {
    assert!(win(&["cmd", "/c", "echo hello & del /f file.txt"]));
}

#[test]
fn cmd_chained_no_space_del_is_dangerous() {
    assert!(win(&["cmd", "/c", "echo hi&del /f file.txt"]));
}

#[test]
fn cmd_chained_andand_no_space_del_is_dangerous() {
    assert!(win(&["cmd", "/c", "echo hi&&del /f file.txt"]));
}

#[test]
fn cmd_chained_oror_no_space_del_is_dangerous() {
    assert!(win(&["cmd", "/c", "echo hi||del /f file.txt"]));
}

#[test]
fn cmd_start_url_single_string_is_dangerous() {
    assert!(win(&["cmd", "/c", "start https://example.com"]));
}

#[test]
fn cmd_chained_no_space_rmdir_is_dangerous() {
    assert!(win(&["cmd", "/c", "echo hi&rmdir /s /q testdir"]));
}

#[test]
fn cmd_del_force_uppercase_flag_is_dangerous() {
    assert!(win(&["cmd", "/c", "DEL", "/F", "file.txt"]));
}

#[test]
fn cmdexe_r_del_force_is_dangerous() {
    assert!(win(&["cmd.exe", "/r", "del", "/f", "file.txt"]));
}

#[test]
fn cmd_start_quoted_url_single_string_is_dangerous() {
    assert!(win(&["cmd", "/c", r#"start "https://example.com""#]));
}

#[test]
fn cmd_start_title_then_url_is_dangerous() {
    assert!(win(&["cmd", "/c", r#"start "" https://example.com"#]));
}

#[test]
fn powershell_rm_alias_force_is_dangerous() {
    assert!(win(&["powershell", "-Command", "rm test -Force"]));
}

/// PowerShell takes any unambiguous prefix of a parameter (`-Fo` is `-Force`; `-F` alone is
/// ambiguous with `-Filter`). Not in Codex's suite.
#[test]
fn powershell_abbreviated_force_is_dangerous() {
    for line in ["Remove-Item test -Fo", "del test -forc -Recurse", "ri test -Force:$true", "Remove-Item -FOR test"] {
        assert!(win(&["powershell", "-Command", line]), "{line}");
    }
    assert!(!win(&["powershell", "-Command", "Remove-Item test -Filter *.tmp"]));
}

#[test]
fn powershell_benign_force_separate_command_is_not_dangerous() {
    assert!(!win(&["powershell", "-Command", "Get-ChildItem -Force; Remove-Item test"]));
}

#[test]
fn shlex_matches_posix_splitting() {
    assert_eq!(shlex_split("a 'b c' \"d\\\"e\" f\\ g").unwrap(), vec_str(&["a", "b c", "d\"e", "f g"]));
    assert_eq!(shlex_split("'open"), None);
    assert_eq!(shlex_split("").unwrap(), Vec::<String>::new());
}
