//! A scripted loopback Ollama for the provider tests: no live inference.
#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use marlowe_provider::LocalEndpoint;
use serde_json::Value;

/// What one request looked like.
#[derive(Debug, Clone)]
pub struct Seen {
    pub path: String,
    pub body: Value,
}

pub enum Reply {
    /// Status and a body sent with Content-Length.
    Json(u16, String),
    /// NDJSON lines sent chunk by chunk.
    Chunked(Vec<String>),
    /// Headers and the given lines, then the connection stays open and silent.
    Stall(Vec<String>),
}

pub type Route = Box<dyn Fn(&str) -> Reply + Send + Sync>;

pub fn serve(route: Route) -> (LocalEndpoint, Arc<Mutex<Vec<Seen>>>) {
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

