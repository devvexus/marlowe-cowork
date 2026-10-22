//! `marlowe -p` against a real in-process daemon with a scripted model: the formats, the exit-code
//! matrix, the permission paths and the SDK control messages. The flags' own wiring is in
//! `tests_flags.rs`; the compiled binary is exercised by `tests/headless.rs`.

use std::collections::HashSet;
use std::io::BufRead;
use std::time::{Duration, Instant};

use marlowe_proto::{Event, Request};
use serde_json::{json, Value};

use super::args::Args;
use super::output::capture::Buf;
use super::testkit::*;

// ---------------------------------------------------------------- H1, H2: both paths, formats

#[test]
fn parity_h1_a_print_run_goes_through_the_shared_daemon_and_prints_only_the_answer() {
    let rig = Rig::new("h1-shared");
    rig.script.push(vec![Ev::Text("the answer is 4"), Ev::Usage(10, 5)]);
    // The real connector: `daemon.json` in the home names the running daemon.
    let args = Args::parse(&["-p".into(), "--workspace".into(), rig.ws.display().to_string(), "what is 2+2?".into()]).unwrap();
    let connect = super::connector(&rig.home, &args);
    let o = run_args(args, &rig.home, &rig.ws, None, connect);
    assert_eq!(o.code, 0, "{}", o.err.text());
    assert_eq!(o.out.text(), "the answer is 4\n");
    assert_eq!(o.err.text(), "", "stdout is the answer and nothing else needs saying");
    assert!(rig.script.requests()[0].messages.last().unwrap().joined_text().ends_with("</system-reminder>what is 2+2?"), "the words follow the turn's tail");
}

#[test]
fn parity_h1_stdin_is_appended_to_the_prompt() {
    let rig = Rig::new("h1-stdin");
    let input: Box<dyn BufRead + Send> = Box::new(std::io::Cursor::new(b"line one\nline two\n".to_vec()));
    let o = rig.print_with(&["summarise this"], Some(input));
    assert_eq!(o.code, 0, "{}", o.err.text());
    assert!(rig.script.requests()[0].messages.last().unwrap().joined_text().ends_with("summarise this\n\nline one\nline two"));
    // No prompt argument: the piped text is the prompt.
    let input: Box<dyn BufRead + Send> = Box::new(std::io::Cursor::new(b"just stdin\n".to_vec()));
    let o = rig.print_with(&[], Some(input));
    assert_eq!(o.code, 0);
    assert!(rig.script.requests()[1].messages.last().unwrap().joined_text().ends_with("</system-reminder>just stdin"));
    // Neither: nothing to do, exit 1 and a reason.
    let o = rig.print(&[]);
    assert_eq!(o.code, 1);
    assert!(o.err.text().contains("nothing to do"), "{}", o.err.text());
}

#[test]
fn parity_h2_json_is_one_result_object() {
    let rig = Rig::new("h2-json");
    rig.script.push(vec![Ev::Text("hello"), Ev::Usage(1_000_000, 500_000)]);
    let o = rig.print(&["--output-format", "json", "hi"]);
    assert_eq!(o.code, 0, "{}", o.err.text());
    let l = o.lines();
    assert_eq!(l.len(), 1, "{l:?}");
    let r = &l[0];
    assert_eq!((r["type"].as_str(), r["subtype"].as_str(), r["is_error"].as_bool()), (Some("result"), Some("success"), Some(false)));
    assert_eq!(r["result"], "hello");
    assert_eq!(r["num_turns"], 1);
    assert_eq!(r["usage"]["input_tokens"], 1_000_000);
    assert_eq!(r["usage"]["output_tokens"], 500_000);
    // $1 per million tokens each way.
    assert!((r["total_cost_usd"].as_f64().unwrap() - 1.5).abs() < 1e-9, "{r}");
    assert!(!r["session_id"].as_str().unwrap().is_empty());
    assert_eq!(r["exit_code"], 0);
}

#[test]
fn parity_h2_stream_json_lines_match_the_golden() {
    let rig = Rig::new("h2-stream");
    rig.script.push(vec![Ev::Text("looking"), Ev::Call("c1", "echo", json!({"text": "x"})), Ev::Usage(100, 20)]);
    rig.script.push(vec![Ev::Text("done: x"), Ev::Usage(130, 10)]);
    let o = rig.print(&["--output-format", "stream-json", "--include-partial-messages", "go"]);
    assert_eq!(o.code, 0, "{}", o.err.text());
    let got = golden(&o.lines());
    let want = [
        json!({"type": "system", "subtype": "init", "cwd": "<cwd>", "session_id": "<session_id>", "model": "ollama/scripted", "permissionMode": "default", "mcp_servers": [], "resumed": false, "apiKeySource": "none"}),
        json!({"type": "stream_event", "event": {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "looking"}}, "session_id": "<session_id>"}),
        json!({"type": "assistant", "message": {"role": "assistant", "model": "ollama/scripted", "content": [{"type": "text", "text": "looking"}, {"type": "tool_use", "id": "c1", "name": "echo", "input": {"summary": "echo x"}}], "stop_reason": "tool_use"}, "parent_tool_use_id": null, "session_id": "<session_id>"}),
        json!({"type": "user", "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "c1", "content": "echo ran", "is_error": false}]}, "parent_tool_use_id": null, "session_id": "<session_id>"}),
        json!({"type": "stream_event", "event": {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "done: x"}}, "session_id": "<session_id>"}),
        json!({"type": "assistant", "message": {"role": "assistant", "model": "ollama/scripted", "content": [{"type": "text", "text": "done: x"}], "stop_reason": "end_turn"}, "parent_tool_use_id": null, "session_id": "<session_id>"}),
    ];
    assert_eq!(&got[..want.len()], &want[..], "\n{got:#?}");
    assert_eq!(got.len(), want.len() + 1, "{got:#?}");
    let last = got.last().unwrap();
    assert_eq!(last["type"], "result");
    assert_eq!(last["result"], "done: x");
    assert_eq!(last["num_turns"], 2);
}

