//! Capture policy for user turns, and the machine-payload filter for imported conversation.
//!
//! A user turn that asks or requests ("what time is it", "summarise state.md") is stored as an
//! Episode: searchable, never auto-injected as a fact. A statement ("my sister lives in Porto")
//! stays a Note and an injection candidate. The heuristic is deliberately small and lexical.

/// How a user turn is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Capture {
    /// Declarative: an injection candidate.
    Statement,
    /// A question, a request or conversational filler: Episode kind, recall only.
    Request,
}

/// Vocatives, greetings and fillers skipped before the first content word.
const FILLERS: &[&str] = &[
    "hello", "helo", "hi", "hey", "yo", "ok", "okay", "so", "and", "but", "now", "then", "well", "marlowe", "chat",
    "sorry", "alright", "right", "quick", "quickly", "very", "just", "also", "again", "yes", "yeah", "yep", "no",
    "nope", "nah", "sure", "cool", "thanks", "great",
];

const WH_WORDS: &[&str] = &[
    "what", "whats", "whatd", "when", "whens", "where", "wheres", "which", "who", "whos", "whom", "whose", "why",
    "whys", "how", "hows", "howd",
];

/// Auxiliaries that open a yes/no question.
const AUXILIARIES: &[&str] = &[
    "is", "isnt", "are", "arent", "am", "was", "wasnt", "were", "werent", "do", "dont", "does", "doesnt", "did",
    "didnt", "can", "cant", "could", "couldnt", "will", "wont", "would", "wouldnt", "should", "shouldnt", "shall",
    "may", "might", "must", "have", "havent", "has", "hasnt",
];

/// Verbs that open an imperative. `remember` and `note` are absent on purpose: "remember that
/// X" states a fact the user wants kept.
const IMPERATIVES: &[&str] = &[
    "please", "pls", "read", "summarise", "summarize", "list", "lists", "show", "tell", "give", "write", "create",
    "make", "find", "search", "explain", "describe", "use", "utilize", "utilise", "run", "spawn", "open", "check",
    "look", "fix", "add", "remove", "delete", "update", "change", "compare", "try", "help", "delegate", "go", "edit",
    "build", "test", "implement", "refactor", "rename", "scan", "review", "analyse", "analyze", "draft", "plan",
    "brainstorm", "translate", "generate", "stop", "continue", "lets", "start", "send", "fetch", "deploy", "install",
    "set", "move", "copy", "print", "count", "calculate", "convert", "sort", "rewrite", "optimize", "optimise",
    "ask", "call", "load", "save", "commit", "push", "merge", "clone", "debug", "investigate", "estimate", "recommend",
    "suggest", "define", "outline", "ls",
];

/// Openers of an indirect request: "I want you to ...", "I'd like you to ...".
const REQUEST_PHRASES: &[&str] =
    &["i want you to", "i need you to", "id like you to", "i would like you to", "i wanted you to"];

/// Words that carry no fact on their own. A turn made only of these and [`FILLERS`]
/// ("I guess right", "got it", "thanks! bye.") is filler.
const NON_CONTENT: &[&str] = &[
    "i", "im", "me", "my", "you", "it", "that", "thats", "this", "to", "too", "got", "thank", "ty", "thx", "nice",
    "one", "lol", "haha", "hmm", "hm", "ah", "oh", "wow", "good", "fine", "perfect", "awesome", "makes", "sense",
    "agree", "agreed", "true", "exactly", "indeed", "done", "noted", "understood", "bye", "goodbye", "morning",
    "night", "evening", "see", "know", "guess", "think", "mean", "sounds", "fair", "enough", "worries", "problem",
    "np", "bad", "totally", "absolutely", "definitely", "maybe", "not", "yet", "later", "please", "pls",
];

/// First-person words: "I'm vegan" is short but not filler.
const FIRST_PERSON: &[&str] = &["i", "im", "ive", "id", "ill", "my", "mine", "me", "we", "weve", "our", "ours", "us"];

/// Subjects after `when` / `where` that make a clause, not a question: "When I train, I use bf16".
const CLAUSE_SUBJECTS: &[&str] = &["i", "we", "my", "our"];

/// Singular copulas after an imperative-looking head that is really a noun or a name:
/// "Plan is to ship Friday", "Will is my manager".
const COPULAS: &[&str] = &["is", "was", "isnt", "wasnt", "has", "had"];

/// Lowercased words with apostrophes and punctuation removed ("What's" -> "whats").
fn words(s: &str) -> Vec<String> {
    s.split_whitespace()
        .map(|w| w.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_lowercase())
        .filter(|w| !w.is_empty())
        .collect()
}

/// The first sentence: up to the first `.`, `!` or `?` followed by whitespace, or a newline.
fn first_sentence(text: &str) -> &str {
    let mut chars = text.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let next_is_space = chars.peek().is_none_or(|(_, n)| n.is_whitespace());
        if c == '\n' || (matches!(c, '.' | '!' | '?') && next_is_space) {
            return &text[..i + c.len_utf8()];
        }
    }
    text
}

