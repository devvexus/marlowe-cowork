//! Non-negotiable 5 for the git tools: API keys never reach a child, `gh` gets the GitHub token
//! only because it is allowed through, and a crew host's no-push environment applies to `git`.
//! Own test binary: it sets process environment variables.

mod gitfix;

use gitfix::*;
use serde_json::json;

#[test]
fn model_keys_are_scrubbed_from_git_and_gh_and_gh_keeps_its_token() {
    for (k, v) in [
        ("OPENROUTER_API_KEY", "canary-openrouter"),
        ("ANTHROPIC_API_KEY", "canary-anthropic"),
        ("HF_TOKEN", "canary-hf"),
        ("GH_TOKEN", "canary-gh"),
    ] {
        std::env::set_var(k, v);
    }
    let fx = Fx::new("env");
    let t = fx.tools();
    fx.respond("pr view", json!({"number": 1, "title": "t", "state": "OPEN", "author": {"login": "a"}}), 0);
    let o = call(&t, "gh", json!({"op": "pr_view", "number": 1}));
    assert!(!o.is_error, "{}", o.content);
    let env = &fx.gh_calls()[0]["env"];
    assert_eq!(env["GH_TOKEN"], true, "the GitHub token is allowed through to gh: {env}");
    for k in ["OPENROUTER_API_KEY", "ANTHROPIC_API_KEY", "HF_TOKEN"] {
        assert_eq!(env[k], false, "{k} must not reach gh");
    }

    // A hook runs as git's child and dumps what it was given.
    let dump = fx.base.join("hook-env.txt");
    let hook = fx.work.join(".git/hooks/pre-commit");
    std::fs::write(&hook, format!("#!/bin/sh\nenv > '{}'\nexit 0\n", dump.to_string_lossy().replace('\\', "/"))).unwrap();
    fx.put("a.txt", "a\n");
    assert!(!call(&t, "git", json!({"op": "add", "paths": ["a.txt"]})).is_error);
    let o = call(&t, "git", json!({"op": "commit", "message": "a"}));
    assert!(!o.is_error, "{}", o.content);
    let seen = std::fs::read_to_string(&dump).expect("the hook ran");
    assert!(!seen.contains("canary-openrouter") && !seen.contains("canary-anthropic") && !seen.contains("canary-hf"), "keys leaked into a git child:\n{seen}");
    assert!(seen.contains("GIT_TERMINAL_PROMPT=0"), "git never prompts: {seen}");

    // A push is git talking to a remote: the user's GitHub token stays for a credential helper
    // (a pre-push hook stands in for one), the model keys still do not.
    let dump = fx.base.join("push-env.txt");
    let hook = fx.work.join(".git/hooks/pre-push");
    std::fs::write(&hook, format!("#!/bin/sh\nenv > '{}'\nexit 0\n", dump.to_string_lossy().replace('\\', "/"))).unwrap();
    fx.git(&["switch", "-q", "-c", "feat/net"]);
    fx.commit_file("n.txt", "n\n", "n");
    let o = call(&t, "git", json!({"op": "push"}));
    assert!(!o.is_error, "{}", o.content);
    let seen = std::fs::read_to_string(&dump).expect("the pre-push hook ran");
    assert!(seen.contains("GH_TOKEN=canary-gh"), "{seen}");
    assert!(!seen.contains("canary-openrouter") && !seen.contains("canary-anthropic") && !seen.contains("canary-hf"), "{seen}");
}

#[test]
fn a_crew_hosts_no_push_environment_applies_to_the_git_tool() {
    let fx = Fx::new("nopush");
    fx.git(&["switch", "-q", "-c", "feat/crew"]);
    fx.commit_file("c.txt", "c\n", "c");
    let t = fx.tools().with_child_env([
        ("GIT_CONFIG_COUNT".to_string(), "1".to_string()),
        ("GIT_CONFIG_KEY_0".to_string(), "remote.origin.pushurl".to_string()),
        ("GIT_CONFIG_VALUE_0".to_string(), "nonexistent://no-such-transport/x".to_string()),
    ]);
    let o = call(&t, "git", json!({"op": "push"}));
    assert!(o.is_error, "{}", o.content);
    assert!(!fx.remote_branches().contains(&"feat/crew".to_string()), "nothing reached the remote");
}