#[test]
fn parity_h2_stream_json_input_runs_several_turns_in_one_session() {
    let rig = Rig::new("h2-multi");
    rig.script.push(vec![Ev::Text("first answer"), Ev::Usage(5, 5)]);
    rig.script.push(vec![Ev::Text("second answer"), Ev::Usage(5, 5)]);
    let (w, r) = pipe();
    w.line(user("one"));
    let out = Buf::default();
    let probe = out.clone();
    let feeder = std::thread::spawn(move || {
        // The second message goes in once the first has its result.
        let end = Instant::now() + Duration::from_secs(30);
        while Instant::now() < end && !probe.lines().iter().any(|l| l["type"] == "result") {
            std::thread::sleep(Duration::from_millis(20));
        }
        w.line(user("two"));
        w.close();
    });
    let args = Args::parse(&rig.argv(&["--output-format", "stream-json", "--input-format", "stream-json"])).unwrap();
    let o = run_args_to(args, &rig.home, &rig.ws, Some(r), rig.connector(), out, Buf::default());
    feeder.join().unwrap();
    assert_eq!(o.code, 0, "{}", o.err.text());
    let results: Vec<Value> = o.lines().into_iter().filter(|l| l["type"] == "result").collect();
    assert_eq!(results.len(), 2, "{:?}", o.lines());
    assert_eq!((results[0]["result"].as_str(), results[1]["result"].as_str()), (Some("first answer"), Some("second answer")));
    assert_eq!(results[0]["session_id"], results[1]["session_id"], "one session");
    let inits = o.lines().iter().filter(|l| l["subtype"] == "init").count();
    assert_eq!(inits, 1);
    let second = rig.script.requests()[1].messages.iter().map(|m| m.joined_text()).collect::<Vec<_>>().join("|");
    assert!(second.contains("one") && second.contains("first answer") && second.contains("two"), "history carried: {second}");
}

// ---------------------------------------------------------------- H3: the exit-code matrix

#[test]
fn parity_h3_exit_0_for_a_finished_turn() {
    let rig = Rig::new("h3-0");
    let o = rig.print(&["hi"]);
    assert_eq!(o.code, 0, "{}", o.err.text());
}

#[test]
fn parity_h3_exit_1_for_an_error_and_a_failure_still_ends_in_a_result_object() {
    let rig = Rig::new("h3-1");
    let o = rig.print(&["--resume", "no-such-session", "hi"]);
    assert_eq!(o.code, 1, "{}", o.err.text());
    assert!(o.err.text().contains("no session no-such-session"), "{}", o.err.text());
    let o = rig.print(&["--output-format", "json", "--resume", "nope", "hi"]);
    assert_eq!(o.code, 1);
    assert_eq!(o.result()["is_error"], true);
    assert_eq!(o.result()["exit_code"], 1);
}

#[test]
fn parity_h3_exit_2_only_under_fail_on_ask() {
    let rig = Rig::new("h3-2");
    // Without the flag an unanswerable approval is a denial and the model carries on.
    rig.script.push(vec![Ev::Call("b1", "bash", json!({"command": "rm -rf x"})), Ev::Usage(5, 5)]);
    rig.script.push(vec![Ev::Text("could not"), Ev::Usage(5, 5)]);
    let o = rig.print(&["--output-format", "json", "clean up"]);
    assert_eq!(o.code, 0, "{}", o.err.text());
    assert_eq!(o.result()["permission_denials"][0]["tool_name"], "bash");

    rig.script.push(vec![Ev::Call("b2", "bash", json!({"command": "rm -rf y"})), Ev::Usage(5, 5)]);
    rig.script.push(vec![Ev::Text("not reached"), Ev::Usage(5, 5)]);
    let o = rig.print(&["--output-format", "json", "--fail-on-ask", "clean up again"]);
    assert_eq!(o.code, 2, "{}", o.err.text());
    let r = o.result();
    assert_eq!(r["subtype"], "error_approval_needed");
    assert!(r["error"].as_str().unwrap().contains("bash"), "{r}");
}