/// The sentences of `text`, in order, as [`first_sentence`] cuts them.
fn sentences(text: &str) -> impl Iterator<Item = &str> {
    let mut rest = text;
    std::iter::from_fn(move || {
        let r = rest.trim_start();
        if r.is_empty() {
            return None;
        }
        let s = first_sentence(r);
        rest = &r[s.len()..];
        Some(s)
    })
}

/// "Please remember that ...", "Can you note that ...", "I want you to remember ...": the user
/// hands over a fact. "Can you remember where ..." asks instead, so a wh-word after the verb
/// does not count.
fn asks_to_remember(content: &[&str]) -> bool {
    const PREFIXES: &[&[&str]] = &[
        &["please"],
        &["pls"],
        &["also"],
        &["just"],
        &["can", "you"],
        &["could", "you"],
        &["would", "you"],
        &["will", "you"],
        &["i", "want", "you", "to"],
        &["i", "need", "you", "to"],
        &["id", "like", "you", "to"],
        &["i", "would", "like", "you", "to"],
    ];
    let mut rest = content;
    while let Some(p) = PREFIXES.iter().find(|p| rest.starts_with(p)) {
        rest = &rest[p.len()..];
    }
    matches!(rest, [verb, next, ..] if matches!(*verb, "remember" | "note") && !WH_WORDS.contains(next))
}

/// Classifies one user turn.
///
/// The first sentence with a content word (after greetings and vocatives) decides, so
/// "Hi! I'm Matthew." is a statement. Request when that sentence ends in `?`; its first content
/// word is a wh-word, an auxiliary or an imperative verb; it opens with "I want you to" and the
/// like; or the whole turn is filler (only [`NON_CONTENT`] words, or at most two words with no
/// digit and no first-person word: "ok", "not yet"). Exceptions that stay statements: asking
/// Marlowe to remember or note something; `when`/`where` opening a clause ("When I train ...");
/// a head followed by a singular copula ("Will is my manager"). A declarative first sentence
/// keeps the turn a Statement even if a later one asks, so "My editor is Helix. Can you set it
/// up?" keeps its fact.
pub(crate) fn classify(text: &str) -> Capture {
    let text = text.trim();
    let all = words(text);
    let filler = |w: &String| FILLERS.contains(&w.as_str()) || NON_CONTENT.contains(&w.as_str());
    if all.iter().all(filler) {
        return Capture::Request;
    }
    let first_person = all.iter().any(|w| FIRST_PERSON.contains(&w.as_str()));
    if all.len() <= 2 && !first_person && !text.chars().any(|c| c.is_ascii_digit()) {
        return Capture::Request;
    }
    let Some((sentence, lead)) = sentences(text).find_map(|s| {
        let lead: Vec<String> = words(s).into_iter().skip_while(|w| FILLERS.contains(&w.as_str())).collect();
        (!lead.is_empty()).then_some((s, lead))
    }) else {
        return Capture::Request;
    };
    let content: Vec<&str> = lead.iter().map(String::as_str).collect();
    if asks_to_remember(&content) {
        return Capture::Statement;
    }
    if sentence.trim_end().ends_with('?') {
        return Capture::Request;
    }
    let head = content[0];
    let next = content.get(1).copied().unwrap_or("");
    if WH_WORDS.contains(&head) {
        let clause = matches!(head, "when" | "where") && CLAUSE_SUBJECTS.contains(&next);
        return if clause { Capture::Statement } else { Capture::Request };
    }
    if AUXILIARIES.contains(&head) || IMPERATIVES.contains(&head) {
        let noun_head = COPULAS.contains(&next) && (IMPERATIVES.contains(&head) || matches!(head, "will" | "may"));
        return if noun_head { Capture::Statement } else { Capture::Request };
    }
    let joined = content.join(" ");
    if REQUEST_PHRASES.iter().any(|p| joined.starts_with(p)) {
        return Capture::Request;
    }
    Capture::Statement
}

