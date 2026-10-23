//! Reading the next request's prefix before the request exists.
//!
//! Some chat templates print an earlier turn differently once a user message follows it (Qwen's
//! drops the turn's thinking), so the prompt the model's cache holds after a reply is not the
//! start of the prompt the next message will render to. The cache then serves only up to the
//! difference and the model re-reads the turn that just ended, while the user waits.
//!
//! The moment a turn ends, the template is asked how the conversation looks to the next request:
//! the history plus a placeholder user message, cut where the placeholder begins. That text is the
//! next request's prefix, whatever the message turns out to be, and it is read into the
//! conversation's slot in the background (prompt only, no generation). The next real request then
//! reads only the new message. [`next_prefix`] finds the text, [`read_ahead`] reads it.

use std::time::Duration;

use serde_json::{json, Value};

use crate::http::{self, LocalEndpoint};

const QUICK: Duration = Duration::from_secs(10);
/// A read-ahead is as long as the conversation; a stuck server is cut off after this.
const READ_AHEAD_TIMEOUT: Duration = Duration::from_secs(900);
/// Tokens from the end of the text searched for the special token a message begins with.
const TAIL_TOKENS: usize = 8;
/// Longest stop marker (`<|im_end|>`, `<end_of_turn>`, `[/INST]`) the cache may hold past the end
/// of the next prefix: the server stops on it and the template prints it in its own way.
const MARKER_MAX: usize = 24;

/// Longest common prefix of `a` and `b`, in bytes, on a character boundary.
pub fn common_prefix(a: &str, b: &str) -> usize {
    let mut n = 0;
    for (x, y) in a.chars().zip(b.chars()) {
        if x != y {
            break;
        }
        n += x.len_utf8();
    }
    n
}

/// Whether the next request can start from a cache that holds `cache`: the rendering `next`
/// begins with it, give or take a stop marker at its end. Anything else the cache holds past the
/// first difference is text the model would read again.
pub fn covers(next: &str, cache: &str) -> bool {
    let tail = cache[common_prefix(next, cache)..].trim_end();
    let marker = |open: char, close: char| tail.starts_with(open) && tail.ends_with(close) && !tail.contains(char::is_whitespace);
    tail.is_empty() || (tail.len() <= MARKER_MAX && (marker('<', '>') || marker('[', ']')))
}

/// Where the last special token of `text` begins (a byte offset), asking the server's own
/// tokenizer. A cut there splits the text where the tokenizer would split it whatever follows, so
/// the tokens read ahead are exactly the first tokens of the next request.
pub fn cut_before_last_special(endpoint: &LocalEndpoint, text: &str) -> Option<usize> {
    let body = json!({"content": text, "add_special": false, "parse_special": true, "with_pieces": true});
    let v = http::post_json(endpoint, "/tokenize", &body, QUICK).ok()?;
    let tokens: Vec<(u64, Vec<u8>)> = v
        .get("tokens")?
        .as_array()?
        .iter()
        .map(|t| {
            let id = t.get("id")?.as_u64()?;
            let piece = match t.get("piece")? {
                Value::String(s) => s.as_bytes().to_vec(),
                Value::Array(a) => a.iter().filter_map(|b| b.as_u64().map(|b| b as u8)).collect(),
                _ => return None,
            };
            Some((id, piece))
        })
        .collect::<Option<_>>()?;
    if tokens.iter().map(|(_, p)| p.len()).sum::<usize>() != text.len() {
        return None;
    }
    let mut end = text.len();
    for (id, piece) in tokens.iter().rev().take(TAIL_TOKENS) {
        end -= piece.len();
        // A special token is one the tokenizer reads from its text only when told to.
        let plain = json!({"content": String::from_utf8_lossy(piece), "add_special": false, "parse_special": false});
        let read = http::post_json(endpoint, "/tokenize", &plain, QUICK).ok()?;
        let ids = read.get("tokens")?.as_array()?;
        let special = !(ids.len() == 1 && ids[0].as_u64() == Some(*id));
        if special && text.is_char_boundary(end) && text.as_bytes()[end..].starts_with(piece) {
            return Some(end);
        }
    }
    None
}

/// The text the next request will begin with: `render(extra)` is the conversation plus `extra`
/// messages rendered by the model's template without the generation prompt. Two placeholder user
/// messages that differ at their first character share exactly what comes before them.
pub fn next_prefix(endpoint: &LocalEndpoint, render: &dyn Fn(&[Value]) -> Option<String>, reply: &Value) -> Option<String> {
    let with = |text: &str| render(&[reply.clone(), json!({"role": "user", "content": text})]);
    let (a, b) = (with("alpha")?, with("bravo")?);
    let shared = &a[..common_prefix(&a, &b)];
    let cut = cut_before_last_special(endpoint, shared)?;
    let prefix = &shared[..cut];
    (!prefix.is_empty()).then(|| prefix.to_string())
}