#[test]
fn parity_h3_exit_3_when_the_budget_is_spent() {
    let rig = Rig::new("h3-3");
    // 2,000,000 tokens at $1 per million.
    rig.script.push(vec![Ev::Text("expensive"), Ev::Usage(1_000_000, 1_000_000)]);
    let o = rig.print(&["--output-format", "json", "--max-budget-usd", "1.0", "spend"]);
    assert_eq!(o.code, 3, "{}", o.err.text());
    let r = o.result();
    assert_eq!(r["subtype"], "error_max_budget_usd");
    assert_eq!(r["result"], "expensive", "the work done so far is kept");
    rig.script.push(vec![Ev::Text("cheap"), Ev::Usage(1_000, 1_000)]);
    let o = rig.print(&["--max-budget-usd", "1.0", "spend less"]);
    assert_eq!(o.code, 0);
}

#[test]
fn parity_h3_exit_130_when_cancelled() {
    let rig = Rig::new("h3-130");
    rig.script.push(vec![Ev::Text("working"), Ev::WaitCancel]);
    let (w, r) = pipe();
    w.line(user("start"));
    let t = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(700));
        w.line(interrupt("r1"));
        w.close();
    });
    let o = rig.print_with(&["--output-format", "stream-json", "--input-format", "stream-json"], Some(r));
    t.join().unwrap();
    assert_eq!(o.code, 130, "{}\n{}", o.err.text(), o.out.text());
    let res = o.result();
    assert_eq!(res["stop"], "interrupted");
    assert_eq!(res["subtype"], "error_during_execution");
    assert!(o.lines().iter().any(|l| l["type"] == "control_response" && l["response"]["request_id"] == "r1"), "the interrupt was acknowledged");
}

#[test]
fn parity_h3_a_step_limit_is_exit_1_with_its_own_subtype() {
    let rig = Rig::new("h3-steps");
    for id in ["s0", "s1", "s2", "s3", "s4", "s5"] {
        rig.script.push(vec![Ev::Call(id, "echo", json!({"text": "again"})), Ev::Usage(5, 5)]);
    }
    let o = rig.print(&["--output-format", "json", "--max-steps", "2", "loop"]);
    assert_eq!(o.code, 1, "{}", o.err.text());
    assert_eq!(o.result()["subtype"], "error_max_turns");
    assert!(rig.script.requests().len() < 6, "the run stopped at the limit, not at the end of the script");
}

#[test]
fn parity_h3_a_silent_daemon_is_given_up_on_by_the_idle_timeout() {
    let rig = Rig::new("h3-idle");
    rig.script.push(vec![Ev::WaitCancel]);
    let started = Instant::now();
    let o = rig.print(&["--output-format", "json", "--idle-timeout", "1", "hello?"]);
    assert_eq!(o.code, 1, "{}", o.err.text());
    assert!(o.err.text().contains("no activity for 1 s"), "{}", o.err.text());
    assert!(started.elapsed() < Duration::from_secs(15), "never hangs");
}

// ---------------------------------------------------------------- A10: dont_ask recovery

#[test]
fn parity_h3_dont_ask_denies_the_call_and_the_model_recovers_to_exit_0() {
    let rig = Rig::new("a10");
    rig.script.push(vec![Ev::Text("trying"), Ev::Call("b1", "bash", json!({"command": "git push"})), Ev::Usage(5, 5)]);
    rig.script.push(vec![Ev::Text("I could not push, so I stopped"), Ev::Usage(5, 5)]);
    let o = rig.print(&["--output-format", "json", "--permission-mode", "dont_ask", "push it"]);
    assert_eq!(o.code, 0, "{}", o.err.text());
    let r = o.result();
    assert_eq!(r["result"], "I could not push, so I stopped");
    assert_eq!(r["permission_denials"].as_array().unwrap().len(), 1);
    assert!(r["permission_denials"][0]["reason"].as_str().unwrap().contains("dont_ask"));
    assert_eq!(rig.script.requests().len(), 2, "the model was asked again after the refusal");
}

// ---------------------------------------------------------------- H4: allow/deny rules

#[test]
fn parity_h4_allowed_tools_answer_the_approval_and_disallowed_win() {
    let rig = Rig::new("h4-rules");
    rig.script.push(vec![Ev::Call("b1", "bash", json!({"command": "git diff --stat"})), Ev::Usage(5, 5)]);
    rig.script.push(vec![Ev::Text("diffed"), Ev::Usage(5, 5)]);
    let o = rig.print(&["--output-format", "json", "--allowed-tools", "Bash(git diff:*)", "diff"]);
    assert_eq!(o.code, 0, "{}", o.err.text());
    assert_eq!(o.denials(), 0, "{}", o.result());
    assert!(rig.script.tool_results().iter().any(|c| c.contains("bash ran")), "{:?}", rig.script.tool_results());

    // A command the rule does not cover is not allowed.
    rig.script.push(vec![Ev::Call("b2", "bash", json!({"command": "git push"})), Ev::Usage(5, 5)]);
    rig.script.push(vec![Ev::Text("no"), Ev::Usage(5, 5)]);
    let o = rig.print(&["--output-format", "json", "--allowed-tools", "Bash(git diff:*)", "push"]);
    assert_eq!(o.denials(), 1);

    // A deny rule beats an allow rule for the same call.
    rig.script.push(vec![Ev::Call("b3", "bash", json!({"command": "git diff"})), Ev::Usage(5, 5)]);
    rig.script.push(vec![Ev::Text("denied"), Ev::Usage(5, 5)]);
    let o = rig.print(&["--output-format", "json", "--allowed-tools", "Bash", "--disallowed-tools", "Bash(git diff:*)", "diff"]);
    let d = o.result()["permission_denials"][0].clone();
    // The daemon's permission layer holds the run's deny rules and refuses first, naming the rule.
    let why = d["reason"].as_str().unwrap();
    assert!(why.contains("--disallowed-tools") || why.contains("denied by the rule `Bash(git diff:*)`"), "{d}");
}

