//! Subjects: a claim about one thing never lands under a question about another.
//!
//! An objective names what it is about ("new Kimi models, DeepSeek and Qwen4"); each question
//! usually names one of them. Found live (run m6): a DeepSeek "V3.2" fact was read off a DeepSeek
//! page for the Qwen4 question, condensed together with Qwen4 claims, and the report called the
//! model "V3.2 (Qwen 4)". This module is the code-only rule that keeps subjects apart: a subject
//! is a family stem of a name the objective carries (Qwen4 -> qwen, so Qwen3.8 counts), a text
//! names it when one of its words starts with the stem, and a question's scope is the subjects
//! it names. No model is asked and no name is written in code.

use std::collections::BTreeSet;

/// The subject families of an objective.
#[derive(Debug, Clone, Default)]
pub struct Subjects {
    families: Vec<String>,
}

/// The families a page is about (its title and address) and the ones its text names.
#[derive(Debug, Clone, Default)]
pub struct PageSubjects {
    about: BTreeSet<usize>,
    text: BTreeSet<usize>,
}

fn words(text: &str) -> Vec<String> {
    text.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_string).collect()
}

/// The stem of a name: lowercase letters and digits with trailing digits cut (Qwen4 -> qwen).
fn family(name: &str) -> Option<String> {
    let flat: String = name.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect();
    let stem = flat.trim_end_matches(|c: char| c.is_ascii_digit());
    let stem = if stem.chars().count() >= 3 { stem } else { flat.as_str() };
    (stem.chars().filter(|c| c.is_alphabetic()).count() >= 3).then(|| stem.to_string())
}

/// The names an objective carries: a word with a capital letter that does not just start a
/// sentence (one that starts a sentence counts only with a digit or a capital inside it, like
/// DeepSeek or Qwen4), or a word with a digit. A possessive is the name (Kimi's -> Kimi).
fn names_in(objective: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut starts = true;
    for raw in objective.split(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | '(' | ')' | '"')).filter(|w| !w.is_empty()) {
        let initial = std::mem::replace(&mut starts, false);
        let w = raw.trim_matches(|c: char| !c.is_alphanumeric());
        // A full stop after a short word or one with a dot inside (vs. e.g.) ends no sentence.
        starts = raw.ends_with(['.', '!', '?', ':']) && w.chars().count() >= 3 && !w.contains('.');
        let w = w.strip_suffix("'s").or_else(|| w.strip_suffix("’s")).unwrap_or(w);
        let has_digit = w.chars().any(|c| c.is_ascii_digit()) && w.chars().any(char::is_alphabetic);
        let inner_capital = w.chars().skip(1).any(char::is_uppercase);
        let capital = if initial { inner_capital } else { w.chars().any(char::is_uppercase) };
        if (capital || has_digit) && w.chars().count() >= 2 && !out.iter().any(|o| o.eq_ignore_ascii_case(w)) {
            out.push(w.to_string());
        }
    }
    out
}

impl Subjects {
    /// The families of the names an objective carries. Fewer than two subjects cannot cross, so
    /// then there are none.
    pub fn of(objective: &str) -> Self {
        let mut families: Vec<String> = Vec::new();
        for n in names_in(objective) {
            if let Some(f) = family(&n) {
                if !families.contains(&f) {
                    families.push(f);
                }
            }
        }
        if families.len() < 2 {
            families.clear();
        }
        Self { families }
    }

    /// Whether there is anything to keep apart.
    pub fn is_empty(&self) -> bool {
        self.families.is_empty()
    }

    /// The families `text` names, as indexes.
    pub fn named(&self, text: &str) -> BTreeSet<usize> {
        if self.families.is_empty() {
            return BTreeSet::new();
        }
        let ws = words(text);
        let hit = |f: &String| ws.iter().any(|w| if f.chars().count() >= 4 { w.starts_with(f.as_str()) } else { w == f });
        self.families.iter().enumerate().filter(|(_, f)| hit(f)).map(|(i, _)| i).collect()
    }

    /// The families a question is about; empty when it names none (then nothing is held apart).
    pub fn scope(&self, question: &str) -> BTreeSet<usize> {
        self.named(question)
    }

