//! §B9's approval overlay, as **data a producer computes** rather than prose anyone formats.
//!
//! # M1's overlay was scaffolding, and the record should say so
//!
//! `BlastRadius` carried three free `String` fields — `headline`, `consequence`, `why`. The
//! *renderer* was already clean: `overlay.rs` reads those fields and composes nothing, and the type
//! has no field for the command, so it could not print one even by accident. That much was real.
//!
//! What was scaffolding is that **nothing could compute those strings.** The stub hand-wrote them
//! as a fixture; the daemon has no path to them at all. So the shape was carrying M1's demo rather
//! than a contract, and the first producer to need one would have invented prose at the single
//! highest-consequence moment in the product — the instant a user decides whether to trust an
//! irreversible action.
//!
//! # Why a surface can never compute this
//!
//! `Delete 1,204 files in ./build · not recoverable` requires a filesystem walk against the
//! permission layer. A surface computing it would be **inventing a claim about a filesystem it does
//! not own**, and presenting that claim as the basis for consent. The same is true of the other two
//! lines and it is the reason each is data here:
//!
//! * **Novelty** — *"unusual · first send to this recipient"* — is a judgment about **history**.
//!   Only something holding the interaction record can make it. Omitting it now would mean
//!   re-litigating this at M6 when the trust ledger lands, so it is [`Novelty`] and it is required.
//! * **The ceiling** — *"this class sits at its ceiling and cannot be promoted"* — is the **trust
//!   ledger's** to state. A surface asserting it would be guessing about promotion logic that
//!   §13 puts out of reach entirely.
//!
//! # `Option` is not used for either, deliberately
//!
//! An `Option<Novelty>` lets a producer omit the line and nothing reports the omission — the
//! permissive-default shape this project has deleted repeatedly. [`Novelty::Routine`] is a value
//! meaning *nothing unusual*, which is a claim a producer has to make on purpose.
//!
//! # Growth rule
//!
//! [`Effect`] grows only when a producer must describe a consequence the set cannot express —
//! never to carry a string a caller already has. ADR-030 §5, applied to this enum as well.

use crate::notice::Echo;
use crate::model::Tone;

/// A path the permission layer resolved. **Data, not prose** — never composed at a call site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathLabel(pub String);

impl PathLabel {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
}

impl std::fmt::Display for PathLabel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// §B9 is risk-tiered per v1.0 §8.2. The tier picks the doubled border's colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RiskTier {
    /// Routine writes, batched.
    Routine,
    /// Irreversible. Blocking.
    Irreversible,
}

impl RiskTier {
    pub fn tone(self) -> Tone {
        match self {
            RiskTier::Routine => Tone::Amber,
            RiskTier::Irreversible => Tone::Red,
        }
    }
}

/// What will actually happen, in the user's terms. **Never the command.**
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    Delete { files: u32, within: PathLabel, recoverable: bool },
    Write { files: u32, added: u32, removed: u32, within: PathLabel },
    Send { medium: Medium, recipient: Echo, impersonating: bool },
    /// A process. Described by what it can reach, not by its argv — §B9's whole point.
    Execute { touches: u32, within: PathLabel, reaches_network: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Medium {
    Email,
    Message,
    Web,
}

impl Medium {
    pub fn name(self) -> &'static str {
        match self {
            Medium::Email => "email",
            Medium::Message => "message",
            Medium::Web => "request",
        }
    }
}

