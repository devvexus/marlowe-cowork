//! BERT WordPiece, hand-rolled.
//!
//! **Why hand-rolled, when `tokenizers` exists.** Same argument the lexical cue makes against
//! FTS5, applied one layer down: `marlowe-eval repro` hashes the injected set byte for byte, so
//! anything that can change the tokenization can change a published number. A tokenizer library
//! is a dependency whose *behaviour* is versioned — and the parts of it we need are ~200 lines
//! that can be asserted against a committed reference. The reference
//! (`tests/fixtures/wordpiece-reference.json`) is generated from HuggingFace's own
//! `BertTokenizer`, so this is not a reimplementation nobody checked; it is a reimplementation
//! checked against the authority on every `cargo test`.
//!
//! Two Unicode tables are still dependencies (`unicode-normalization` for NFD,
//! `unicode-general-category` for the category checks). That is not a contradiction of the
//! above — it is the honest version of it. The tables cannot be avoided without shipping our
//! own copy of them, and **what makes them safe is the fixture, not their absence**: a table
//! change that moved a single token id would fail
//! `the_tokenizer_reproduces_huggingface_exactly` by name, rather than surfacing as two
//! different sha256s a month later.
//!
//! The algorithm is `BertTokenizer` with the settings pinned in
//! `models/jina-embeddings-v2-small-en/tokenizer_config.json`, in order:
//!
//! 1. clean — drop NUL, U+FFFD and control characters; fold every whitespace form to a space
//! 2. space out CJK characters (`tokenize_chinese_chars: true`)
//! 3. split on whitespace
//! 4. lowercase, then **strip accents** — `strip_accents: null` means *derive from
//!    `do_lower_case`*, so accents ARE stripped. "null" reads like "off" and is not; this is the
//!    single most likely place for a silent disagreement with the reference.
//! 5. split on punctuation, each punctuation character its own token
//! 6. WordPiece: greedy longest-match-first, `##` on every piece after the first
//!
//! Every constant here is a **frozen parameter** under HP1, exactly as the BM25 constants are.

use std::collections::BTreeMap;

use unicode_general_category::{get_general_category, GeneralCategory};
use unicode_normalization::UnicodeNormalization;

/// A word longer than this becomes a single `[UNK]` rather than being pieced.
///
/// BERT's own value. It exists so a pathological token (a base64 blob, a minified line) cannot
/// cost quadratic time in the longest-match loop.
pub const MAX_INPUT_CHARS_PER_WORD: usize = 100;

pub const UNK: &str = "[UNK]";
pub const CLS: &str = "[CLS]";
pub const SEP: &str = "[SEP]";
pub const PAD: &str = "[PAD]";
pub const MASK: &str = "[MASK]";

/// The special-token literals HuggingFace splits out of the *input text* before tokenizing.
///
/// Verified against `BertTokenizer` directly, because the behaviour is not what the plain
/// WordPiece algorithm gives and every property of it is easy to guess wrong:
///
/// | Input | Result | Note |
/// |---|---|---|
/// | `[CLS]` | `[CLS]` | matched |
/// | `[cls]` | `[`, `cl`, `##s`, `]` | **case-sensitive** — `normalized: false` on the added token |
/// | `x[MASK]y` | `x`, `[MASK]`, `y` | matches mid-word, no boundary required |
/// | `[MASKED]` | `[`, `masked`, `]` | not a match; the closing bracket must follow |
///
/// Ordered longest-first so a literal that is a prefix of another cannot shadow it. None of
/// these five currently is, and relying on that would be relying on a coincidence.
const SPECIAL_LITERALS: [&str; 5] = [MASK, UNK, CLS, SEP, PAD];