    /// What a page is about: its title and address, and what its text names.
    pub fn page(&self, title: &str, url: &str, text: &str) -> PageSubjects {
        PageSubjects { about: self.named(&format!("{title} {url}")), text: self.named(text) }
    }

    /// The families a claim stands for: the ones its own words name, else the page's.
    pub fn of_claim(&self, claim: &str, quote: &str, page: &PageSubjects) -> BTreeSet<usize> {
        let own = self.named(&format!("{claim} {quote}"));
        if own.is_empty() {
            page.subjects()
        } else {
            own
        }
    }

    /// Whether a claim belongs under a question of `scope`. A claim that names a subject of the
    /// scope does; one that names only other subjects does when its page is about the scope; one
    /// that names nothing does when its page fits.
    pub fn claim_fits(&self, scope: &BTreeSet<usize>, claim: &str, quote: &str, page: &PageSubjects) -> bool {
        if scope.is_empty() {
            return true;
        }
        let own = self.named(&format!("{claim} {quote}"));
        if own.iter().any(|i| scope.contains(i)) {
            return true;
        }
        if own.is_empty() {
            page.fits(scope)
        } else {
            page.about.iter().any(|i| scope.contains(i))
        }
    }

    /// Whether a text that names subjects (a condensed claim, a section's sentence) belongs under
    /// `scope`: it names one of them, or none at all, or a page it stands on is about one (a claim
    /// kept on its page's subject may name only another subject, as a comparison does).
    pub fn text_fits(&self, scope: &BTreeSet<usize>, text: &str, pages: &[PageSubjects]) -> bool {
        let own = self.named(text);
        scope.is_empty() || own.is_empty() || own.iter().any(|i| scope.contains(i)) || pages.iter().any(|p| p.about.iter().any(|i| scope.contains(i)))
    }
}

impl PageSubjects {
    /// What the page stands for: its title and address when they name a subject, else its text.
    pub fn subjects(&self) -> BTreeSet<usize> {
        if self.about.is_empty() {
            self.text.clone()
        } else {
            self.about.clone()
        }
    }

    /// Whether the page is about something in `scope`.
    pub fn fits(&self, scope: &BTreeSet<usize>) -> bool {
        self.subjects().iter().any(|i| scope.contains(i))
    }
}