/// Why this is being asked **now**. A judgment about history, so only a producer can make it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Novelty {
    /// Nothing unusual; the tier alone is why this is being asked. A stated claim, not an absence.
    Routine,
    FirstTime(FirstTime),
    Unusual(Deviation),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FirstTime {
    Recipient,
    Host,
    Path,
    ToolClass,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deviation {
    FarLargerThanUsual,
    OutsideUsualHours,
    LongDormantContact,
}

/// Whether this class can still be promoted. **The trust ledger's statement, never the surface's.**
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ceiling {
    /// Promotion is still possible, and how far off it is.
    Promotable { agreements: u32, needed: u32 },
    /// §B9: *"this class sits at its ceiling and cannot be promoted."* Addendum A §A8 — some
    /// classes never become automatic however often the user agrees.
    AtCeiling,
}

/// Which of §B9's four keys the producer is offering.
///
/// Accept and deny are always offered — an overlay with no answer is not a question. The other two
/// are the producer's to offer **or withhold**, and `send_as_marlowe` especially: it is Addendum A
/// §A3's delegation escape hatch, the path that avoids impersonation entirely, and offering it
/// when nothing can perform a delegated send would be a key that does nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Offered {
    pub edit_first: bool,
    pub send_as_marlowe: bool,
}

/// §B9's overlay content. **States blast radius, not the command.**
///
/// There is still no field for the command string, which was right in M1 and stays right: a
/// surface cannot show what it was never given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlastRadius {
    pub effect: Effect,
    pub tier: RiskTier,
    pub novelty: Novelty,
    pub ceiling: Ceiling,
    pub offered: Offered,
}

impl BlastRadius {
    /// What will happen, in the user's terms. First line of the overlay.
    pub fn headline(&self) -> String {
        match &self.effect {
            Effect::Delete { files, within, .. } => {
                format!("Delete {files} file{} in {within}", plural(*files))
            }
            Effect::Write { files, added, removed, within } => format!(
                "Write {files} file{} in {within} · +{added} \u{2212}{removed}",
                plural(*files)
            ),
            Effect::Send { medium, recipient, impersonating } => format!(
                "Send {} to {recipient}{}",
                medium.name(),
                if *impersonating { " as you" } else { " as Marlowe" }
            ),
            Effect::Execute { touches, within, .. } => {
                format!("Run a process over {touches} file{} in {within}", plural(*touches))
            }
        }
    }

    /// The consequence, in the risk tier's colour. Second line.
    pub fn consequence(&self) -> String {
        match &self.effect {
            Effect::Delete { recoverable: false, .. } => "not recoverable".into(),
            Effect::Delete { recoverable: true, .. } => "recoverable from the journal".into(),
            Effect::Write { .. } => "reversible with ^u".into(),
            Effect::Send { impersonating: true, .. } => {
                "not recoverable · it goes out under your name".into()
            }
            Effect::Send { impersonating: false, .. } => "not recoverable".into(),
            Effect::Execute { reaches_network: true, .. } => {
                "can reach the network · not recoverable".into()
            }
            Effect::Execute { reaches_network: false, .. } => "not recoverable".into(),
        }
    }

    /// §B9: novelty gating explained in **one line**, with the ceiling stated where it applies.
    pub fn why(&self) -> String {
        let novelty = match &self.novelty {
            Novelty::Routine => "routine for this class".to_string(),
            Novelty::FirstTime(f) => format!(
                "unusual · first {} ",
                match f {
                    FirstTime::Recipient => "send to this recipient",
                    FirstTime::Host => "request to this host",
                    FirstTime::Path => "write under this path",
                    FirstTime::ToolClass => "use of this tool",
                }
            )
            .trim_end()
            .to_string(),
            Novelty::Unusual(d) => format!(
                "unusual · {}",
                match d {
                    Deviation::FarLargerThanUsual => "far larger than your usual",
                    Deviation::OutsideUsualHours => "outside your usual hours",
                    Deviation::LongDormantContact => "no contact in months",
                }
            ),
        };
        match self.ceiling {
            Ceiling::AtCeiling => {
                format!("{novelty} · this class sits at its ceiling and cannot be promoted")
            }
            Ceiling::Promotable { agreements, needed } => {
                format!("{novelty} · {agreements} of {needed} agreements toward promotion")
            }
        }
    }

