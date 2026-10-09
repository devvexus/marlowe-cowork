//! Which installed voice speaks.

use crate::{VoiceGender, VoiceInfo};

/// `preferred` (matched case-insensitively against name, display or id) when installed; else an
/// en-GB male voice (George first); else any en-GB voice; else the first (the system default).
pub fn pick_voice<'a>(voices: &'a [VoiceInfo], preferred: Option<&str>) -> Option<&'a VoiceInfo> {
    if let Some(p) = preferred.map(str::trim).filter(|p| !p.is_empty()) {
        if let Some(v) = find(voices, p) {
            return Some(v);
        }
    }
    let gb = |v: &&VoiceInfo| v.language.eq_ignore_ascii_case("en-GB");
    voices
        .iter()
        .filter(gb)
        .find(|v| v.gender == VoiceGender::Male && v.name.eq_ignore_ascii_case("George"))
        .or_else(|| voices.iter().filter(gb).find(|v| v.gender == VoiceGender::Male))
        .or_else(|| voices.iter().find(gb))
        .or_else(|| voices.first())
}

/// The installed voice `name` names (name, display or id, any case), if any.
pub(crate) fn find<'a>(voices: &'a [VoiceInfo], name: &str) -> Option<&'a VoiceInfo> {
    let name = name.trim();
    voices.iter().find(|v| {
        v.name.eq_ignore_ascii_case(name) || v.display().eq_ignore_ascii_case(name) || v.id.eq_ignore_ascii_case(name)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(name: &str, lang: &str, gender: VoiceGender) -> VoiceInfo {
        VoiceInfo { id: format!("HKLM\\Voices\\{name}"), name: name.into(), language: lang.into(), gender }
    }

    fn installed() -> Vec<VoiceInfo> {
        vec![
            v("David", "en-US", VoiceGender::Male),
            v("Hazel", "en-GB", VoiceGender::Female),
            v("Ryan", "en-GB", VoiceGender::Male),
            v("George", "en-GB", VoiceGender::Male),
            v("Zira", "en-US", VoiceGender::Female),
        ]
    }

    fn pick(voices: &[VoiceInfo], pref: Option<&str>) -> Option<String> {
        pick_voice(voices, pref).map(|v| v.name.clone())
    }

    #[test]
    fn george_wins_among_en_gb_males() {
        assert_eq!(pick(&installed(), None).as_deref(), Some("George"));
    }

    #[test]
    fn any_en_gb_male_before_an_en_gb_female() {
        let mut vs = installed();
        vs.retain(|v| v.name != "George");
        assert_eq!(pick(&vs, None).as_deref(), Some("Ryan"));
    }

    #[test]
    fn en_gb_female_before_the_default() {
        let vs = vec![v("David", "en-US", VoiceGender::Male), v("Hazel", "en-gb", VoiceGender::Female)];
        assert_eq!(pick(&vs, None).as_deref(), Some("Hazel"));
    }

    #[test]
    fn falls_back_to_the_first_voice_then_none() {
        let vs = vec![v("Zira", "en-US", VoiceGender::Female), v("David", "en-US", VoiceGender::Male)];
        assert_eq!(pick(&vs, None).as_deref(), Some("Zira"));
        assert_eq!(pick(&[], None), None);
        assert_eq!(pick(&[], Some("George")), None);
    }

    #[test]
    fn preferred_matches_name_display_or_id_in_any_case() {
        let vs = installed();
        assert_eq!(pick(&vs, Some("zira")).as_deref(), Some("Zira"));
        assert_eq!(pick(&vs, Some("Hazel (en-GB)")).as_deref(), Some("Hazel"));
        assert_eq!(pick(&vs, Some("hklm\\voices\\david")).as_deref(), Some("David"));
        assert_eq!(pick(&vs, Some("  David ")).as_deref(), Some("David"));
    }

    #[test]
    fn unknown_or_blank_preference_falls_back() {
        let vs = installed();
        assert_eq!(pick(&vs, Some("Brian")).as_deref(), Some("George"));
        assert_eq!(pick(&vs, Some("")).as_deref(), Some("George"));
    }
}
