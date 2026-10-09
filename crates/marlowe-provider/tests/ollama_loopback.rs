//! `OllamaProvider` against a scripted loopback server: no live inference.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use marlowe_model::{
    CancelToken, ChatRequest, Effort, Message, Provider, ProviderErrorKind, StopReason,
    StreamEvent, SystemBlock,
};
use marlowe_provider::{LocalEndpoint, OllamaProvider};
use serde_json::{json, Value};

/// What one request looked like.
#[derive(Debug, Clone)]
struct Seen {
    path: String,
    body: Value,
}

enum Reply {
    /// Status and a body sent with Content-Length.
    Json(u16, String),
    /// NDJSON lines sent chunk by chunk.
    Chunked(Vec<String>),
    /// Headers and the given lines, then the connection stays open and silent.
    Stall(Vec<String>),
}

type Route = Box<dyn Fn(&str) -> Reply + Send + Sync>;

fn serve(route: Route) -> (LocalEndpoint, Arc<Mutex<Vec<Seen>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen2 = seen.clone();
    let route = Arc::new(route);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { break };
            let seen = seen2.clone();
            let route = route.clone();
            thread::spawn(move || handle(stream, &seen, &route));
        }
    });
    (LocalEndpoint::new("127.0.0.1", port).unwrap(), seen)
}

fn handle(stream: TcpStream, seen: &Mutex<Vec<Seen>>, route: &Route) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut first = String::new();
    reader.read_line(&mut first).unwrap();
    let path = first.split_whitespace().nth(1).unwrap_or("").to_string();
    let mut length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
            break;
        }
        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = v.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).ok();
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    seen.lock().unwrap().push(Seen { path: path.clone(), body });

    let mut out = stream;
    match route(&path) {
        Reply::Json(status, text) => {
            let head = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                text.len()
            );
            out.write_all(head.as_bytes()).unwrap();
            out.write_all(text.as_bytes()).unwrap();
        }
        Reply::Chunked(lines) => {
            out.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nTransfer-Encoding: chunked\r\n\r\n")
                .unwrap();
            for l in lines {
                let data = format!("{l}\n");
                out.write_all(format!("{:x}\r\n{data}\r\n", data.len()).as_bytes()).unwrap();
                out.flush().unwrap();
            }
            out.write_all(b"0\r\n\r\n").unwrap();
        }
        Reply::Stall(lines) => {
            out.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nConnection: close\r\n\r\n")
                .unwrap();
            for l in lines {
                out.write_all(format!("{l}\n").as_bytes()).unwrap();
            }
            out.flush().unwrap();
            thread::sleep(Duration::from_secs(20));
        }
    }
    out.flush().ok();
}

fn show_thinking() -> String {
    json!({
        "capabilities": ["completion", "tools", "thinking"],
        "details": {"family": "qwen35"},
        "model_info": {"general.architecture": "qwen35", "qwen35.context_length": 262144}
    })
    .to_string()
}

fn request() -> ChatRequest {
    let mut req = ChatRequest::new("qwen3.5:9b", vec![Message::user("what is 2+2")]);
    req.system = vec![SystemBlock { text: "You are Marlowe.".into(), cache: true }];
    req.effort = Effort::Medium;
    req
}