    /// §B9's keys, in its order: `↵ send · e edit first · s send as marlowe · esc deny`.
    ///
    /// The middle two are present only when offered. **The delegation hatch is a producer's to
    /// withhold**, so this is derived rather than a fixed row.
    pub fn keys(&self) -> Vec<(char, &'static str)> {
        let mut out = vec![('\n', self.accept_label())];
        if self.offered.edit_first {
            out.push(('e', "edit first"));
        }
        if self.offered.send_as_marlowe {
            out.push(('s', "send as marlowe"));
        }
        out.push(('\u{1b}', "deny"));
        out
    }

    fn accept_label(&self) -> &'static str {
        match self.effect {
            Effect::Delete { .. } => "delete",
            Effect::Write { .. } => "write",
            Effect::Send { .. } => "send",
            Effect::Execute { .. } => "run",
        }
    }
}

fn plural(n: u32) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn send(impersonating: bool, offered: Offered) -> BlastRadius {
        BlastRadius {
            effect: Effect::Send {
                medium: Medium::Email,
                recipient: Echo::new("procurement@acme.com"),
                impersonating,
            },
            tier: RiskTier::Irreversible,
            novelty: Novelty::FirstTime(FirstTime::Recipient),
            ceiling: Ceiling::AtCeiling,
            offered,
        }
    }

    #[test]
    fn the_overlay_states_blast_radius_and_has_no_way_to_state_a_command() {
        // §B9's central rule, kept structural: the type has no command field, so a renderer cannot
        // print one however much it wants to.
        let r = send(true, Offered { edit_first: true, send_as_marlowe: true });
        let all = format!("{} {} {}", r.headline(), r.consequence(), r.why());
        assert!(r.headline().contains("procurement@acme.com"));
        assert!(!all.contains("--"), "an argv fragment reached the overlay: {all}");
        let debug = format!("{r:?}");
        for forbidden in ["command", "argv", "shell"] {
            assert!(!debug.to_lowercase().contains(forbidden), "{debug}");
        }
    }

    #[test]
    fn the_novelty_reason_and_the_ceiling_both_reach_the_one_line() {
        // Both are producer judgments the surface cannot make: novelty is about history, the
        // ceiling is the trust ledger's. §B9 requires both to be explained.
        let why = send(true, Offered { edit_first: true, send_as_marlowe: true }).why();
        assert!(why.contains("first send to this recipient"), "{why}");
        assert!(why.contains("sits at its ceiling"), "{why}");
    }

    #[test]
    fn a_promotable_class_states_how_far_off_promotion_is() {
        let mut r = send(true, Offered { edit_first: true, send_as_marlowe: true });
        r.ceiling = Ceiling::Promotable { agreements: 2, needed: 5 };
        let why = r.why();
        assert!(why.contains("2 of 5 agreements"), "{why}");
        assert!(!why.contains("ceiling"), "a promotable class must not claim a ceiling: {why}");
    }

    #[test]
    fn the_delegation_hatch_can_be_withheld_and_the_answer_keys_never_can() {
        // Addendum A §A3: send-as-Marlowe avoids impersonation entirely, so §B9 offers it — but
        // only a producer that can actually perform a delegated send may offer it. A key that
        // does nothing is worse than an absent one.
        let with = send(true, Offered { edit_first: true, send_as_marlowe: true });
        let keys: Vec<char> = with.keys().iter().map(|(k, _)| *k).collect();
        assert_eq!(keys, vec!['\n', 'e', 's', '\u{1b}']);

        let without = send(true, Offered { edit_first: false, send_as_marlowe: false });
        let keys: Vec<char> = without.keys().iter().map(|(k, _)| *k).collect();
        assert_eq!(keys, vec!['\n', '\u{1b}'], "accept and deny are never withheld");
    }

    #[test]
    fn the_accept_label_follows_the_effect_rather_than_always_saying_send() {
        let d = BlastRadius {
            effect: Effect::Delete {
                files: 1_204,
                within: PathLabel::new("./build"),
                recoverable: false,
            },
            tier: RiskTier::Irreversible,
            novelty: Novelty::Unusual(Deviation::FarLargerThanUsual),
            ceiling: Ceiling::AtCeiling,
            offered: Offered { edit_first: false, send_as_marlowe: false },
        };
        assert_eq!(d.headline(), "Delete 1204 files in ./build");
        assert_eq!(d.consequence(), "not recoverable");
        assert_eq!(d.keys()[0].1, "delete");
    }

    #[test]
    fn routine_is_a_stated_claim_rather_than_an_absent_one() {
        // There is no `Option<Novelty>`. A producer saying "nothing unusual" has made a judgment;
        // a producer omitting the field would have made none, and nothing would have reported it.
        let mut r = send(false, Offered { edit_first: false, send_as_marlowe: false });
        r.novelty = Novelty::Routine;
        assert!(r.why().starts_with("routine for this class"), "{}", r.why());
    }
}