#[derive(Debug, thiserror::Error)]
pub enum VocabError {
    #[error(
        "{path} is missing the special token {token:?}. This is not a jina-embeddings-v2-small-en \
         vocabulary; refusing rather than tokenizing into ids the model never saw"
    )]
    MissingSpecial { path: String, token: &'static str },

    #[error(
        "{path} holds {found} entries; the expected vocabulary has {expected}. A different \
         vocabulary produces token ids the embedding matrix was not trained on, and the model \
         would still return a plausible-looking vector"
    )]
    WrongSize { path: String, found: usize, expected: usize },

    #[error("{path} is not a readable tokenizer.json: no model.vocab object")]
    MalformedTokenizerJson { path: String },

    #[error(
        "{path} declares normalizer.{key} = {found}, this build implements {expected}. The \
         tokenizer would produce different ids than the model's own, and the graph would still \
         return a plausible score. See DECISIONS.md ADR-004"
    )]
    NormalizerDisagrees { path: String, key: &'static str, expected: bool, found: String },
}

/// jina-embeddings-v2-small-en's vocabulary size, from the model's own `config.json`.
///
/// 30,528 rather than all-MiniLM-L6-v2's 30,522. The five special tokens keep the same ids
/// (`[PAD]` 0, `[UNK]` 100, `[CLS]` 101, `[SEP]` 102, `[MASK]` 103), verified against both
/// tokenizers, so the algorithm is unchanged and only the size check moves.
pub const VOCAB_SIZE: usize = 30528;

/// The WordPiece vocabulary: token -> id.
#[derive(Debug, Clone)]
pub struct Vocab {
    /// BTreeMap, not HashMap — the determinism guard bans hash-ordered collections outright,
    /// and lookup order here is not worth an allowlist entry.
    tokens: BTreeMap<String, u32>,
    unk_id: u32,
    cls_id: u32,
    sep_id: u32,
}

/// `ms-marco-MiniLM-L-2-v2`'s vocabulary size, from its own `tokenizer.json`.
///
/// 30,522 — plain `bert-base-uncased`, where jina's 30,528 is the same vocabulary padded. Verified
/// id-for-id: the cross-encoder's vocabulary is exactly the first 30,522 entries of jina's
/// `vocab.txt`, so `encode`'s WordPiece is the same algorithm over a prefix of the same table.
pub const CROSS_ENCODER_VOCAB_SIZE: usize = 30522;

impl Vocab {
    /// Parse a `vocab.txt`: one token per line, id = line number.
    pub fn parse(text: &str, path: &str) -> Result<Self, VocabError> {
        Self::parse_with_size(text, path, VOCAB_SIZE)
    }

    /// Parse a HuggingFace `tokenizer.json`, which is what the cross-encoder ships instead of a
    /// `vocab.txt`.
    ///
    /// **The normalizer settings are checked, not assumed.** This build implements exactly one
    /// normalization — BERT's, lowercasing, keeping accents, cleaning control characters, folding
    /// Chinese characters. `tokenizer.json` *declares* its normalizer, so a pinned file that ever
    /// changed one of those flags would leave this WordPiece silently tokenizing differently from
    /// the model's own tokenizer, producing plausible ids and a plausible score. Refused at load.
    pub fn from_tokenizer_json(text: &str, path: &str) -> Result<Self, VocabError> {
        let root: serde_json::Value = serde_json::from_str(text)
            .map_err(|_| VocabError::MalformedTokenizerJson { path: path.to_string() })?;

        let expect_flag = |key: &'static str, want: bool| -> Result<(), VocabError> {
            let found = root["normalizer"][key].as_bool();
            if found != Some(want) {
                return Err(VocabError::NormalizerDisagrees {
                    path: path.to_string(),
                    key,
                    expected: want,
                    found: found.map(|b| b.to_string()).unwrap_or_else(|| "absent".into()),
                });
            }
            Ok(())
        };
        expect_flag("lowercase", true)?;
        expect_flag("clean_text", true)?;
        expect_flag("handle_chinese_chars", true)?;
        // `strip_accents: null` means "follow lowercase", and this build keeps accents by
        // decomposing without stripping. An explicit `true` would be a different tokenizer.
        if !root["normalizer"]["strip_accents"].is_null() {
            return Err(VocabError::NormalizerDisagrees {
                path: path.to_string(),
                key: "strip_accents",
                expected: false,
                found: root["normalizer"]["strip_accents"].to_string(),
            });
        }