#[test]
fn a_streamed_turn_folds_into_events_and_a_completion() {
    let (endpoint, seen) = serve(Box::new(|path| match path {
        "/api/show" => Reply::Json(200, show_thinking()),
        "/api/chat" => Reply::Chunked(vec![
            r#"{"message":{"role":"assistant","thinking":"Two"},"done":false}"#.into(),
            r#"{"message":{"role":"assistant","thinking":" plus two"},"done":false}"#.into(),
            r#"{"message":{"role":"assistant","content":"4"},"done":false}"#.into(),
            r#"{"message":{"role":"assistant","content":""},"done":true,"done_reason":"stop","prompt_eval_count":13,"eval_count":6}"#.into(),
        ]),
        _ => Reply::Json(404, "{}".into()),
    }));
    let p = OllamaProvider::new(endpoint);
    let mut events = Vec::new();
    let c = p.stream(&request(), &CancelToken::new(), &mut |e| events.push(e)).unwrap();

    assert_eq!(c.text, "4");
    assert_eq!(c.thinking.as_ref().unwrap().text, "Two plus two");
    assert_eq!(c.usage.input, 13);
    assert_eq!(c.usage.output, 6);
    assert_eq!(c.usage.reasoning, 5);
    assert_eq!(c.stop, StopReason::EndTurn);
    assert!(events.iter().any(|e| matches!(e, StreamEvent::TextDelta(t) if t == "4")));
    assert!(matches!(events.last(), Some(StreamEvent::Stop(StopReason::EndTurn))));

    let seen_handle = seen;
    let seen = seen_handle.lock().unwrap();
    let chat = seen.iter().find(|s| s.path == "/api/chat").unwrap();
    assert_eq!(chat.body["think"], true, "a thinking model gets think: true at medium effort");
    assert_eq!(chat.body["options"]["num_ctx"], 32_768);
    assert_eq!(chat.body["messages"][0]["role"], "system");
    assert_eq!(chat.body["messages"][1]["content"], "what is 2+2");

    drop(seen);

    // The capability lookup is cached: a second call does not re-ask /api/show.
    p.stream(&request(), &CancelToken::new(), &mut |_| {}).unwrap();
    let seen = seen_handle.lock().unwrap();
    assert_eq!(seen.iter().filter(|s| s.path == "/api/show").count(), 1);
}