// ---------------------------------------------------------------- H5: control messages

#[test]
fn parity_h5_can_use_tool_asks_the_sdk_client_and_its_answer_decides() {
    let rig = Rig::new("h5-stdio");
    rig.script.push(vec![Ev::Call("b1", "bash", json!({"command": "echo allowed"})), Ev::Usage(5, 5)]);
    rig.script.push(vec![Ev::Call("b2", "bash", json!({"command": "echo refused"})), Ev::Usage(5, 5)]);
    rig.script.push(vec![Ev::Text("all done"), Ev::Usage(5, 5)]);
    let (w, r) = pipe();
    w.line(user("run two commands"));
    let out = Buf::default();
    let probe = out.clone();
    // The client side: answer each can_use_tool as it shows up in the output.
    let client = std::thread::spawn(move || {
        let mut answered: HashSet<String> = HashSet::new();
        let end = Instant::now() + Duration::from_secs(30);
        while Instant::now() < end && answered.len() < 2 {
            for l in probe.lines() {
                if l["type"] == "control_request" && l["request"]["subtype"] == "can_use_tool" {
                    let id = l["request_id"].as_str().unwrap().to_string();
                    if !answered.insert(id.clone()) {
                        continue;
                    }
                    let cmd = l["request"]["input"]["command"].as_str().unwrap_or("").to_string();
                    let answer = if cmd.contains("refused") {
                        json!({"behavior": "deny", "message": "no"})
                    } else {
                        json!({"behavior": "allow", "updatedInput": l["request"]["input"]})
                    };
                    w.line(json!({"type": "control_response", "response": {"subtype": "success", "request_id": id, "response": answer}}));
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        w.close();
    });
    let args = Args::parse(&rig.argv(&["--output-format", "stream-json", "--input-format", "stream-json", "--permission-prompt-tool", "stdio"])).unwrap();
    let o = run_args_to(args, &rig.home, &rig.ws, Some(r), rig.connector(), out, Buf::default());
    client.join().unwrap();
    assert_eq!(o.code, 0, "{}\n{}", o.err.text(), o.out.text());
    let asks: Vec<Value> = o.lines().into_iter().filter(|l| l["type"] == "control_request").collect();
    assert_eq!(asks.len(), 2, "{asks:?}");
    assert_eq!(asks[0]["request"]["tool_name"], "Bash", "Claude Code's name for it");
    assert_eq!(asks[0]["request"]["input"]["command"], "echo allowed");
    let res = o.result();
    assert_eq!(res["permission_denials"].as_array().unwrap().len(), 1, "{res}");
    assert_eq!(res["result"], "all done");
}

#[test]
fn parity_h5_permission_prompt_tool_is_consulted_and_its_answer_decides() {
    use marlowe_mcp::fake::{FakeHttp, FakeOpts, HttpOpts};
    let gate = FakeHttp::start(HttpOpts { mcp: FakeOpts { gate: true, ..Default::default() }, ..Default::default() });
    let rig = Rig::new("h5-gate");
    rig.script.push(vec![Ev::Call("b1", "bash", json!({"command": "echo fine"})), Ev::Usage(5, 5)]);
    rig.script.push(vec![Ev::Call("b2", "bash", json!({"command": "rm -rf build"})), Ev::Usage(5, 5)]);
    rig.script.push(vec![Ev::Text("finished"), Ev::Usage(5, 5)]);
    let cfg = json!({"mcpServers": {"gate": {"type": "http", "url": gate.url(), "consequence": "inert"}}}).to_string();
    let o = rig.print(&["--output-format", "json", "--strict-mcp-config", "--mcp-config", &cfg, "--permission-prompt-tool", "mcp__gate__approve", "run both"]);
    assert_eq!(o.code, 0, "{}
{}", o.err.text(), o.out.text());
    let r = o.result();
    assert_eq!(r["result"], "finished");
    let denials = r["permission_denials"].as_array().unwrap();
    assert_eq!(denials.len(), 1, "{r}");
    assert!(denials[0]["reason"].as_str().unwrap().contains("the gate refuses rm"), "{r}");
    let ran = rig.script.tool_results().iter().filter(|c| c.contains("bash ran")).count();
    assert_eq!(ran, 1, "only the allowed command ran: {:?}", rig.script.tool_results());
    let asked = gate.log().iter().filter(|l| l.starts_with("POST")).count();
    assert!(asked >= 2, "the tool was called for each ask: {:?}", gate.log());
}

#[test]
fn parity_h5_a_permission_prompt_tool_that_does_not_exist_fails_the_run_at_the_start() {
    let rig = Rig::new("h5-gate-missing");
    let o = rig.print(&["--output-format", "json", "--strict-mcp-config", "--permission-prompt-tool", "mcp__nope__approve", "hi"]);
    assert_eq!(o.code, 1);
    assert!(o.err.text().contains("mcp__nope__approve"), "{}", o.err.text());
    assert!(rig.script.requests().is_empty(), "no model call was made");
}

#[test]
fn parity_h5_set_permission_mode_changes_the_mode_mid_run() {
    let rig = Rig::new("h5-mode");
    // After the switch to dontAsk, a command that would ask is refused, and does not run.
    rig.script.push(vec![Ev::Call("b1", "bash", json!({"command": "echo should-not-run"})), Ev::Usage(5, 5)]);
    rig.script.push(vec![Ev::Text("mode set"), Ev::Usage(5, 5)]);
    let (w, r) = pipe();
    w.line(json!({"type": "control_request", "request_id": "m1", "request": {"subtype": "set_permission_mode", "mode": "dontAsk"}}));
    w.line(user("go"));
    w.close();
    let args = Args::parse(&rig.argv(&["--output-format", "stream-json", "--input-format", "stream-json"])).unwrap();
    let o = run_args(args, &rig.home, &rig.ws, Some(r), rig.connector());
    assert_eq!(o.code, 0, "{}\n{}", o.err.text(), o.out.text());
    let ack = o.lines().into_iter().find(|l| l["type"] == "control_response").expect("an acknowledgement");
    assert_eq!(ack["response"]["request_id"], "m1");
    assert_eq!(ack["response"]["subtype"], "success", "{ack}");
    // The switch has an effect: the ask was refused and the command never ran.
    let res = o.result();
    assert_eq!(res["permission_denials"].as_array().unwrap().len(), 1, "{res}");
    assert!(res["permission_denials"][0]["reason"].as_str().unwrap().contains("dont_ask"), "{res}");
    assert!(!rig.script.tool_results().iter().any(|c| c.contains("bash ran")), "{:?}", rig.script.tool_results());
}

/// With track E merged the daemon holds plan mode, so the switch is acknowledged (a daemon that
/// cannot hold it is `tests_flags.rs`'s `parity_h4_plan_mode_is_an_error_when_the_daemon_cannot_hold_it`).
#[test]
fn parity_h5_set_permission_mode_plan_is_held_by_the_daemon() {
    let rig = Rig::new("h5-mode-plan");
    rig.script.push(vec![Ev::Text("planning"), Ev::Usage(5, 5)]);
    let (w, r) = pipe();
    w.line(json!({"type": "control_request", "request_id": "p1", "request": {"subtype": "set_permission_mode", "mode": "plan"}}));
    w.line(user("go"));
    w.close();
    let args = Args::parse(&rig.argv(&["--output-format", "stream-json", "--input-format", "stream-json"])).unwrap();
    let o = run_args(args, &rig.home, &rig.ws, Some(r), rig.connector());
    assert_eq!(o.code, 0, "{}\n{}", o.err.text(), o.out.text());
    let ack = o.lines().into_iter().find(|l| l["type"] == "control_response").expect("an answer");
    assert_eq!(ack["response"]["subtype"], "success", "{ack}");
    assert_eq!(ack["response"]["response"]["mode"], "plan", "{ack}");
}

#[test]
fn parity_h5_a_malformed_control_message_is_answered_not_fatal() {
    let rig = Rig::new("h5-bad");
    rig.script.push(vec![Ev::Text("fine"), Ev::Usage(5, 5)]);
    let (w, r) = pipe();
    w.line(json!({"type": "control_request", "request_id": "x1", "request": {"subtype": "frobnicate"}}));
    w.line(user("go"));
    w.close();
    let args = Args::parse(&rig.argv(&["--output-format", "stream-json", "--input-format", "stream-json"])).unwrap();
    let o = run_args(args, &rig.home, &rig.ws, Some(r), rig.connector());
    assert_eq!(o.code, 0, "{}", o.err.text());
    let resp = o.lines().into_iter().find(|l| l["type"] == "control_response").unwrap();
    assert_eq!(resp["response"]["subtype"], "error");
    assert!(resp["response"]["error"].as_str().unwrap().contains("frobnicate"));
}

#[test]
fn parity_h5_a_user_message_during_a_turn_steers_it() {
    let rig = Rig::new("h5-steer");
    // First model call is slow; the steer is folded in at the next step.
    rig.script.push(vec![Ev::Sleep(500), Ev::Call("e1", "echo", json!({"text": "a"})), Ev::Usage(5, 5)]);
    rig.script.push(vec![Ev::Text("steered answer"), Ev::Usage(5, 5)]);
    let (w, r) = pipe();
    w.line(user("start"));
    let t = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(250));
        w.line(user("actually, use b"));
        w.close();
    });
    let args = Args::parse(&rig.argv(&["--output-format", "stream-json", "--input-format", "stream-json"])).unwrap();
    let o = run_args(args, &rig.home, &rig.ws, Some(r), rig.connector());
    t.join().unwrap();
    assert_eq!(o.code, 0, "{}", o.err.text());
    let seen = rig.script.requests().last().unwrap().messages.iter().map(|m| m.joined_text()).collect::<Vec<_>>().join("|");
    assert!(seen.contains("actually, use b"), "the model saw the steer: {seen}");
    let results = o.lines().into_iter().filter(|l| l["type"] == "result").count();
    assert!(results >= 1, "{:?}", o.lines());
}

// ---------------------------------------------------------------- A resumed session

#[test]
fn parity_h4_continue_and_resume_reuse_the_session() {
    let rig = Rig::new("h4-resume");
    rig.script.push(vec![Ev::Text("first"), Ev::Usage(5, 5)]);
    let o = rig.print(&["--output-format", "json", "remember the number 7"]);
    let first = o.result()["session_id"].as_str().unwrap().to_string();

    rig.script.push(vec![Ev::Text("second"), Ev::Usage(5, 5)]);
    let o = rig.print(&["--output-format", "json", "-c", "what number?"]);
    assert_eq!(o.code, 0, "{}", o.err.text());
    assert_eq!(o.result()["session_id"], first, "-c continues the newest session of this workspace");
    let seen = rig.script.requests().last().unwrap().messages.iter().map(|m| m.joined_text()).collect::<Vec<_>>().join("|");
    assert!(seen.contains("remember the number 7") && seen.contains("what number?"), "{seen}");

    rig.script.push(vec![Ev::Text("third"), Ev::Usage(5, 5)]);
    let o = rig.print(&["--output-format", "json", "-r", &first, "again"]);
    assert_eq!(o.result()["session_id"], first, "-r names it");
    assert_eq!(rig.sessions().iter().filter(|s| s.id.0 == first).count(), 1);
    assert_eq!(rig.sessions().len(), 1, "no extra session was made");

    // Another workspace has no session to continue.
    let other = rig.home.join("other");
    std::fs::create_dir_all(&other).unwrap();
    let mut v = rig.argv(&["--output-format", "json", "-c", "hi"]);
    v[2] = other.display().to_string();
    let o = run_args(Args::parse(&v).unwrap(), &rig.home, &other, None, rig.connector());
    assert_eq!(o.code, 1);
    assert!(o.err.text().contains("no earlier session"), "{}", o.err.text());
}

// ---------------------------------------------------------------- answers that never come

/// A client thread: waits for the first `can_use_tool` request in `probe`, then runs `then`.
fn on_first_ask(probe: Buf, then: impl FnOnce(Value) + Send + 'static) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let end = Instant::now() + Duration::from_secs(30);
        while Instant::now() < end {
            if let Some(l) = probe.lines().into_iter().find(|l| l["type"] == "control_request" && l["request"]["subtype"] == "can_use_tool") {
                return then(l);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("no can_use_tool request appeared");
    })
}

#[test]
fn parity_h5_an_allow_with_a_rewritten_input_is_refused_and_the_original_does_not_run() {
    let rig = Rig::new("h5-updated");
    rig.script.push(vec![Ev::Call("b1", "bash", json!({"command": "rm -rf build"})), Ev::Usage(5, 5)]);
    rig.script.push(vec![Ev::Text("finished"), Ev::Usage(5, 5)]);
    let (w, r) = pipe();
    w.line(user("clean up"));
    let out = Buf::default();
    let client = on_first_ask(out.clone(), move |l| {
        let id = l["request_id"].as_str().unwrap().to_string();
        // "Allow, but sanitised": a narrower path.
        w.line(json!({"type": "control_response", "response": {"subtype": "success", "request_id": id, "response": {"behavior": "allow", "updatedInput": {"command": "rm -rf build/tmp"}}}}));
        w.close();
    });
    let args = Args::parse(&rig.argv(&["--output-format", "stream-json", "--input-format", "stream-json", "--permission-prompt-tool", "stdio"])).unwrap();
    let o = run_args_to(args, &rig.home, &rig.ws, Some(r), rig.connector(), out, Buf::default());
    client.join().unwrap();
    assert_eq!(o.code, 0, "{}\n{}", o.err.text(), o.out.text());
    let res = o.result();
    assert_eq!(res["permission_denials"].as_array().unwrap().len(), 1, "{res}");
    assert!(res["permission_denials"][0]["reason"].as_str().unwrap().contains("updatedInput is not supported yet"), "{res}");
    assert!(!rig.script.tool_results().iter().any(|c| c.contains("bash ran")), "neither the original nor the rewrite ran: {:?}", rig.script.tool_results());
}

#[test]
fn parity_h5_closing_stdin_with_a_question_outstanding_denies_it_and_ends_promptly() {
    let rig = Rig::new("h5-eof");
    rig.script.push(vec![Ev::Call("b1", "bash", json!({"command": "echo never-answered"})), Ev::Usage(5, 5)]);
    rig.script.push(vec![Ev::Text("gave up asking"), Ev::Usage(5, 5)]);
    let (w, r) = pipe();
    w.line(user("go"));
    let out = Buf::default();
    // The client sees the request and hangs up without answering.
    let client = on_first_ask(out.clone(), move |_| w.close());
    // Without the fix this waited for the (default 30 minute) idle timeout.
    let args = Args::parse(&rig.argv(&["--output-format", "stream-json", "--input-format", "stream-json", "--permission-prompt-tool", "stdio", "--idle-timeout", "60"])).unwrap();
    let started = Instant::now();
    let o = run_args_to(args, &rig.home, &rig.ws, Some(r), rig.connector(), out, Buf::default());
    client.join().unwrap();
    assert!(started.elapsed() < Duration::from_secs(15), "ended in {:?}: {}", started.elapsed(), o.err.text());
    assert_eq!(o.code, 0, "{}\n{}", o.err.text(), o.out.text());
    let res = o.result();
    assert_eq!(res["permission_denials"].as_array().unwrap().len(), 1, "{res}");
    assert!(res["permission_denials"][0]["reason"].as_str().unwrap().contains("closed its input"), "{res}");
    assert!(!rig.script.tool_results().iter().any(|c| c.contains("bash ran")));
    assert!(o.lines().iter().any(|l| l["type"] == "control_cancel_request"), "the client is told the request is moot");
}

fn approval_fake(after: Vec<Event>) -> Fake {
    use marlowe_proto::ApprovalId;
    let tail = std::sync::Mutex::new(after);
    Fake::with(move |r| match r {
        Request::Ask { session, .. } => {
            let s = session.clone().unwrap_or_else(fake_session);
            let mut evs = vec![
                Event::TurnStarted { session: s.clone(), turn: 0, model: model_info().model_ref(), user: String::new() },
                Event::ApprovalRequested {
                    approval: ApprovalId(7),
                    session: s.clone(),
                    tool: "bash".into(),
                    summary: "bash x".into(),
                    detail: json!({"command": "x"}).to_string(),
                    run: None,
                    proposed_rule: None,
                },
            ];
            evs.extend(std::mem::take(&mut *tail.lock().unwrap()));
            evs
        }
        other => default_reply(other),
    })
}

fn stream_fake_run(fake: &Fake) -> Outcome {
    let home = temp_dir("pending-home");
    let ws = home.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let (w, r) = pipe();
    w.line(user("go"));
    // Hangs up once the run has had time to take the events in; the run ends with its turn.
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(1500));
        w.close();
    });
    let mut v = vec!["-p".to_string(), "--workspace".into(), ws.display().to_string()];
    v.extend(["--output-format", "stream-json", "--input-format", "stream-json", "--permission-prompt-tool", "stdio", "--idle-timeout", "20"].map(String::from));
    run_args(Args::parse(&v).unwrap(), &home, &ws, Some(r), fake.connector())
}