        let map = root["model"]["vocab"]
            .as_object()
            .ok_or_else(|| VocabError::MalformedTokenizerJson { path: path.to_string() })?;
        let mut tokens = BTreeMap::new();
        for (token, id) in map {
            let id = id
                .as_u64()
                .ok_or_else(|| VocabError::MalformedTokenizerJson { path: path.to_string() })?;
            tokens.insert(token.clone(), id as u32);
        }
        Self::finish(tokens, path, CROSS_ENCODER_VOCAB_SIZE)
    }

    fn parse_with_size(text: &str, path: &str, expected: usize) -> Result<Self, VocabError> {
        let mut tokens = BTreeMap::new();
        for (index, line) in text.lines().enumerate() {
            // `vocab.txt` is one token per line and the token may not be trimmed of its own
            // content — only the line ending. `lines()` already removes \n; strip a stray \r
            // so a CRLF checkout does not shift every id by producing "token\r".
            let token = line.strip_suffix('\r').unwrap_or(line);
            tokens.insert(token.to_string(), index as u32);
        }
        Self::finish(tokens, path, expected)
    }

    fn finish(
        tokens: BTreeMap<String, u32>,
        path: &str,
        expected: usize,
    ) -> Result<Self, VocabError> {
        if tokens.len() != expected {
            return Err(VocabError::WrongSize {
                path: path.to_string(),
                found: tokens.len(),
                expected,
            });
        }

        let get = |token: &'static str| {
            tokens
                .get(token)
                .copied()
                .ok_or(VocabError::MissingSpecial { path: path.to_string(), token })
        };
        let unk_id = get(UNK)?;
        let cls_id = get(CLS)?;
        let sep_id = get(SEP)?;
        // Every literal `encode` may emit must exist, not just the three it wraps with.
        // `encode` unwraps these ids, and this is what makes that unwrap sound.
        for literal in SPECIAL_LITERALS {
            get(literal)?;
        }

        Ok(Self { tokens, unk_id, cls_id, sep_id })
    }

    pub fn len(&self) -> usize {
        self.tokens.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }

    pub fn id(&self, token: &str) -> Option<u32> {
        self.tokens.get(token).copied()
    }
}

// ---------------------------------------------------------------- character classification

/// BERT's `_is_whitespace`: the three ASCII forms it names explicitly, plus Unicode `Zs`.
///
/// Deliberately NOT `char::is_whitespace()`, which follows the `White_Space` property and so
/// also matches U+0085, U+2028 and U+2029. Those fall into the control branch in BERT and are
/// *dropped*, not folded to a space. The difference is invisible on ordinary English and is
/// exactly the kind of near-miss the fixture exists to catch — `score_longmemeval.py` already
/// records that LongMemEval transcripts contain U+2028/U+2029.
fn is_whitespace(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r')
        || get_general_category(c) == GeneralCategory::SpaceSeparator
}

/// BERT's `_is_control`: any `C*` category, with tab/newline/carriage return excluded so they
/// fall through to the whitespace branch.
///
/// **Control characters are DROPPED, not folded to a space** — so `ab\u{0085}cd` becomes the
/// single token `abcd`, not `ab` and `cd`. Verified against HuggingFace, and it is the opposite
/// of what "these are whitespace-ish" intuition suggests.
fn is_control(c: char) -> bool {
    if matches!(c, '\t' | '\n' | '\r') {
        return false;
    }
    matches!(
        get_general_category(c),
        GeneralCategory::Control
            | GeneralCategory::Format
            | GeneralCategory::PrivateUse
            | GeneralCategory::Surrogate
            | GeneralCategory::Unassigned
    )
}