#[test]
fn effort_off_turns_thinking_off_and_a_non_thinking_model_gets_no_think_field() {
    let (endpoint, seen) = serve(Box::new(|path| match path {
        "/api/show" => Reply::Json(200, show_thinking()),
        _ => Reply::Chunked(vec![r#"{"message":{"content":"ok"},"done":true}"#.into()]),
    }));
    let p = OllamaProvider::new(endpoint);
    let mut req = request();
    req.effort = Effort::Off;
    p.stream(&req, &CancelToken::new(), &mut |_| {}).unwrap();
    let body = seen.lock().unwrap().iter().find(|s| s.path == "/api/chat").unwrap().body.clone();
    assert_eq!(body["think"], false);

    let (endpoint, seen) = serve(Box::new(|path| match path {
        "/api/show" => Reply::Json(200, json!({"capabilities":["completion"],"model_info":{}}).to_string()),
        _ => Reply::Chunked(vec![r#"{"message":{"content":"ok"},"done":true}"#.into()]),
    }));
    OllamaProvider::new(endpoint).stream(&request(), &CancelToken::new(), &mut |_| {}).unwrap();
    let body = seen.lock().unwrap().iter().find(|s| s.path == "/api/chat").unwrap().body.clone();
    assert!(body.get("think").is_none(), "think on a non-thinking model is a 400");
}

#[test]
fn list_models_uses_tags_and_asks_show_only_when_tags_are_incomplete() {
    let (endpoint, seen) = serve(Box::new(|path| match path {
        "/api/tags" => Reply::Json(
            200,
            json!({"models":[
                {"name":"qwen3.5:9b","details":{"family":"qwen35","context_length":262144},
                 "capabilities":["completion","tools","thinking"]},
                {"name":"old:7b","details":{"family":"llama"}},
                {"name":"nomic-embed-text:latest","details":{"context_length":2048},"capabilities":["embedding"]},
                {"name":"glm-5:cloud","details":{"context_length":202752},"capabilities":["completion","tools","thinking"]}
            ]})
            .to_string(),
        ),
        "/api/show" => Reply::Json(
            200,
            json!({"capabilities":["completion"],"model_info":{"general.architecture":"llama","llama.context_length":8192}})
                .to_string(),
        ),
        _ => Reply::Json(404, "{}".into()),
    }));
    let models = OllamaProvider::new(endpoint).list_models().unwrap();
    let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["glm-5:cloud", "old:7b", "qwen3.5:9b"], "embedders are not chat models");
    let old = models.iter().find(|m| m.id == "old:7b").unwrap();
    assert_eq!(old.context_window, Some(8192));
    assert!(!old.supports_tools && old.effort_levels.is_empty());
    let qwen = models.iter().find(|m| m.id == "qwen3.5:9b").unwrap();
    assert!(qwen.supports_tools && qwen.supports_reasoning && qwen.local);
    assert_eq!(qwen.effort_levels, vec![Effort::Off, Effort::Medium]);
    assert!(!models.iter().find(|m| m.id == "glm-5:cloud").unwrap().local);
    let shows = seen.lock().unwrap().iter().filter(|s| s.path == "/api/show").count();
    assert_eq!(shows, 1, "only the entry without capabilities needs /api/show");
}

#[test]
fn cancel_returns_promptly_while_the_server_is_silent() {
    let (endpoint, _) = serve(Box::new(|path| match path {
        "/api/show" => Reply::Json(200, show_thinking()),
        _ => Reply::Stall(vec![r#"{"message":{"thinking":"hmm"},"done":false}"#.into()]),
    }));
    let p = OllamaProvider::new(endpoint);
    let cancel = CancelToken::new();
    let c2 = cancel.clone();
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(300));
        c2.cancel();
    });
    let started = Instant::now();
    let mut got_reasoning = false;
    let err = p
        .stream(&request(), &cancel, &mut |e| {
            if matches!(e, StreamEvent::ReasoningDelta { .. }) {
                got_reasoning = true;
            }
        })
        .unwrap_err();
    assert_eq!(err.kind, ProviderErrorKind::Cancelled, "{err}");
    assert!(got_reasoning, "frames before the stall were delivered");
    assert!(started.elapsed() < Duration::from_secs(3), "took {:?}", started.elapsed());
}

#[test]
fn a_missing_model_names_the_pull_command() {
    let (endpoint, _) = serve(Box::new(|_| {
        Reply::Json(404, r#"{"error":"model 'nope:1b' not found"}"#.into())
    }));
    let mut req = request();
    req.model = "nope:1b".into();
    let err = OllamaProvider::new(endpoint).stream(&req, &CancelToken::new(), &mut |_| {}).unwrap_err();
    assert_eq!(err.kind, ProviderErrorKind::BadRequest);
    assert!(err.detail.contains("ollama pull nope:1b"), "{}", err.detail);
}

#[test]
fn an_absent_server_is_a_network_error_with_a_remedy() {
    let p = OllamaProvider::new(LocalEndpoint::new("127.0.0.1", 1).unwrap());
    let err = p.list_models().unwrap_err();
    assert_eq!(err.kind, ProviderErrorKind::Network);
    assert!(err.detail.contains("ollama serve"), "{}", err.detail);
    assert!(!p.is_running(Duration::from_millis(300)));
}

#[test]
fn a_stream_that_ends_without_done_is_an_error_not_a_short_answer() {
    let (endpoint, _) = serve(Box::new(|path| match path {
        "/api/show" => Reply::Json(200, show_thinking()),
        _ => Reply::Chunked(vec![r#"{"message":{"content":"partial"},"done":false}"#.into()]),
    }));
    let err = OllamaProvider::new(endpoint).stream(&request(), &CancelToken::new(), &mut |_| {}).unwrap_err();
    assert_eq!(err.kind, ProviderErrorKind::Network);
}

#[test]
fn a_mid_stream_error_frame_is_a_server_error() {
    let (endpoint, _) = serve(Box::new(|path| match path {
        "/api/show" => Reply::Json(200, show_thinking()),
        _ => Reply::Chunked(vec![r#"{"error":"llama runner process has terminated"}"#.into()]),
    }));
    let err = OllamaProvider::new(endpoint).stream(&request(), &CancelToken::new(), &mut |_| {}).unwrap_err();
    assert_eq!(err.kind, ProviderErrorKind::Server);
    assert!(err.detail.contains("terminated"));
}

/// Catalog against the real local Ollama (`/api/tags` + `/api/show` only, no inference).
#[test]
#[ignore = "needs a running Ollama"]
fn live_list_models() {
    let models = OllamaProvider::default().list_models().unwrap();
    for m in &models {
        println!(
            "{} ctx={:?} tools={} reasoning={} efforts={:?} local={}",
            m.id, m.context_window, m.supports_tools, m.supports_reasoning, m.effort_levels, m.local
        );
    }
    assert!(!models.is_empty());
}