/// Splits claim indexes into the groups that may be merged: claims that share a subject (directly
/// or through another claim of the group) stay together; claims naming no subject go with the
/// first group. `sets[i]` is the subjects of claim `i`. Groups come in order of their first claim.
pub fn split_groups(sets: &[BTreeSet<usize>]) -> Vec<Vec<usize>> {
    let n = sets.len();
    let mut parent: Vec<usize> = (0..n).collect();
    fn find(p: &mut [usize], mut i: usize) -> usize {
        while p[i] != i {
            p[i] = p[p[i]];
            i = p[i];
        }
        i
    }
    for i in 0..n {
        for j in (i + 1)..n {
            if !sets[i].is_disjoint(&sets[j]) {
                let (a, b) = (find(&mut parent, i), find(&mut parent, j));
                if a != b {
                    parent[a.max(b)] = a.min(b);
                }
            }
        }
    }
    let mut groups: Vec<(usize, Vec<usize>)> = Vec::new();
    let mut loose: Vec<usize> = Vec::new();
    for (i, set) in sets.iter().enumerate() {
        if set.is_empty() {
            loose.push(i);
            continue;
        }
        let r = find(&mut parent, i);
        match groups.iter_mut().find(|g| g.0 == r) {
            Some(g) => g.1.push(i),
            None => groups.push((r, vec![i])),
        }
    }
    let mut out: Vec<Vec<usize>> = groups.into_iter().map(|g| g.1).collect();
    match out.first_mut() {
        Some(first) => {
            first.extend(loose);
            first.sort_unstable();
        }
        None if !loose.is_empty() => out.push(loose),
        None => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const OBJECTIVE: &str = "Compile a comprehensive report on latest upcoming AI models, specifically new Kimi models, DeepSeek and Qwen4";

    fn set(v: &[usize]) -> BTreeSet<usize> {
        v.iter().copied().collect()
    }

    #[test]
    fn the_objectives_names_become_families_and_noise_words_do_not() {
        let s = Subjects::of(OBJECTIVE);
        assert_eq!(s.families, ["kimi", "deepseek", "qwen"], "AI is too short to be a subject; Qwen4 is the Qwen family");
        assert!(Subjects::of("Find a good Rust crate").is_empty(), "one subject has nothing to cross");
        assert!(Subjects::of("what is new").is_empty());
    }

    #[test]
    fn a_word_that_starts_a_sentence_and_a_possessive_are_not_new_subjects() {
        let s = Subjects::of("Compile a report on Mamba and RWKV. Include benchmarks. Also Kimi's results.");
        assert_eq!(s.families, ["mamba", "rwkv", "kimi"], "Include and Also only start sentences; Kimi's is Kimi");
        assert_eq!(s.named("The architecture includes token shift"), set(&[]));
        assert_eq!(Subjects::of("Compare DeepSeek vs. Qwen on code: Tokens per second.").families, ["deepseek", "qwen"], "vs. ends no sentence; Tokens starts one");
        assert_eq!(Subjects::of("Qwen4 and DeepSeek are new. Kimi too.").families, ["qwen", "deepseek"]);
    }

    #[test]
    fn a_text_names_a_family_by_its_words_not_by_letters_inside_words() {
        let s = Subjects::of(OBJECTIVE);
        assert_eq!(s.named("Qwen3.8-Max and Qwen 4"), set(&[2]));
        assert_eq!(s.named("DeepSeek-V3.2 vs Kimi-K2"), set(&[0, 1]));
        assert_eq!(s.named("a kimono and a qwerty keyboard"), set(&[]));
        assert_eq!(s.named("https://api-docs.deepseek.com/news/news251201/"), set(&[1]));
    }

    #[test]
    fn a_claim_about_another_subject_is_not_kept_for_the_question() {
        let s = Subjects::of(OBJECTIVE);
        let scope = s.scope("What are the confirmed public benchmarks for Qwen3 vs Qwen4 tiers?");
        assert_eq!(scope, set(&[2]));
        let deepseek_page = s.page("DeepSeek-V3.2 release", "https://api-docs.deepseek.com/news/news251201/", "V3.2 beats GPT-5 on reasoning.");
        let qwen_page = s.page("Qwen 4 release date", "https://techpilot.ai/qwen-4-release-date/", "");
        // The m6 leak: unnamed DeepSeek facts under the Qwen4 question.
        assert!(!s.claim_fits(&scope, "V3.2-Speciale attains gold-level results.", "V3.2-Speciale attains gold-level results in IMO.", &deepseek_page));
        assert!(!s.claim_fits(&scope, "Models are now released under the MIT License, just like DeepSeek-R1!", "", &deepseek_page));
        // Its own subject, named or inherited from its page, stays.
        assert!(s.claim_fits(&scope, "Alibaba said Qwen 4 is in training", "", &deepseek_page), "it names the subject itself");
        assert!(s.claim_fits(&scope, "It has not announced a release date.", "", &qwen_page));
        // A comparison on the subject's own page stays even when it names another model.
        assert!(s.claim_fits(&scope, "Second on CorpFin, behind Kimi K2.5", "", &qwen_page));
        // A question that names no subject holds nothing apart.
        assert!(s.claim_fits(&s.scope("Are there any official benchmarks?"), "V3.2 beats GPT-5", "", &deepseek_page));
    }

    #[test]
    fn a_page_without_a_title_subject_fits_by_its_text() {
        let s = Subjects::of(OBJECTIVE);
        let scope = s.scope("Which Kimi versions are in beta?");
        assert!(s.claim_fits(&scope, "It is in beta.", "", &s.page("Release notes", "https://example.org/notes", "Kimi K3 is in beta")));
        assert!(!s.claim_fits(&scope, "It is in beta.", "", &s.page("Release notes", "https://example.org/notes", "DeepSeek V4 is in beta")));
    }

    #[test]
    fn claims_of_different_subjects_never_share_a_group() {
        // kimi, deepseek+qwen, qwen, (none), deepseek
        let sets = vec![set(&[0]), set(&[1, 2]), set(&[2]), set(&[]), set(&[1])];
        assert_eq!(split_groups(&sets), vec![vec![0, 3], vec![1, 2, 4]]);
        assert_eq!(split_groups(&[set(&[]), set(&[])]), vec![vec![0, 1]]);
        assert!(split_groups(&[]).is_empty());
    }
}