/// What `whitespace_tokenize` splits on, which is **not** what `_is_whitespace` folds.
///
/// BERT cleans with `_is_whitespace` (space, `\t\n\r`, and `Zs`) and then calls Python's
/// `str.split()`, which additionally splits on `Zl` and `Zp`. Those two categories are neither
/// control nor `Zs`, so they survive cleaning as literal characters and are split on afterwards.
///
/// Getting this wrong is silent and this corpus provokes it: `tools/score_longmemeval.py`
/// records that LongMemEval transcripts contain U+2028 and U+2029. Treating them as ordinary
/// characters would weld two words into one token; treating every `char::is_whitespace()` as a
/// separator would wrongly split on U+0085, which BERT drops.
fn is_split_boundary(c: char) -> bool {
    c == ' '
        || matches!(
            get_general_category(c),
            GeneralCategory::LineSeparator | GeneralCategory::ParagraphSeparator
        )
}

/// BERT's `_is_punctuation`: the four ASCII spans it treats as punctuation regardless of
/// category, plus any Unicode `P*`.
///
/// The ASCII spans matter: `$`, `+`, `<`, `^`, `` ` `` and `|` are Symbol categories in Unicode,
/// and BERT splits on them anyway.
fn is_punctuation(c: char) -> bool {
    let cp = c as u32;
    if (33..=47).contains(&cp)
        || (58..=64).contains(&cp)
        || (91..=96).contains(&cp)
        || (123..=126).contains(&cp)
    {
        return true;
    }
    matches!(
        get_general_category(c),
        GeneralCategory::ConnectorPunctuation
            | GeneralCategory::DashPunctuation
            | GeneralCategory::OpenPunctuation
            | GeneralCategory::ClosePunctuation
            | GeneralCategory::InitialPunctuation
            | GeneralCategory::FinalPunctuation
            | GeneralCategory::OtherPunctuation
    )
}

/// BERT's `_is_chinese_char`. The ranges are copied from the reference implementation verbatim;
/// note it deliberately includes the CJK compatibility ideographs and excludes kana and hangul.
fn is_cjk(c: char) -> bool {
    let cp = c as u32;
    (0x4E00..=0x9FFF).contains(&cp)
        || (0x3400..=0x4DBF).contains(&cp)
        || (0x20000..=0x2A6DF).contains(&cp)
        || (0x2A700..=0x2B73F).contains(&cp)
        || (0x2B740..=0x2B81F).contains(&cp)
        || (0x2B820..=0x2CEAF).contains(&cp)
        || (0xF900..=0xFAFF).contains(&cp)
        || (0x2F800..=0x2FA1F).contains(&cp)
}

// ---------------------------------------------------------------- the pipeline

/// Step 1 and 2: clean, then space out CJK.
fn clean_and_space_cjk(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 16);
    for c in text.chars() {
        if c == '\0' || c == '\u{fffd}' {
            continue;
        }
        if is_whitespace(c) {
            out.push(' ');
            continue;
        }
        if is_control(c) {
            continue;
        }
        if is_cjk(c) {
            out.push(' ');
            out.push(c);
            out.push(' ');
        } else {
            out.push(c);
        }
    }
    out
}

/// Step 4: lowercase, then drop combining marks from the NFD form.
fn lower_and_strip_accents(token: &str) -> String {
    // Lowercase first, then decompose. BERT does exactly this order, and it matters: some
    // uppercase forms decompose differently than their lowercase counterparts.
    token
        .to_lowercase()
        .nfd()
        .filter(|c| get_general_category(*c) != GeneralCategory::NonspacingMark)
        .collect()
}