#[test]
fn parity_h5_an_approval_the_daemon_settles_itself_is_forgotten_and_the_client_told() {
    use marlowe_proto::{ApprovalDecision, ApprovalId, TurnStop};
    let s = fake_session();
    let fake = approval_fake(vec![
        Event::ApprovalResolved { approval: ApprovalId(7), decision: ApprovalDecision::Deny },
        // The turn goes on after the daemon's own denial.
        Event::ToolStarted { session: s.clone(), turn: 0, call_id: "c1".into(), tool: "echo".into(), summary: "echo".into() },
        Event::ToolFinished { session: s.clone(), turn: 0, call_id: "c1".into(), ok: true, summary: "echo".into() },
        Event::TurnFinished { session: s, turn: 0, usage: Default::default(), stop: TurnStop::Done },
    ]);
    let o = stream_fake_run(&fake);
    assert_eq!(o.code, 0, "{}\n{}", o.err.text(), o.out.text());
    let lines = o.lines();
    let at = |pred: &dyn Fn(&Value) -> bool| lines.iter().position(|l| pred(l));
    let ask = at(&|l| l["type"] == "control_request").expect("the request");
    let cancel = at(&|l| l["type"] == "control_cancel_request").expect("the cancel");
    let later_tool = at(&|l| l["type"] == "assistant").expect("the turn's later tool call");
    assert!(ask < cancel && cancel < later_tool, "cancelled when the daemon settled it, not when the turn ended: {lines:?}");
    assert_eq!(lines.iter().filter(|l| l["type"] == "control_cancel_request").count(), 1);
    assert_eq!(lines[ask]["request_id"], lines[cancel]["request_id"]);
}

