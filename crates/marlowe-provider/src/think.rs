//! **Reasoning that arrives in the `content` channel, split back out of it.**
//!
//! # The bug this exists for
//!
//! Observed live, on a turn that called a tool and then kept thinking. The transcript rendered:
//!
//! ```text
//! ▸ thinking… 1688 characters
//!   ⋯ tool      web search cass lake mn weather today api services
//!       tool
//!
//! I see the tool output shows just "tool ·" -- possibly indicating
//! some kind of error or limitation with how `use` works here, [...]
//!
//! [your reasoning continues]
//! </think>
//! ```
//!
//! Everything below the collapsed thinking block is in the **response** colour. It is not a
//! response. It is the model still reasoning, and the literal `</think>` at the end is the proof
//! — the model was inside a think block the whole time and the harness rendered it as Marlowe
//! speaking. A closing tag reaching the user's screen is the most visible possible statement that
//! the channel split was wrong.
//!
//! # Why the `think` field alone does not fix it
//!
//! Measured against `qwen3.5:9b` on 2026-08-09, three ways:
//!
//! | `think` | reasoning lands in | `</think>` in content |
//! |---|---|---|
//! | `true` | `message.thinking` | no |
//! | `false` | (suppressed) | no |
//! | absent | `message.thinking` | no |
//!
//! So on a **single-shot** call the native channel is reliable and this splitter never fires.
//! The leak is on the **post-tool** iteration, where the model re-opens reasoning inside
//! `content` and closes it with a literal tag. Setting `think` is necessary and it is not
//! sufficient, and the difference between those two is exactly the kind of adjacent answer this
//! project keeps writing down.
//!
//! # The rule, stated as the requirement was
//!
//! *Never switch to the response colour before the model emits `</think>`.*
//!
//! Two shapes satisfy that, and both are handled:
//!
//! 1. **`<think>` … `</think>` in content.** Ordinary nesting. Text inside is reasoning.
//! 2. **A closing tag with no opening tag** — the observed case, because the block was opened
//!    before `content` began. There is no online way to know that a `</think>` is coming, so the
//!    text ahead of it has already been emitted as speech by the time it arrives. It is
//!    **retracted**: [`Split::retract_speech`] tells the caller that everything it has emitted as
//!    speech this turn was reasoning after all, and the transcript moves it into the thinking
//!    block.
//!
//! Retraction is the honest mechanism rather than the pretty one. The alternative — holding every
//! content byte until the turn ends in case a `</think>` shows up — makes the answer stop
//! streaming for **every** model, and streaming is a hard requirement.
//!
//! **How often it fires, measured rather than guessed.** On the first live run after this landed,
//! `--dev`'s frame dump showed a bare closing tag arriving in `content` on **both** model calls of
//! a two-iteration turn. So on the post-tool path this is the ordinary case, not the exotic one, and
//! an earlier draft of this comment calling it rare was wrong. That does not change the trade — it
//! raises how much the correctness of the retraction matters.

/// One classified run of text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    /// Inside a think block. Belongs in the thinking region, never in the transcript.
    Reasoning(String),
    /// Outside every think block. This is what Marlowe said.
    Speech(String),
}

/// What one chunk produced.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Split {
    pub segments: Vec<Segment>,
    /// **Everything previously emitted as speech was reasoning.** Set when a closing tag arrives
    /// with no matching open — see the module header, case 2. The caller drops that text from the
    /// answer and moves it into the thinking block.
    pub retract_speech: bool,
}

/// Both spellings, because models emit both and a set that recognises one renders the other.
const OPEN: [&str; 2] = ["<think>", "<thinking>"];
const CLOSE: [&str; 2] = ["</think>", "</thinking>"];

/// Splits a streamed `content` channel into reasoning and speech.
///
/// Tags are matched across chunk boundaries: a chunk ending in `"</thi"` holds that fragment back
/// rather than emitting it as speech, because a tag arriving one token at a time is the normal
/// case, not the exotic one.
#[derive(Debug, Default)]
pub struct ThinkSplitter {
    inside: bool,
    /// A trailing fragment that might still become a tag.
    pending: String,
    /// Whether any speech has been emitted, so a retraction knows there is something to retract.
    spoke: bool,
    /// Whether a think tag was ever seen on this channel.
    ///
    /// Distinguishes "the model fenced its reasoning inside `content`" from "Ollama parsed the
    /// block and `content` is the answer". At the end of a call those look identical, and without
    /// this the answer gets filed as reasoning and the turn renders silent.
    saw_tag: bool,
}