/// **The live approval, which is deliberately NOT [`BlastRadius`].**
///
/// §B9's overlay is the target and this is not it. The first tool that ever actually needed an
/// approval — `web`, fetching a URL — cannot be expressed as one, and finding that out is what
/// this type records:
///
/// 1. **[`Effect`] has no fetch variant.** `Delete`, `Write`, `Send` and `Execute`. `Send` is
///    about sending *to* somebody and `Execute` is about a process; neither describes "retrieve
///    this URL". The enum was written before anything could reach the network.
/// 2. **[`Ceiling`] has no producer.** It comes from the trust ledger, which is M6. Both of its
///    variants are claims about promotion — `Promotable { agreements, needed }` states a count
///    nobody is counting, and `AtCeiling` states that promotion is impossible. Choosing either
///    would be inventing the answer to §B9's hardest question.
///
/// So rather than fabricate two fields to reuse a richer type, the live path carries **exactly
/// what the permission layer actually knows** and says so on screen. A partial prompt that admits
/// what it does not know is a better basis for a decision than a complete-looking one with two
/// invented fields — and it cannot be mistaken for the finished overlay while it looks like this.
///
/// **This type is expected to disappear.** When `Effect` gains a fetch variant and the ledger can
/// produce a `Ceiling`, the live path builds a real `BlastRadius` and this goes away rather than
/// living on beside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingApproval {
    /// Matches the daemon's decision id. The answer must name it, or a reply could be read as the
    /// answer to a later question.
    pub decision: u64,
    pub verb: String,
    /// Every declared Target, rendered — including the numeric ones, which used to be dropped.
    pub scope: String,
    pub reversible: bool,
    /// `None` renders as *not assessed*, never as "routine".
    pub novelty: Option<String>,
}

impl PendingApproval {
    /// The line the user reads first.
    pub fn headline(&self) -> String {
        format!("{} · {}", self.verb, self.scope)
    }

    /// What is known about consequence. **Only what varies.**
    ///
    /// The first version printed *"reversible · novelty not assessed · ceiling unknown (trust
    /// ledger is M6)"*, and two of those three are **constants**: there is no novelty producer
    /// and no ceiling producer, so they said the same thing on every prompt and would keep
    /// saying it until M6. That is a note about the implementation, and §B1's rule is that
    /// implementation detail belongs under `--dev` rather than in the interface — the user is
    /// deciding whether to allow a fetch, not auditing which subsystems exist.
    ///
    /// **Not fabricating a field and not displaying its absence are different requirements**, and
    /// the first version confused them. The type still refuses to invent a ceiling; it just
    /// stopped announcing that it had refused.
    pub fn detail(&self) -> String {
        let mut parts = vec![if self.reversible {
            "reversible".to_string()
        } else {
            "NOT reversible".to_string()
        }];
        // Shown only when there is one. When novelty gains a producer this starts appearing on
        // its own, with no further change here.
        if let Some(n) = &self.novelty {
            parts.push(n.clone());
        }
        parts.join(" · ")
    }
}