#[test]
fn parity_h5_an_interrupted_turn_does_not_leave_the_run_waiting_for_the_answer() {
    use marlowe_proto::TurnStop;
    let s = fake_session();
    let fake = approval_fake(vec![Event::TurnFinished { session: s, turn: 0, usage: Default::default(), stop: TurnStop::Interrupted }]);
    let started = Instant::now();
    let o = stream_fake_run(&fake);
    assert!(started.elapsed() < Duration::from_secs(15));
    let lines = o.lines();
    assert_eq!(lines.iter().filter(|l| l["type"] == "control_cancel_request").count(), 1, "{lines:?}");
    let cancel = lines.iter().position(|l| l["type"] == "control_cancel_request").unwrap();
    let result = lines.iter().position(|l| l["type"] == "result").unwrap();
    assert!(cancel < result);
}

// ---------------------------------------------------------------- stdin that never ends

#[test]
fn parity_h1_an_open_silent_stdin_does_not_hold_a_run_that_has_a_prompt() {
    let rig = Rig::new("h1-open-stdin");
    rig.script.push(vec![Ev::Text("answered"), Ev::Usage(5, 5)]);
    // A parent that spawned us with a pipe it never writes to or closes. `w` stays alive.
    let (w, r) = pipe();
    let started = Instant::now();
    let o = rig.print_with(&["--output-format", "json", "what is 2+2"], Some(r));
    assert!(started.elapsed() < Duration::from_secs(15), "returned in {:?}", started.elapsed());
    assert_eq!(o.code, 0, "{}", o.err.text());
    assert_eq!(o.result()["result"], "answered");
    assert!(o.err.text().contains("no data on stdin"), "said so: {}", o.err.text());
    drop(w);
}