impl ThinkSplitter {
    pub fn new() -> Self {
        Self::default()
    }

    /// True while the model is inside a think block in the content channel.
    pub fn inside(&self) -> bool {
        self.inside
    }

    /// Whether any `<think>` or `</think>` was seen on this channel.
    pub fn saw_any_tag(&self) -> bool {
        self.saw_tag
    }

    /// Feed one streamed chunk.
    pub fn feed(&mut self, chunk: &str) -> Split {
        let mut split = Split::default();
        let mut buf = std::mem::take(&mut self.pending);
        buf.push_str(chunk);

        let mut rest = buf.as_str();
        while !rest.is_empty() {
            // LOOP-EXEMPT: consuming a bounded buffer, not a driving loop.
            match next_tag(rest) {
                Some((at, len, opening)) => {
                    self.saw_tag = true;
                    self.push(&mut split, &rest[..at]);
                    if opening {
                        self.inside = true;
                    } else {
                        // **A close with no open.** The block began before this channel did, so
                        // the speech already emitted this turn was reasoning.
                        if !self.inside && self.spoke {
                            split.retract_speech = true;
                            split.segments.retain(|s| !matches!(s, Segment::Speech(_)));
                            self.spoke = false;
                        }
                        self.inside = false;
                    }
                    rest = &rest[at + len..];
                }
                None => {
                    // Hold back anything that could still grow into a tag.
                    let keep = partial_tag_from(rest);
                    self.push(&mut split, &rest[..rest.len() - keep]);
                    self.pending = rest[rest.len() - keep..].to_string();
                    break;
                }
            }
        }

        split
    }

    /// The stream ended. Anything still held back was never a tag, so it is ordinary text.
    pub fn finish(&mut self) -> Split {
        let mut split = Split::default();
        let tail = std::mem::take(&mut self.pending);
        self.push(&mut split, &tail);
        split
    }

    fn push(&mut self, split: &mut Split, text: &str) {
        if text.is_empty() {
            return;
        }
        if self.inside {
            split.segments.push(Segment::Reasoning(text.to_string()));
        } else {
            self.spoke = true;
            split.segments.push(Segment::Speech(text.to_string()));
        }
    }
}

/// The earliest tag in `s`: `(byte offset, tag length, is_opening)`.
fn next_tag(s: &str) -> Option<(usize, usize, bool)> {
    let mut best: Option<(usize, usize, bool)> = None;
    for (tag, opening) in OPEN.iter().map(|t| (*t, true)).chain(CLOSE.iter().map(|t| (*t, false))) {
        if let Some(at) = s.find(tag) {
            if best.is_none_or(|(b, _, _)| at < b) {
                best = Some((at, tag.len(), opening));
            }
        }
    }
    best
}

