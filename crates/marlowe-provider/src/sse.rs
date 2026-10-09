//! `text/event-stream`, decoded as the bytes land.
//!
//! `data:` lines are joined per event, `data: [DONE]` ends the stream, and `:` comment lines
//! (OpenRouter keepalives) are counted and skipped. Lives here, not in `marlowe-openrouter`, so a
//! local SSE server can share it without pulling TLS into this crate.

use std::io::BufRead;

/// One decoded event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// A `data:` payload that was not the sentinel.
    Data(String),
    /// `data: [DONE]`. The stream is over and **nothing follows it that matters**.
    Done,
}

/// The sentinel, spelled once.
pub const DONE: &str = "[DONE]";

pub struct SseStream<R: BufRead> {
    reader: R,
    finished: bool,
    /// Bytes seen, so a stream that produced no frames can say whether it was empty or full of
    /// something unrecognised. A zero-frame call is otherwise indistinguishable from a hang.
    pub bytes_seen: usize,
    pub comments_seen: usize,
}

impl<R: BufRead> SseStream<R> {
    pub fn new(reader: R) -> Self {
        Self { reader, finished: false, bytes_seen: 0, comments_seen: 0 }
    }

    /// The next event, or `None` at end of stream. Blocks until one is available.
    pub fn next_frame(&mut self) -> Option<Result<Frame, std::io::Error>> {
        if self.finished {
            return None;
        }
        let mut data: Vec<String> = Vec::new();
        loop {
            // LOOP-EXEMPT: consuming a response stream, not a driving loop.
            let mut line = String::new();
            match self.reader.read_line(&mut line) {
                Ok(0) => {
                    self.finished = true;
                    // A partial event at EOF is still an event: the upstream closed mid-stream and
                    // discarding what it did send would lose the last token of the answer.
                    return (!data.is_empty()).then(|| Ok(Frame::Data(data.join("\n"))));
                }
                Ok(n) => {
                    self.bytes_seen += n;
                    let line = line.trim_end_matches(['\r', '\n']);
                    if line.is_empty() {
                        // End of one event.
                        if data.is_empty() {
                            continue;
                        }
                        let joined = data.join("\n");
                        if joined.trim() == DONE {
                            self.finished = true;
                            return Some(Ok(Frame::Done));
                        }
                        return Some(Ok(Frame::Data(joined)));
                    }
                    if let Some(rest) = line.strip_prefix(':') {
                        // A keepalive. Counted rather than dropped silently: `comments_seen`
                        // without frames is "queued behind a busy upstream", which is a different
                        // fact from "nothing arrived".
                        let _ = rest;
                        self.comments_seen += 1;
                        continue;
                    }
                    if let Some(rest) = line.strip_prefix("data:") {
                        data.push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
                        continue;
                    }
                    // `event:`, `id:`, `retry:` and anything else. OpenRouter sends none of them
                    // on this endpoint; ignoring rather than erroring is what the specification
                    // requires of a client, and a stricter reading buys nothing.
                }
                Err(e) => {
                    self.finished = true;
                    return Some(Err(e));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufReader;

    fn frames(wire: &str) -> Vec<Frame> {
        let mut s = SseStream::new(BufReader::new(wire.as_bytes()));
        let mut out = Vec::new();
        while let Some(f) = s.next_frame() {
            out.push(f.expect("a decodable frame"));
        }
        out
    }

    #[test]
    fn the_done_sentinel_is_a_sentinel_and_not_malformed_json() {
        // `[DONE]` is not JSON. A decoder that parses every `data:` payload reports a malformed
        // frame on the last frame of EVERY successful call — an error on the success path, which
        // is the kind that gets suppressed and then hides a real one.
        let f = frames("data: {\"a\":1}\n\ndata: [DONE]\n\n");
        assert_eq!(f, vec![Frame::Data("{\"a\":1}".into()), Frame::Done]);
    }

    #[test]
    fn a_keepalive_comment_is_not_a_frame_and_not_an_error() {
        // OpenRouter emits `: OPENROUTER PROCESSING` while a request waits behind a busy
        // upstream. Treating it as a protocol error would fail precisely the slow requests
        // streaming exists for.
        let mut s = SseStream::new(BufReader::new(
            ": OPENROUTER PROCESSING\n\n: OPENROUTER PROCESSING\n\ndata: {\"a\":1}\n\ndata: [DONE]\n\n"
                .as_bytes(),
        ));
        let mut out = Vec::new();
        while let Some(f) = s.next_frame() {
            out.push(f.expect("a decodable frame"));
        }
        assert_eq!(out, vec![Frame::Data("{\"a\":1}".into()), Frame::Done]);
        assert_eq!(s.comments_seen, 2, "keepalives are counted, not silently dropped");
    }

    #[test]
    fn nothing_after_done_is_read() {
        // The sentinel ends the stream. Anything after it is a server that kept talking, and
        // reading it would be reading past the end of the answer.
        let f = frames("data: [DONE]\n\ndata: {\"late\":true}\n\n");
        assert_eq!(f, vec![Frame::Done]);
    }

    #[test]
    fn a_stream_cut_short_yields_what_arrived_rather_than_nothing() {
        // The upstream closed mid-event. The bytes it did send are the last of the answer, and
        // dropping them would silently truncate a reply.
        let f = frames("data: {\"a\":1}");
        assert_eq!(f, vec![Frame::Data("{\"a\":1}".into())]);
    }

    #[test]
    fn a_multi_line_data_field_is_joined_with_newlines() {
        // The specification says so. OpenRouter sends one line today; "today" is not a property.
        let f = frames("data: line one\ndata: line two\n\n");
        assert_eq!(f, vec![Frame::Data("line one\nline two".into())]);
    }
}