/// Reads `prefix` into `slot` and generates nothing. Returns the server's complaint, if any.
pub fn read_ahead(endpoint: &LocalEndpoint, slot: u32, prefix: &str) -> Result<(), String> {
    let body = json!({"prompt": prefix, "n_predict": 0, "cache_prompt": true, "id_slot": slot, "stream": false, "temperature": 0.0});
    http::post_json(endpoint, "/completion", &body, READ_AHEAD_TIMEOUT).map(|_| ()).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llama::testkit::{chatml_with, Handler, Http, Reply, Tpl};
    use std::sync::Arc;

    fn fake() -> (Http, LocalEndpoint) {
        let handler: Arc<Handler> = Arc::new(|_, path, body| match path {
            "/tokenize" => Reply::json(crate::llama::testkit::fake_tokenize(body)),
            _ => Reply::status(404, json!({})),
        });
        let http = Http::start(handler);
        let ep = http.endpoint();
        (http, ep)
    }

    fn render_with(tpl: Tpl, messages: Vec<Value>) -> impl Fn(&[Value]) -> Option<String> {
        move |extra: &[Value]| {
            let mut all = messages.clone();
            all.extend(extra.iter().cloned());
            Some(chatml_with(&json!({"messages": all, "add_generation_prompt": false}), tpl))
        }
    }

    fn history() -> Vec<Value> {
        vec![json!({"role": "user", "content": "one"})]
    }

    fn reply() -> Value {
        json!({"role": "assistant", "content": "answer", "reasoning_content": "pondering"})
    }

    #[test]
    fn the_prefix_ends_where_the_next_message_begins() {
        let (_http, ep) = fake();
        let p = next_prefix(&ep, &render_with(Tpl::Drops, history()), &reply()).unwrap();
        assert_eq!(p, "<|im_start|>user\none<|im_end|>\n<|im_start|>assistant\nanswer<|im_end|>\n", "no wrapper of the next message, and the turn printed as the next request prints it");
        assert!(!p.contains("pondering"), "this template drops the thinking once a message follows");
        let kept = next_prefix(&ep, &render_with(Tpl::Keeps, history()), &reply()).unwrap();
        assert!(kept.contains("<think>\npondering\n</think>"));
    }

    #[test]
    fn a_cache_is_covered_by_a_rendering_that_starts_with_it_give_or_take_the_stop_token() {
        assert!(covers("abcdef", "abc"));
        assert!(covers("abcdef", "abcdef<|im_end|>"));
        assert!(covers("abcdef", "abcdef<|im_end|>
"));
        assert!(!covers("abcdef", "abcXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX"));
        assert!(!covers("abcdef", "abcdef and more"), "text past the end is read again");
        assert!(!covers("abc d", "abc answer"), "a short tail that is words, not a stop marker, is not covered");
    }

    /// Against a real llama-server on a model file (CPU is enough; nothing is generated):
    /// `MARLOWE_LLAMA_LIVE_PORT=<port> cargo test -p marlowe-provider live_ -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs a running llama-server (MARLOWE_LLAMA_LIVE_PORT)"]
    fn live_the_model_files_own_template_and_tokenizer_answer() {
        let port: u16 = std::env::var("MARLOWE_LLAMA_LIVE_PORT").expect("MARLOWE_LLAMA_LIVE_PORT").parse().unwrap();
        let ep = LocalEndpoint::new("127.0.0.1", port).unwrap();
        let thinking = crate::llama::template::probe(&ep);
        println!("THINKING {thinking:?}");
        let render = |extra: &[Value]| {
            let mut messages = history();
            messages.extend(extra.iter().cloned());
            let body = json!({"messages": messages, "chat_template_kwargs": {"enable_thinking": true}, "add_generation_prompt": false});
            http::post_json(&ep, "/apply-template", &body, QUICK).ok()?.get("prompt")?.as_str().map(str::to_string)
        };
        let prefix = next_prefix(&ep, &render, &reply());
        println!("PREFIX {prefix:?}");
        let prefix = prefix.expect("a prefix");
        assert!(!prefix.contains("pondering"), "{prefix:?}");
        let wrapper = render(&[reply(), json!({"role": "user", "content": "alpha"})]).unwrap();
        assert!(wrapper.starts_with(&prefix), "the next request begins with the prefix");
        let tail = &wrapper[prefix.len()..];
        println!("NEXT MESSAGE BEGINS {tail:?}");
    }

    #[test]
    fn nothing_is_cut_when_the_server_cannot_tokenize() {
        let ep = LocalEndpoint::new("127.0.0.1", 9).unwrap();
        assert_eq!(cut_before_last_special(&ep, "<|im_start|>user\n"), None);
        assert_eq!(next_prefix(&ep, &render_with(Tpl::Drops, history()), &reply()), None);
    }
}