/// How many trailing bytes of `s` could still become a tag.
///
/// Without this, a chunk boundary inside `</think>` emits `"</thi"` as speech — the closing tag
/// reaches the screen in pieces instead of whole, which is the same bug wearing a smaller hat.
fn partial_tag_from(s: &str) -> usize {
    for tag in OPEN.iter().chain(CLOSE.iter()) {
        for take in (1..tag.len()).rev() {
            if s.len() >= take && s.is_char_boundary(s.len() - take) && s[s.len() - take..] == tag[..take] {
                return take;
            }
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn speech(split: &Split) -> String {
        split
            .segments
            .iter()
            .filter_map(|s| match s {
                Segment::Speech(t) => Some(t.as_str()),
                _ => None,
            })
            .collect()
    }

    fn reasoning(split: &Split) -> String {
        split
            .segments
            .iter()
            .filter_map(|s| match s {
                Segment::Reasoning(t) => Some(t.as_str()),
                _ => None,
            })
            .collect()
    }

    /// The ordinary path: no tags, everything is speech, nothing is held back.
    #[test]
    fn content_without_tags_is_speech_and_streams_unchanged() {
        let mut s = ThinkSplitter::new();
        let a = s.feed("The weather in Cass Lake ");
        let b = s.feed("is 12°C.");
        assert_eq!(speech(&a), "The weather in Cass Lake ");
        assert_eq!(speech(&b), "is 12°C.");
        assert!(reasoning(&a).is_empty() && reasoning(&b).is_empty());
        assert!(!a.retract_speech && !b.retract_speech);
    }

    #[test]
    fn a_nested_block_is_reasoning_and_the_tags_never_reach_the_caller() {
        let mut s = ThinkSplitter::new();
        let out = s.feed("<think>weigh the options</think>Here is the answer.");
        assert_eq!(reasoning(&out), "weigh the options");
        assert_eq!(speech(&out), "Here is the answer.");
        assert!(!out.retract_speech);
    }

    /// **The observed bug.** A close with no open: the text ahead of it was reasoning, and the
    /// caller is told to take back what it already rendered as speech.
    #[test]
    fn a_closing_tag_with_no_opening_tag_retracts_the_speech_before_it() {
        let mut s = ThinkSplitter::new();
        let first = s.feed("I see the tool output shows just \"tool\" -- possibly an error");
        assert_eq!(
            speech(&first),
            "I see the tool output shows just \"tool\" -- possibly an error",
            "with no evidence yet, content is speech and it streams"
        );

        let second = s.feed("\n\n[your reasoning continues]\n</think>Here is the weather.");
        assert!(
            second.retract_speech,
            "the closing tag proves the earlier content was reasoning, and the caller must move it"
        );
        assert!(
            !speech(&second).contains("[your reasoning continues]"),
            "reasoning must not survive in the speech channel: {:?}",
            speech(&second)
        );
        assert_eq!(speech(&second), "Here is the weather.");
        assert!(
            !speech(&second).contains("</think>") && !reasoning(&second).contains("</think>"),
            "the tag itself is never text"
        );
    }

    /// A tag arriving one token at a time is the normal case for a streamed channel.
    #[test]
    fn a_tag_split_across_chunks_is_never_emitted_in_pieces() {
        let mut s = ThinkSplitter::new();
        let mut seen = String::new();
        for chunk in ["reasoning", "</thi", "nk>", "answer"] {
            let out = s.feed(chunk);
            seen.push_str(&speech(&out));
            seen.push_str(&reasoning(&out));
        }
        seen.push_str(&speech(&s.finish()));
        assert_eq!(seen, "reasoninganswer");
        assert!(!seen.contains('<'), "no fragment of a tag reached the caller: {seen:?}");
    }

    /// A held-back fragment that never becomes a tag is ordinary text, not a swallowed token.
    #[test]
    fn a_fragment_that_never_becomes_a_tag_is_released_at_the_end() {
        let mut s = ThinkSplitter::new();
        let out = s.feed("cost < ");
        let tail = s.finish();
        assert_eq!(format!("{}{}", speech(&out), speech(&tail)), "cost < ");
    }

    /// Retraction fires once. A second block does not re-retract text already moved.
    #[test]
    fn retraction_applies_only_to_speech_that_is_still_outstanding() {
        let mut s = ThinkSplitter::new();
        s.feed("early reasoning");
        let closed = s.feed("</think>real answer");
        assert!(closed.retract_speech);

        let again = s.feed("<think>more</think> and more");
        assert!(!again.retract_speech, "the real answer must not be retracted");
        assert_eq!(reasoning(&again), "more");
    }
}

#[cfg(test)]
mod hold_rule {
    use super::*;

    /// Replays the shape `--dev` captured on a real 848-frame turn: the opening tag never appears
    /// (the chat template consumed it), 400+ frames of reasoning arrive in `content`, then
    /// `"\n</think>"`, then the answer.
    ///
    /// **The rule under test:** nothing before that closing tag may be classified as speech.
    fn drive(chunks: &[&str], tool_calls: bool) -> (String, String) {
        let mut s = ThinkSplitter::new();
        let (mut held, mut speech, mut reasoning) = (String::new(), String::new(), String::new());
        let mut closed = false;

        for c in chunks {
            let split = s.feed(c);
            if split.retract_speech {
                reasoning.push_str(&held);
                held.clear();
                closed = true;
            }
            for seg in &split.segments {
                match seg {
                    Segment::Reasoning(r) => reasoning.push_str(r),
                    Segment::Speech(t) => {
                        if closed {
                            speech.push_str(t)
                        } else {
                            held.push_str(t)
                        }
                    }
                }
            }
        }
        for seg in &s.finish().segments {
            match seg {
                Segment::Reasoning(r) => reasoning.push_str(r),
                Segment::Speech(t) => {
                    if closed {
                        speech.push_str(t)
                    } else {
                        held.push_str(t)
                    }
                }
            }
        }
        if !held.is_empty() {
            if !tool_calls && (closed || !s.saw_any_tag()) {
                speech.push_str(&held);
            } else {
                reasoning.push_str(&held);
            }
        }
        (speech, reasoning)
    }

    #[test]
    fn nothing_before_the_closing_tag_is_ever_speech() {
        let (speech, reasoning) = drive(
            &[
                " The",
                " error",
                " message",
                " indicates",
                " that",
                " paths",
                " should",
                " be",
                " relative",
                "\n</think>",
                "\n\nhello",
            ],
            false,
        );
        assert_eq!(
            speech.trim(),
            "hello",
            "only what follows `</think>` is the answer; got {speech:?}"
        );
        assert!(
            reasoning.contains("paths") && reasoning.contains("relative"),
            "everything before the tag belongs to the think block: {reasoning:?}"
        );
        assert!(!speech.contains("</think>") && !reasoning.contains("</think>"));
    }

    /// The block never closes and the call ends in a tool call: the model was thinking the whole
    /// time, and none of it is an answer.
    #[test]
    fn an_unclosed_block_that_ends_in_a_tool_call_yields_no_speech() {
        let (speech, reasoning) =
            drive(&["<think>Let me try one more time with cwd=\"\""], true);
        assert_eq!(speech, "", "mid-chain narration is not an answer: {speech:?}");
        assert!(reasoning.contains("one more time"));
    }

    /// **The case that must not regress.** Ollama parsed the block itself, so `content` carries no
    /// tag and is the answer. Holding must not swallow it.
    #[test]
    fn content_with_no_tag_and_no_tool_call_is_the_answer() {
        let (speech, reasoning) = drive(&["2 + 2 = ", "4"], false);
        assert_eq!(speech, "2 + 2 = 4", "a clean answer must still be spoken");
        assert_eq!(reasoning, "");
    }
}


#[cfg(test)]
mod streaming_rule {
    use super::*;

    /// **The reply must stream, and it stopped.**
    ///
    /// Reported live: "streaming works fine on everything BUT the final response". Holding every
    /// content byte until a `</think>` arrived was correct while the model was fencing its
    /// reasoning inside `content` — and once the conversation shape was fixed, Ollama parsed the
    /// block itself and no closing tag ever came. So the hold never resolved until end-of-call and
    /// the answer landed in one lump.
    ///
    /// This drives the resolved rule: **a `thinking` delta means the provider is separating the
    /// channels, so `content` is outside the block and streams.**
    fn drive(native_thinking: bool, chunks: &[&str], thinking_setting: bool) -> Vec<String> {
        let mut s = ThinkSplitter::new();
        let mut closed = !thinking_setting;
        if native_thinking {
            closed = true;
        }
        let mut emitted = Vec::new();
        let mut held = String::new();
        for c in chunks {
            let split = s.feed(c);
            if split.retract_speech {
                held.clear();
                emitted.clear();
                closed = true;
            }
            for seg in &split.segments {
                if let Segment::Speech(t) = seg {
                    if closed {
                        emitted.push(t.clone());
                    } else {
                        held.push_str(t);
                    }
                }
            }
        }
        if !held.is_empty() {
            emitted.push(held);
        }
        emitted
    }

    #[test]
    fn the_reply_streams_chunk_by_chunk_once_the_provider_separates_the_channels() {
        let out = drive(true, &["The ", "first ", "heading ", "is ", "`# State`."], true);
        assert_eq!(
            out.len(),
            5,
            "the reply arrived in {} piece(s); it must stream as it did before the hold: {out:?}",
            out.len()
        );
        assert_eq!(out.concat(), "The first heading is `# State`.");
    }

    /// The hold still applies when there is no evidence the block is shut.
    #[test]
    fn content_with_no_native_thinking_is_held_until_the_tag() {
        let out = drive(false, &["still reasoning here", " and more"], true);
        assert_eq!(out.len(), 1, "unresolved content must not stream: {out:?}");
    }

    /// Thinking off: there is no block, so nothing is ever held.
    #[test]
    fn with_thinking_off_the_reply_streams_immediately() {
        let out = drive(false, &["4", "2"], false);
        assert_eq!(out.len(), 2, "no think channel means no hold: {out:?}");
    }
}
