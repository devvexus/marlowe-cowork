//! Which MS file generations an ms-llama.cpp version runs (see `marlowe_hw::ms` for the
//! generations). Gen 1 is every file published today and ms-v1.x runs it; a newer ms-llama.cpp
//! rejects Gen 1 (and ships a header-only migration tool) and runs Gen 2. A tag this does not
//! recognise (a prerelease build, a developer's own) is not second-guessed: it may run either.

/// The generations the ms-llama.cpp release `tag` runs (1 and/or 2).
pub fn generations(tag: &str) -> Vec<u8> {
    let major = tag.trim().strip_prefix("ms-").unwrap_or(tag).trim_start_matches(['v', 'V']).split('.').next().and_then(|m| m.parse::<u32>().ok());
    match major {
        Some(1) => vec![1],
        Some(m) if m >= 2 => vec![2],
        _ => vec![1, 2],
    }
}

/// Whether ms-llama.cpp `tag` runs MS files of `generation`.
pub fn runs(tag: &str, generation: u8) -> bool {
    generations(tag).contains(&generation)
}

/// The generations in `have` that `tag` would stop running: what an upgrade to it costs.
pub fn would_stop(tag: &str, have: &[u8]) -> Vec<u8> {
    let mut lost: Vec<u8> = have.iter().copied().filter(|g| !runs(tag, *g)).collect();
    lost.sort_unstable();
    lost.dedup();
    lost
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v1_runs_gen_1_and_a_later_major_runs_gen_2() {
        assert_eq!(generations("ms-v1.0"), vec![1]);
        assert_eq!(generations("ms-v1.3"), vec![1]);
        assert_eq!(generations("ms-v2.0"), vec![2]);
        assert!(runs("ms-v1.0", 1) && !runs("ms-v1.0", 2));
        assert!(!runs("ms-v2.0", 1) && runs("ms-v2.0", 2));
    }

    #[test]
    fn a_tag_that_is_not_a_release_is_not_second_guessed() {
        assert_eq!(generations("b1234"), vec![1, 2]);
        assert_eq!(generations("ms-b17"), vec![1, 2]);
        assert_eq!(would_stop("b1234", &[1, 2]), Vec::<u8>::new());
    }

    #[test]
    fn an_upgrade_names_the_generations_it_would_stop() {
        assert_eq!(would_stop("ms-v2.0", &[1, 2, 1]), vec![1]);
        assert_eq!(would_stop("ms-v1.1", &[1]), Vec::<u8>::new());
    }
}