/// Step 5: split a token so each punctuation character stands alone.
fn split_on_punctuation(token: &str, out: &mut Vec<String>) {
    let mut current = String::new();
    for c in token.chars() {
        if is_punctuation(c) {
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
            out.push(c.to_string());
        } else {
            current.push(c);
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
}

/// Steps 1-5: text -> basic tokens, before WordPiece.
pub fn basic_tokenize(text: &str) -> Vec<String> {
    let cleaned = clean_and_space_cjk(text);
    let mut out = Vec::new();
    for word in cleaned.split(is_split_boundary) {
        if word.is_empty() {
            continue;
        }
        let folded = lower_and_strip_accents(word);
        if folded.is_empty() {
            continue;
        }
        split_on_punctuation(&folded, &mut out);
    }
    out
}

/// Step 6: greedy longest-match-first WordPiece over one basic token.
fn wordpiece(vocab: &Vocab, token: &str, out: &mut Vec<u32>) {
    let chars: Vec<char> = token.chars().collect();
    if chars.len() > MAX_INPUT_CHARS_PER_WORD {
        out.push(vocab.unk_id);
        return;
    }

    // Byte offsets for each char boundary, so the inner loop slices without re-walking.
    let mut offsets = Vec::with_capacity(chars.len() + 1);
    let mut byte = 0usize;
    offsets.push(0);
    for c in &chars {
        byte += c.len_utf8();
        offsets.push(byte);
    }

    let mut pieces = Vec::new();
    let mut start = 0usize;
    while start < chars.len() {
        let mut end = chars.len();
        let mut matched: Option<u32> = None;
        while start < end {
            let piece = &token[offsets[start]..offsets[end]];
            let candidate = if start == 0 {
                vocab.id(piece)
            } else {
                vocab.id(&format!("##{piece}"))
            };
            if let Some(id) = candidate {
                matched = Some(id);
                break;
            }
            end -= 1;
        }
        match matched {
            // Any unmatchable substring makes the WHOLE word [UNK] -- not the matched prefix
            // plus [UNK] for the rest. Getting this wrong produces ids that look reasonable.
            None => {
                out.push(vocab.unk_id);
                return;
            }
            Some(id) => {
                pieces.push(id);
                start = end;
            }
        }
    }
    out.extend(pieces);
}

/// One tokenized text, ready for the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Encoded {
    pub input_ids: Vec<u32>,
    /// All ones. Carried explicitly because the model takes it as an input and because the
    /// mean-pooling divides by its sum — a caller that assumed it could be omitted would get a
    /// silently wrong pooled vector rather than an error.
    pub attention_mask: Vec<u32>,
    /// Whether the text hit `max_seq_len` and lost its tail.
    pub truncated: bool,
}

/// A run of input text, split around special-token literals.
enum Segment<'a> {
    /// A literal like `[MASK]` that appeared in the text and maps straight to its id.
    Special(&'static str),
    Text(&'a str),
}

/// Split raw text around [`SPECIAL_LITERALS`], leftmost-first, longest-first at a tie.
///
/// Runs on the **raw** text, before cleaning, which is where HuggingFace does it. Cleaning
/// cannot affect the outcome — none of the literals contains a control or whitespace character
/// — but doing it in the same order removes the question.
fn split_on_special(text: &str) -> Vec<Segment<'_>> {
    let mut out = Vec::new();
    let mut cursor = 0usize;
    let bytes = text.as_bytes();

    let mut position = 0usize;
    while position < bytes.len() {
        // `[` is the only character any literal starts with, so this is the cheap gate.
        if bytes[position] == b'[' {
            let matched = SPECIAL_LITERALS
                .iter()
                .find(|literal| text[position..].starts_with(**literal));
            if let Some(literal) = matched {
                if position > cursor {
                    out.push(Segment::Text(&text[cursor..position]));
                }
                out.push(Segment::Special(literal));
                position += literal.len();
                cursor = position;
                continue;
            }
        }
        position += 1;
    }
    if cursor < text.len() {
        out.push(Segment::Text(&text[cursor..]));
    }
    out
}

/// Tokenize one text: `[CLS] pieces... [SEP]`, truncated to `max_seq_len` total.
///
/// Truncation reserves room for `[SEP]`, so the final token is always `[SEP]` and the model
/// never sees a sequence that simply stops. That matches HuggingFace's `truncation=True`.
pub fn encode(vocab: &Vocab, text: &str, max_seq_len: usize) -> Encoded {
    debug_assert!(max_seq_len >= 2, "no room for [CLS] and [SEP]");

    let mut pieces = Vec::new();
    for segment in split_on_special(text) {
        match segment {
            // Unwrap is safe: `Vocab::parse` refuses a vocabulary missing any of these, and
            // SPECIAL_LITERALS is exactly the set it checks.
            Segment::Special(literal) => pieces.push(
                vocab
                    .id(literal)
                    .expect("Vocab::parse guarantees every special literal is present"),
            ),
            Segment::Text(chunk) => {
                for token in basic_tokenize(chunk) {
                    wordpiece(vocab, &token, &mut pieces);
                }
            }
        }
    }

    let room = max_seq_len - 2;
    let truncated = pieces.len() > room;
    if truncated {
        pieces.truncate(room);
    }

    let mut input_ids = Vec::with_capacity(pieces.len() + 2);
    input_ids.push(vocab.cls_id);
    input_ids.extend(pieces);
    input_ids.push(vocab.sep_id);

    let attention_mask = vec![1u32; input_ids.len()];
    Encoded { input_ids, attention_mask, truncated }
}

/// One tokenized `(query, document)` pair, ready for a cross-encoder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedPair {
    pub input_ids: Vec<u32>,
    pub attention_mask: Vec<u32>,
    /// 0 for `[CLS] query [SEP]`, 1 for `document [SEP]`, 0 for padding.
    ///
    /// This is the input that carries the query/candidate boundary. A cross-encoder fed all-zero
    /// segment ids still returns a score, and the score is wrong in a way nothing downstream can
    /// see — which is why `rerank.rs` binds ONNX inputs by name rather than by position.
    pub token_type_ids: Vec<u32>,
    pub truncated: bool,
}