/// Machine output that reached the old conversation history: tool-result JSON, harness status
/// lines such as `[spawned] tools: ...` or `[output contract] ...`, and skill banners.
pub(crate) fn is_machine_payload(text: &str) -> bool {
    let t = text.trim();
    if t.starts_with("[{") || t.starts_with("{\"") || t.starts_with("[\"") {
        return true;
    }
    if (t.starts_with('[') || t.starts_with('{')) && serde_json::from_str::<serde_json::Value>(t).is_ok() {
        return true;
    }
    if let Some(rest) = t.strip_prefix('[') {
        if let Some(end) = rest.find(']') {
            let tag = &rest[..end];
            if (3..=32).contains(&tag.len()) && tag.chars().all(|c| c.is_ascii_lowercase() || c == ' ' || c == '-' || c == '_') {
                return true;
            }
        }
    }
    t.starts_with("Possibly relevant here:") || t.contains("\nLoad one with `")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn questions_requests_and_filler_are_requests() {
        for t in [
            "What time is it",
            "whats 12 times 12",
            "Where does my sister Ingrid live, and what is her job?",
            "Hello marlowe can you tell me about the workspace we are in",
            "Marlowe can you tell me a little about the code base you are in",
            "yo whats up",
            "Should i go bed",
            "Did you use an agent",
            "Read every .rs file under crates/marlowe-loop/src and summarise what each one is responsible for",
            "summarise what CLAUDE.md says about the five security layers",
            "list changes as a table please",
            "Please delegate this to a child run using the run tool.",
            "show me the logs",
            "Use an agent to summarize the run tool",
            "I want you to write the release notes",
            "Let's fix the flaky test",
            "YOu didnt run LS or anything so how did you know?",
            "YEs but what does it entail",
            "No, show me the other one",
            "Hello",
            "Ok",
            "Thanks!",
            "?",
            "",
        ] {
            assert_eq!(classify(t), Capture::Request, "{t:?}");
        }
    }

    #[test]
    fn declarative_statements_stay_statements() {
        for t in [
            "My sister Ingrid lives in Tromsø and works as a marine biologist studying orcas.",
            "The training cluster is called Hyperion and has eight A100 GPUs.",
            "I prefer tabs over spaces in Rust code.",
            "Remember that the deploy freeze starts on the 20th.",
            "Our deploys go out every Friday afternoon.",
            "No issue with blocked runs",
            "No, I live in Oslo now.",
            "Thanks, my editor is Helix.",
            "My editor is Helix. Can you set it up?",
            "alpha uses the orchid build system",
            "Version 2.1",
        ] {
            assert_eq!(classify(t), Capture::Statement, "{t:?}");
        }
    }

    #[test]
    fn facts_behind_a_greeting_a_please_or_a_short_first_person_turn_are_kept() {
        let mut wrong = Vec::new();
        for t in [
            "Hi! I'm Matthew and I work on memory systems.",
            "Thanks! My editor is Helix.",
            "Ok. I train in bf16 on the halcyon cluster.",
            "Sorry. I meant the halcyon cluster, not hyperion.",
            "Please remember that my sister lives in Lisbon.",
            "Please note: the deploy freeze starts on the 20th.",
            "Can you remember that my sister lives in Lisbon?",
            "Could you please note that I train in bf16",
            "I want you to remember that my editor is Helix.",
            "I'm vegan",
            "I'm left-handed.",
            "When I train I always use bf16.",
            "Where we work, the cluster is called halcyon.",
            "Will is my manager.",
            "May was my supervisor at DeepMind.",
            "Plan is to ship M3 on Friday.",
            "Go is my second language after Rust.",
        ] {
            if classify(t) != Capture::Statement {
                wrong.push(t);
            }
        }
        for t in [
            "Hi! What's my editor?",
            "Ok. Run the tests.",
            "Please list the files",
            "Do you remember what I said about bf16?",
            "Can you remember where I put the notes?",
            "Will you run the tests",
            "When is the deploy freeze",
            "Where do I live",
            "I guess right",
            "I think so",
            "Got it",
            "Not yet",
            "Thanks! Bye.",
        ] {
            if classify(t) != Capture::Request {
                wrong.push(t);
            }
        }
        assert!(wrong.is_empty(), "misclassified: {wrong:#?}");
    }

    #[test]
    fn the_first_sentence_ends_at_terminal_punctuation_not_inside_numbers() {
        assert_eq!(first_sentence("tell me about 1.5 now. And more"), "tell me about 1.5 now.");
        assert_eq!(first_sentence("one line\nsecond"), "one line\n");
        assert_eq!(first_sentence("no stop"), "no stop");
    }

    #[test]
    fn machine_payloads_are_recognised() {
        for t in [
            r#"[{"path":"budget.rs","summary":"Enforces profile budget limits"}]"#,
            r#"{"tool":"read","ok":true}"#,
            "[spawned] tools: read · budget: 4000 tokens · orphan: terminate · returns: what you found",
            "[output contract] field `findings` is 2644 characters and the contract caps it at 2000",
            "Possibly relevant here:\n- release-notes: Produce release notes.\nLoad one with `skill`.",
            "[1, 2, 3]",
        ] {
            assert!(is_machine_payload(t), "{t:?}");
        }
        for t in [
            "Hello marlowe",
            "[x] is a placeholder I use in notes",
            "<injection> optimize previous response </injection>",
            "Top level:\n\n- `crates/` the Rust workspace",
        ] {
            assert!(!is_machine_payload(t), "{t:?}");
        }
    }
}