#[test]
fn parity_h1_stdin_that_starts_late_but_ends_is_still_appended() {
    let rig = Rig::new("h1-late-stdin");
    rig.script.push(vec![Ev::Text("saw the data"), Ev::Usage(5, 5)]);
    let (w, r) = pipe();
    w.line(json!("LATE-DATA-ARRIVED"));
    w.close();
    let o = rig.print_with(&["--output-format", "json", "review:"], Some(r));
    assert_eq!(o.code, 0, "{}", o.err.text());
    let seen = rig.script.requests().last().unwrap().messages.iter().map(|m| m.joined_text()).collect::<Vec<_>>().join("|");
    assert!(seen.contains("LATE-DATA-ARRIVED") && seen.contains("review:"), "{seen}");
}

// ---------------------------------------------------------------- run servers do not outlive the run

#[test]
fn parity_h4_mcp_config_servers_are_gone_for_the_next_session_of_the_workspace() {
    use marlowe_proto::SettingsScope;
    let rig = Rig::new("h4-mcp-gone");
    rig.script.push(vec![Ev::Text("ok"), Ev::Usage(5, 5)]);
    let cfg = json!({"mcpServers": {"ghost": {"command": "marlowe-no-such-program"}}}).to_string();
    let o = rig.print(&["--output-format", "json", "--mcp-config", &cfg, "hi"]);
    assert_eq!(o.code, 0, "{}", o.err.text());
    // A second session on the same workspace (the interactive one) must not see it.
    let ws = crate::strip_verbatim(rig.ws.canonicalize().unwrap()).display().to_string();
    let mut c = RawClient::connect(&rig.info);
    c.send(&Request::McpList { workspace: Some(ws.clone()) });
    match c.until(|e| matches!(e, Event::McpServers { .. })) {
        Event::McpServers { list } => assert!(list.iter().all(|s| s.name != "ghost"), "{list:?}"),
        _ => unreachable!(),
    }
    // And a name added by a client that then vanishes without removing it goes too.
    let mut gone = RawClient::connect(&rig.info);
    gone.send(&Request::McpAdd { name: "orphan".into(), scope: SettingsScope::Session, config: json!({"command": "marlowe-no-such-program"}), workspace: Some(ws.clone()) });
    gone.until(|e| matches!(e, Event::McpServers { .. }));
    drop(gone);
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        c.send(&Request::McpList { workspace: Some(ws.clone()) });
        let Event::McpServers { list } = c.until(|e| matches!(e, Event::McpServers { .. })) else { unreachable!() };
        if list.iter().all(|s| s.name != "orphan") {
            break;
        }
        assert!(Instant::now() < end, "still listed: {list:?}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

// ---------------------------------------------------------------- --standalone beside a live daemon

#[test]
fn parity_h4_standalone_runs_beside_a_daemon_that_already_serves_the_home() {
    let rig = Rig::new("h4-beside");
    // The rig's daemon holds the home. A standalone run used to be refused outright.
    let (info, stop) = super::standalone(&rig.home, false, false).expect("a private daemon starts beside the live one");
    assert_ne!(info.port, rig.info.port, "its own daemon, not the shared one");
    let mut c = RawClient::connect(&info);
    c.send(&Request::LibraryState);
    match c.until(|e| matches!(e, Event::LibraryState { .. })) {
        Event::LibraryState { models, .. } => assert!(models.iter().any(|m| m.model.model_id == "scripted"), "the model library came along: {models:?}"),
        _ => unreachable!(),
    }
    // The live daemon is untouched.
    let mut live = RawClient::connect(&rig.info);
    live.send(&Request::Status);
    live.until(|e| matches!(e, Event::Status(_)));
    stop();
}
