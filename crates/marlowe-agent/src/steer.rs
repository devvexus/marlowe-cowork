//! The inbox: user text that arrives while a turn is running.

use marlowe_model::Message;

/// Longest accepted steer, in characters. Longer steers are refused, never truncated.
pub const MAX_STEER_CHARS: usize = 2_000;

/// A user message queued into a running turn; applied at the next step boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Steer {
    pub text: String,
}

/// Why a steer was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SteerRefused {
    #[error("an empty steer is not a correction")]
    Empty,
    #[error("that steer is {len} characters and the limit is {max}; shorten it and send it again")]
    TooLong { len: usize, max: usize },
}

impl Steer {
    /// Validates user text as a steer: trimmed, non-empty, at most [`MAX_STEER_CHARS`] chars.
    pub fn admit(text: &str) -> Result<Steer, SteerRefused> {
        let t = text.trim();
        if t.is_empty() {
            return Err(SteerRefused::Empty);
        }
        let len = t.chars().count();
        if len > MAX_STEER_CHARS {
            return Err(SteerRefused::TooLong { len, max: MAX_STEER_CHARS });
        }
        Ok(Steer { text: t.to_string() })
    }

    /// The user message this steer becomes in the conversation.
    pub fn to_message(&self) -> Message {
        Message::user(self.text.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steers_are_trimmed_and_bounded() {
        assert_eq!(Steer::admit("  stop  ").unwrap().text, "stop");
        assert_eq!(Steer::admit("   "), Err(SteerRefused::Empty));
        let long = "é".repeat(MAX_STEER_CHARS);
        assert!(Steer::admit(&long).is_ok(), "counted in chars, not bytes");
        assert!(matches!(Steer::admit(&format!("{long}x")), Err(SteerRefused::TooLong { .. })));
    }
}