fn pieces_of(vocab: &Vocab, text: &str) -> Vec<u32> {
    let mut pieces = Vec::new();
    for segment in split_on_special(text) {
        match segment {
            Segment::Special(literal) => pieces.push(
                vocab
                    .id(literal)
                    .expect("Vocab::parse guarantees every special literal is present"),
            ),
            Segment::Text(chunk) => {
                for token in basic_tokenize(chunk) {
                    wordpiece(vocab, &token, &mut pieces);
                }
            }
        }
    }
    pieces
}

/// Encode `[CLS] query [SEP] document [SEP]`, padded to exactly `max_seq_len`.
///
/// **Truncation is HuggingFace's `longest_first`, which is its default for pairs** and is not the
/// obvious thing. It does not truncate the document to fit around the query; it repeatedly drops
/// one token from whichever sequence is currently longer. For a short query and a long document
/// the two agree, and for a long query they do not — so implementing the obvious rule would match
/// the reference on most inputs and diverge on exactly the ones where sequence length is doing
/// work. `tests/cross_encoder_reference.rs` compares against the real tokenizer id-for-id.
///
/// **Padding is to a fixed `max_seq_len`, always.** The graph is then a fixed `[1, max_seq_len]`
/// shape — the same shape Session G's re-costing measured, so its 92.41 ms figure stays the thing
/// being checked rather than a different measurement wearing its name.
pub fn encode_pair(vocab: &Vocab, query: &str, document: &str, max_seq_len: usize) -> EncodedPair {
    debug_assert!(max_seq_len >= 3, "no room for [CLS] and two [SEP]s");

    let mut a = pieces_of(vocab, query);
    let mut b = pieces_of(vocab, document);

    let room = max_seq_len - 3;
    let truncated = a.len() + b.len() > room;
    while a.len() + b.len() > room {
        // **On a tie the FIRST sequence loses the token**, hence `>=` rather than `>`.
        //
        // Measured against HuggingFace, not reasoned about: two equal 60-piece sequences into a
        // 61-piece budget come back as (30, 31), so the query is what shrinks when the two are
        // level. The first draft used `>` — the opposite — and produced (31, 30). Every other
        // fixture case still passed; only `long query AND long document` caught it, which is why
        // that case exists.
        if a.len() >= b.len() {
            a.pop();
        } else {
            b.pop();
        }
    }

    let mut input_ids = Vec::with_capacity(max_seq_len);
    let mut token_type_ids = Vec::with_capacity(max_seq_len);

    input_ids.push(vocab.cls_id);
    input_ids.extend(&a);
    input_ids.push(vocab.sep_id);
    token_type_ids.resize(input_ids.len(), 0);

    input_ids.extend(&b);
    input_ids.push(vocab.sep_id);
    token_type_ids.resize(input_ids.len(), 1);

    let mut attention_mask = vec![1u32; input_ids.len()];

    let pad_id = vocab.id(PAD).expect("Vocab::parse guarantees [PAD] is present");
    input_ids.resize(max_seq_len, pad_id);
    attention_mask.resize(max_seq_len, 0);
    token_type_ids.resize(max_seq_len, 0);

    EncodedPair { input_ids, attention_mask, token_type_ids, truncated }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_vocab() -> Vocab {
        // Not a real vocabulary -- `Vocab::parse` enforces the real size, so these unit tests
        // build the map directly. The reference test below is what checks the real one.
        let mut tokens = BTreeMap::new();
        for (i, t) in [
            "[PAD]", "[UNK]", "[CLS]", "[SEP]", "[MASK]", "the", "ingest", "job", "time",
            "##s", "##out", "out", "cafe", "run", "##ning", ",", "!", "2023",
        ]
        .iter()
        .enumerate()
        {
            tokens.insert((*t).to_string(), i as u32);
        }
        Vocab { tokens, unk_id: 1, cls_id: 2, sep_id: 3 }
    }

    #[test]
    fn basic_tokenize_lowercases_and_splits_punctuation() {
        assert_eq!(basic_tokenize("Hello, World!"), vec!["hello", ",", "world", "!"]);
        assert_eq!(basic_tokenize(""), Vec::<String>::new());
        assert_eq!(basic_tokenize("   "), Vec::<String>::new());
    }

    #[test]
    fn accents_are_stripped_because_strip_accents_null_means_derive_from_do_lower_case() {
        // The single most likely silent disagreement with the reference: `null` reads like
        // "off" and means "on, following do_lower_case".
        assert_eq!(basic_tokenize("café"), vec!["cafe"]);
        assert_eq!(basic_tokenize("CAFÉ"), vec!["cafe"]);
        assert_eq!(basic_tokenize("naïve Zürich"), vec!["naive", "zurich"]);
        // Precomposed and decomposed forms must land on the same token.
        assert_eq!(basic_tokenize("e\u{0301}"), basic_tokenize("\u{e9}"));
    }

    #[test]
    fn cjk_characters_each_become_their_own_token() {
        assert_eq!(basic_tokenize("你好世界"), vec!["你", "好", "世", "界"]);
        // Kana is deliberately NOT in BERT's CJK ranges and is not spaced out.
        assert_eq!(basic_tokenize("テスト"), vec!["テスト"]);
    }

    #[test]
    fn separators_split_but_control_characters_weld() {
        // Verified against HuggingFace directly, because the two cases look identical and
        // behave oppositely. LongMemEval transcripts contain U+2028/U+2029.
        assert_eq!(basic_tokenize("ab\u{2028}cd"), vec!["ab", "cd"], "Zl splits");
        assert_eq!(basic_tokenize("ab\u{2029}cd"), vec!["ab", "cd"], "Zp splits");
        assert_eq!(basic_tokenize("ab\u{00a0}cd"), vec!["ab", "cd"], "Zs folds to a space");
        assert_eq!(basic_tokenize("ab\u{0085}cd"), vec!["abcd"], "Cc is DROPPED, not split");
        assert_eq!(basic_tokenize("ab\u{200b}cd"), vec!["abcd"], "Cf is DROPPED, not split");
        assert_eq!(basic_tokenize("ab cd"), vec!["ab", "cd"]);
    }

    #[test]
    fn wordpiece_uses_continuations_and_falls_back_to_unk_for_the_whole_word() {
        let v = tiny_vocab();
        let mut out = Vec::new();
        wordpiece(&v, "times", &mut out);
        assert_eq!(out, vec![v.id("time").unwrap(), v.id("##s").unwrap()]);

        // "zzz" matches nothing: the WHOLE word is [UNK], not a matched prefix plus [UNK].
        let mut out = Vec::new();
        wordpiece(&v, "runzzz", &mut out);
        assert_eq!(out, vec![v.unk_id], "a partial match must not survive");
    }

    #[test]
    fn an_overlong_word_is_a_single_unk() {
        let v = tiny_vocab();
        let mut out = Vec::new();
        wordpiece(&v, &"a".repeat(MAX_INPUT_CHARS_PER_WORD + 1), &mut out);
        assert_eq!(out, vec![v.unk_id]);
    }

    #[test]
    fn encode_wraps_in_cls_and_sep() {
        let v = tiny_vocab();
        let e = encode(&v, "the ingest job", 128);
        assert_eq!(e.input_ids.first(), Some(&v.cls_id));
        assert_eq!(e.input_ids.last(), Some(&v.sep_id));
        assert_eq!(e.attention_mask.len(), e.input_ids.len());
        assert!(!e.truncated);
    }

    #[test]
    fn truncation_keeps_sep_last_and_says_so() {
        let v = tiny_vocab();
        let e = encode(&v, &"the ".repeat(50), 8);
        assert_eq!(e.input_ids.len(), 8);
        assert_eq!(e.input_ids.first(), Some(&v.cls_id));
        assert_eq!(e.input_ids.last(), Some(&v.sep_id), "never a sequence that just stops");
        assert!(e.truncated, "and the caller can see it happened");
    }

    #[test]
    fn an_empty_text_is_just_the_special_tokens() {
        let v = tiny_vocab();
        let e = encode(&v, "", 128);
        assert_eq!(e.input_ids, vec![v.cls_id, v.sep_id]);
        assert!(!e.truncated);
    }

    #[test]
    fn special_token_literals_in_the_text_are_split_out_case_sensitively() {
        // Verified against HuggingFace, and every property here is easy to guess wrong.
        // It matters because the text is USER CONTENT: a memory that happens to contain
        // "[SEP]" must tokenize the way the reference does, not the way plain WordPiece would.
        let v = tiny_vocab();
        let ids = |t: &str| encode(&v, t, 128).input_ids;

        assert_eq!(ids("[CLS]"), vec![v.cls_id, v.cls_id, v.sep_id], "matched as a literal");
        // Lowercase must NOT match: the added token is declared `normalized: false`.
        assert_ne!(ids("[cls]"), ids("[CLS]"));
        // Matches mid-word, with no boundary required.
        let mask = v.id("[MASK]").unwrap();
        assert_eq!(ids("run[MASK]run"), vec![v.cls_id, v.id("run").unwrap(), mask, v.id("run").unwrap(), v.sep_id]);
        // Adjacent literals both match.
        assert_eq!(ids("[MASK][MASK]"), vec![v.cls_id, mask, mask, v.sep_id]);
        // A longer word that merely starts the same way does not match.
        assert!(!ids("[MASKED]").contains(&mask));
    }

    #[test]
    fn tokenizing_is_deterministic() {
        let v = tiny_vocab();
        let text = "The ingest job times out, running 2023!";
        assert_eq!(encode(&v, text, 128), encode(&v, text, 128));
    }
}
