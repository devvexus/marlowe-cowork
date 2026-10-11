//! CONTRACTS.md §5 — `Budget`, and brief §8.2's spend caps.
//!
//! > **Hitting a cap pauses and asks. It never fails silently and never spends past the line.**
//!
//! # "Never past the line" is a pre-call cap, not a post-call check
//!
//! The obvious implementation checks `spent` at the top of each iteration and pauses when it
//! exceeds the budget. That satisfies *"never fails silently"* and fails *"never spends past
//! the line"*: by the time the check fires, the tokens are bought. The overspend is reported
//! accurately, which is what makes it easy to miss — the number in the pause message is right.
//!
//! So there are two mechanisms and they answer different questions:
//!
//! | Mechanism | Question it answers |
//! |---|---|
//! | [`Budget::exhausted`] at the top of the loop | may another step begin at all? |
//! | [`Budget::call_limits`] on every model call | can this step *return* more than is left? |
//!
//! The second is the one that holds the line: the provider is handed a hard output cap derived
//! from what remains, so a call cannot come back over budget. The first alone would be a
//! measurement of the overspend rather than a prevention of it.
//!
//! A driver that ignores its limit is still caught — [`Budget::exhausted`] sees it on the next
//! iteration — but that is the backstop, not the mechanism.

use serde::{Deserialize, Serialize};

use crate::profile::ModelRoute;

/// CONTRACTS.md §5. Six dimensions, all of them caps rather than targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    pub tokens: u64,
    pub wall_ms: u64,
    pub tool_calls: u32,
    pub subagents: u16,
    pub depth: u8,
    pub micros_usd: u64,
}

/// Below this many tokens left, a model call cannot produce anything worth the round trip, and
/// issuing one spends the remainder on a truncated step. The loop pauses instead.
///
/// It is a constant rather than a fraction of the budget because the floor is a property of
/// what a useful step costs, not of how large the run's allowance happens to be.
pub const MIN_CALL_TOKENS: u64 = 512;

/// The smallest FIRST model call this project has measured for a spawned child, prompt and
/// completion counted together — which is what `Usage::as_budget` adds to `spent`.
///
/// Journal seq 4597, 2026-08-26, on this machine: a toolless child with a two-line task, on the
/// stable tier this build assembles. **It is a measurement of one system and it is not
/// inherited** — a different persona, a different governance set or a different model moves it,
/// and the way to move this constant is to re-read the journal, not to argue about it.
pub const MEASURED_CHILD_FIRST_CALL_TOKENS: u64 = 3_089;

/// The smallest grant that buys a child a single reply — and the floor `Budget::grant` enforces.
///
/// # This was advice for one session, the model ignored it, and children died silently
///
/// ADR-057 made `budget_tokens` model-supplied. The tool description told the model that *"a few
/// hundred buys no model call at all"* and an earlier session declined to enforce it, on the
/// grounds that a refusal threshold would be *"a constant nobody has measured"*. Watched live
/// 2026-08-26: two of the next three spawns asked for **100** and **500** tokens (journal seq
/// 4584 and 4615). Both children paused on their first iteration having spent **nothing**, and
/// the parent's transcript read `spawn … failed · 0 tokens · no result`. The model, given no
/// reason it could act on, invented one — *"I did not have access to its documentation"* — and
/// abandoned the task.
///
/// # It is derived, not invented, which is what the earlier objection was actually about
///
/// `MIN_CALL_TOKENS` was **already the enforced floor** — `has_room_for_a_call` refuses to issue
/// a call below it, and that is precisely what fired. So the number was never missing; it was
/// read by the CONSUMER and not by the GRANTER, which is instance #16 in `CLAUDE.md` — a control
/// that exists, is correct, and is not read at the site that could have acted on it. A grant
/// below this hands out a budget the loop is *guaranteed* to reject, and the two halves of the
/// system disagreed in silence.
///
/// One measured first call, plus enough left over to issue a second: at exactly this figure a
/// child gets its reply and one more step, and below it a child cannot finish a sentence.
pub const MIN_CHILD_TOKENS: u64 = MEASURED_CHILD_FIRST_CALL_TOKENS + MIN_CALL_TOKENS;

/// Which dimension ran out. `&'static str` so it can travel into a journal payload and a
/// `BlockReason` without allocating, and so the set of names is closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct Dimension(pub &'static str);

/// What a model call is allowed to return, and **which model is to return it**.
///
/// # Why the route travels here
///
/// A `ModelDriver` is handed a view, a tool set and this. It has no access to the `Run`, so this
/// is the only carrier that can tell it which of `Routing`'s columns the call belongs to. Before
/// M3 Session C `OllamaDriver::request_body` hardcoded `ModelRoute::Orchestrator`, so
/// `CapabilityProfile::model_route` had **zero readers in the workspace** and the whole subagent
/// tree ran on the secretary's model -- ADR-008's cost lever declared in three documents and
/// inert in the product.
///
/// # No `Default`, no `#[serde(default)]`, and that is load-bearing
///
/// `route` has no default anywhere, which is why adding it was a compile error at all 32
/// construction sites rather than a silent inheritance at 31 of them. A later session adding a
/// `Default` impl as a convenience removes that property, so it is written down here rather than
/// left to be noticed.
///
/// The fields stay public, and the guard against a second production constructor is a test whose
/// input is the source tree rather than the type: `call_limits_has_one_production_constructor`
/// greps `crates/*/src/` and asserts exactly one hit. Privacy would have moved the same problem
/// into a `pub fn new`, which is a second way in wearing a different hat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallLimits {
    /// Hard cap handed to the provider. Derived from what is left, never from a default.
    pub max_output_tokens: u64,
    /// Which of `Routing`'s columns this call goes to. ADR-008: routing is by **task role**,
    /// declared in `CapabilityProfile`, never by user preference.
    ///
    /// **Read by `OllamaDriver::request_body` and by nothing on the other two driver paths**, and
    /// that asymmetry is stated rather than papered over: `llama-server` serves whatever weights
    /// it was launched with and does not route on the `"model"` field at all, and
    /// `OpenRouterDriver` holds a single model id rather than a `Routing`. A test asserting
    /// `body["model"]` differed by route on the llama.cpp arm would be green over unrouted
    /// weights -- the string, not the fate of the tokens.
    pub route: ModelRoute,
}

impl Budget {
    /// A budget generous enough for an interactive turn. Every field is a real number rather
    /// than `u64::MAX`: an unbounded dimension is a dimension whose cap never fires, and the
    /// point of the type is that all six fire.
    pub fn interactive() -> Self {
        Self {
            tokens: 200_000,
            wall_ms: 10 * 60 * 1_000,
            tool_calls: 200,
            subagents: 8,
            depth: 3,
            micros_usd: 2_000_000, // $2.00
        }
    }

    /// Which cap `spent` has reached, if any. Checked **before** any model spend.
    pub fn exhausted(&self, spent: &Budget) -> Option<Dimension> {
        if spent.tokens >= self.tokens {
            return Some(Dimension("tokens"));
        }
        if spent.wall_ms >= self.wall_ms {
            return Some(Dimension("wall_ms"));
        }
        if spent.tool_calls >= self.tool_calls {
            return Some(Dimension("tool_calls"));
        }
        if spent.subagents >= self.subagents {
            return Some(Dimension("subagents"));
        }
        if spent.micros_usd >= self.micros_usd {
            return Some(Dimension("micros_usd"));
        }
        // `depth` is not compared against `spent`: it is a property of the run's position in
        // the tree, checked at spawn (`slice_for`), not something a run accumulates.
        None
    }

    pub fn remaining(&self, spent: &Budget) -> Budget {
        Budget {
            tokens: self.tokens.saturating_sub(spent.tokens),
            wall_ms: self.wall_ms.saturating_sub(spent.wall_ms),
            tool_calls: self.tool_calls.saturating_sub(spent.tool_calls),
            subagents: self.subagents.saturating_sub(spent.subagents),
            depth: self.depth,
            micros_usd: self.micros_usd.saturating_sub(spent.micros_usd),
        }
    }

    /// The cap handed to the provider for the next call. **This is the line.**
    ///
    /// `route` is the run's own -- `run.profile.model_route()` at the one call site -- so the
    /// column is a property of the run rather than of the call, and no caller can name a model
    /// the run's profile did not declare.
    pub fn call_limits(&self, spent: &Budget, route: ModelRoute) -> CallLimits {
        CallLimits { max_output_tokens: self.remaining(spent).tokens, route }
    }

    /// Whether enough remains for a call to be worth issuing.
    pub fn has_room_for_a_call(&self, spent: &Budget) -> bool {
        self.remaining(spent).tokens >= MIN_CALL_TOKENS
    }

    /// The child's budget. §10.1: declared at spawn, never inferred.
    ///
    /// # GRANTED, NEVER SLICED — M3 Session A, and this replaces `slice_for`
    ///
    /// `slice_for` took its fraction of what **remained**, which for repeated spawning decays
    /// geometrically. `CLAUDE.md` records what that produced at depth one: the eighth quarantined
    /// reader held **~0.3%** of the budget, and nothing reported it — the reader simply returned a
    /// worse summary of an attacker-controlled page, which is the output nobody can audit.
    /// `slice_for_quarantined_read` was written to escape exactly that, for one caller.
    ///
    /// M3's tree is depth four before tool-spawned agents, so the decay compounds three more
    /// times and it starves the leaves, which is where all the work happens. So the general case
    /// now does what the quarantine case already did: **the share is of `self` — the run's
    /// ORIGINAL budget — and is then clamped by what actually remains.** Every sibling is offered
    /// the same allocation until the run is genuinely out of budget, at which point this refuses
    /// with the numbers rather than handing out a slice too small to use.
    ///
    /// `explicit` is the *"a master with 200k spends it or hands it down"* half: a parent that
    /// knows the job may name the amount, and it is refused by name if it exceeds what is left.
    /// `None` means *"decide for me"*, and takes `share` of the original.
    ///
    /// **Depth is still structural.** `Err(GrantRefused::NoDepth)` at `depth == 0`: a master
    /// cannot conjure depth, and the caller turns the refusal into something the model can read
    /// rather than running a child at `depth: 0` that spawns anyway.
    pub fn grant(
        &self,
        spent: &Budget,
        share: BudgetShare,
        explicit: Option<u64>,
    ) -> Result<Budget, GrantRefused> {
        if self.depth == 0 {
            return Err(GrantRefused::NoDepth);
        }
        let left = self.remaining(spent);
        if left.tokens == 0 {
            return Err(GrantRefused::PoolEmpty { dimension: "tokens" });
        }
        // ── THE FLOOR, ENFORCED WHERE THE BUDGET IS HANDED OUT ──────────────────────────
        //
        // `has_room_for_a_call` already refused to issue a call below `MIN_CALL_TOKENS`. Reading
        // that floor only at the point of spending meant `grant` could hand out a budget the loop
        // was certain to reject, and it did — twice in three spawns. See `MIN_CHILD_TOKENS`.
        if let Some(want) = explicit {
            if want > left.tokens {
                return Err(GrantRefused::MoreThanRemains { want, left: left.tokens });
            }
        }
        if left.tokens < MIN_CHILD_TOKENS {
            return Err(GrantRefused::PoolTooSmall { left: left.tokens, min: MIN_CHILD_TOKENS });
        }
        if let Some(want) = explicit {
            if want < MIN_CHILD_TOKENS {
                return Err(GrantRefused::BelowFloor { want, min: MIN_CHILD_TOKENS });
            }
        }
        let of_original = |original: u64| share.apply_u64(original);
        // The share path is floored too, and floored SILENTLY rather than refused: the parent did
        // not choose this number, the harness did, so refusing would report the parent's request
        // as the fault. `left.tokens >= MIN_CHILD_TOKENS` is guaranteed above, so raising to the
        // floor can never exceed what remains.
        let tokens = explicit
            .unwrap_or_else(|| of_original(self.tokens))
            .min(left.tokens)
            .max(MIN_CHILD_TOKENS);
        Ok(Budget {
            // **Every dimension floors at 1 while the parent still has any.** Audit findings C4
            // and C5 — the seventeenth instance, twice, in the function that hands budgets out.
            //
            // `Budget::exhausted` compares `spent >= budget`, so a granted dimension that rounds
            // to **zero means ALREADY EXHAUSTED, not "may not use"**: the child pauses before its
            // first model call and returns nothing, and the caller reports a refusal whose stated
            // reason is not the real one.
            //
            // Withholding a capability is done structurally elsewhere — `ExposedSet::empty()`
            // means there is no tool to call, `depth: 0` means a spawn is refused. A counter set
            // to zero is not a prohibition, it is a spent budget.
            tokens: at_least_one_u64(tokens, left.tokens),
            wall_ms: at_least_one_u64(of_original(self.wall_ms).min(left.wall_ms), left.wall_ms),
            tool_calls: at_least_one_u32(
                share.apply_u32(self.tool_calls).min(left.tool_calls),
                left.tool_calls,
            ),
            // A child may not spawn more children than its parent had left, and it starts one
            // short because it is itself one of them.
            subagents: at_least_one_u16(left.subagents.saturating_sub(1), left.subagents),
            depth: self.depth - 1,
            micros_usd: at_least_one_u64(
                of_original(self.micros_usd).min(left.micros_usd),
                left.micros_usd,
            ),
        })
    }

    /// What fraction of the **original** token budget one quarantined read may spend.
    ///
    /// A quarter. Generous on purpose: this reader is the only thing standing between a fetched
    /// page and the run, and a reader too poor to summarise does not fail — it returns something
    /// worse, which is indistinguishable from a page that had little to say.
    pub const QUARANTINED_READ_NUMERATOR: u64 = 2;
    pub const QUARANTINED_READ_DENOMINATOR: u64 = 8;

    /// The slice a **quarantined read** gets. Deliberately not [`slice_for`], and the two
    /// differences were each a bug in effect.
    ///
    /// # 1. A share of the ORIGINAL budget, not of what remains
    ///
    /// `slice_for` takes its fraction of `remaining`, which for a repeated operation decays
    /// geometrically: read 1 gets ⅛ of B, read 2 gets ⅛ of what is left, and **read 8's reader
    /// holds about 0.3% of the original budget.** Nothing reports that. The reader simply returns
    /// a worse summary, and a degraded summary of an attacker-controlled page is precisely the
    /// output nobody can audit.
    ///
    /// Here the share is taken from `self` — the original — and then clamped by what actually
    /// remains, so every read is offered the same allocation until the run is genuinely out of
    /// budget, at which point the caller fails closed with a message that says so.
    ///
    /// # 2. Depth is not consumed, because the reader cannot spawn
    ///
    /// `slice_for` refuses at `depth == 0`, which is correct for a subagent that might spawn its
    /// own. A quarantined reader holds `ExposedSet::empty()`, so it structurally **cannot** call
    /// `run` — it has no need of depth and returning `None` for it was a capability that
    /// disappeared with distance from the root. Live consequence: a run four levels deep could
    /// fetch a page and then never read it, receiving *"no budget remained to condense it"* while
    /// the real cause was depth. The child is given `depth: 0` and `subagents: 0`, which is what
    /// a run that cannot spawn actually needs.
    /// # 3. The floor `grant` enforces and this function did not
    ///
    /// **`MIN_CHILD_TOKENS` was read by one of the two functions that hand out a child budget.**
    /// `grant` refuses below it by name — `PoolTooSmall`, `BelowFloor` — because a grant beneath
    /// one measured first call hands out a budget the loop is *guaranteed* to reject. This
    /// function computed a quarter of the original, floored it at **1**, and returned `Some`.
    ///
    /// The arithmetic, because the defect has a threshold rather than being universal. The share
    /// is `tokens * 2 / 8`, so it falls below `MIN_CHILD_TOKENS` whenever the run's own budget is
    /// under `4 * MIN_CHILD_TOKENS` = **14,404 tokens**. `Budget::interactive()` is 200,000, so a
    /// root-level read was never affected and nothing on the interactive path could show it. A
    /// *child* running a `web` call is: a child granted 10,544 tokens gave its reader **2,636**
    /// against a **3,089**-token measured first call, so the reader paused before that call having
    /// spent nothing, and the parent read *"the content could not be condensed"*.
    ///
    /// **That is instance #17's exact symptom at the layer-1 component**, and it reached the
    /// parent as a sentence about the page rather than about the budget — the reading that is
    /// indistinguishable from a page with little to say, which the section above names as the
    /// output nobody can audit.
    ///
    /// **A SHORT SHARE AND A SHORT POOL ARE TWO STATES AND ONLY ONE IS A REFUSAL.** The 10,544
    /// case above has a pool covering the floor three times over — nothing is exhausted, the
    /// *fraction* is simply too small — so the answer is to **raise to the floor**, and refusing
    /// there would deny a read the run could easily afford. Only `left.tokens < MIN_CHILD_TOKENS`
    /// is a genuine refusal. The first test written against this asserted `None` for the 10,544
    /// case and was wrong about the product rather than about the defect; the distinction is
    /// recorded here because collapsing the two is how a fix for starvation becomes a cause of it.
    ///
    /// It is also instance #16's shape one step on: the control existed, was correct, and was not
    /// read at the second site that could act on it. `MIN_CHILD_TOKENS`'s own doc comment says
    /// this in the singular — *"it was read by the CONSUMER and not by the GRANTER"* — and there
    /// were two granters.
    ///
    /// **`MIN_CHILD_TOKENS` IS A LOWER BOUND HERE, NOT THE RIGHT NUMBER.** It was measured on *"a
    /// toolless child with a two-line task"*; a quarantined reader's first call carries a fetched
    /// **page**, so its true floor is strictly higher. The honest floor is a
    /// `MEASURED_QUARANTINED_READ_TOKENS` read from the journal, and it is deliberately not
    /// written here — an invented constant is worse than a missing one, and a divisor guessed from
    /// characters would be a number nobody measured wearing a measurement's clothes. What this
    /// closes is the case where the reader could not afford *any* first call. What it does not
    /// close is the case where it cannot afford *this* one.
    pub fn slice_for_quarantined_read(&self, spent: &Budget) -> Option<Budget> {
        let left = self.remaining(spent);
        if left.tokens == 0 || left.wall_ms == 0 {
            return None;
        }
        // Fail closed, exactly as `grant` does. The caller already renders `None` as a named,
        // actionable refusal — *"no budget remained to condense it"* — which is true, and which is
        // a better thing for the parent to read than a summary produced by a reader that was
        // never able to speak.
        if left.tokens < MIN_CHILD_TOKENS {
            return None;
        }
        let share = |original: u64, remaining: u64| {
            original
                .saturating_mul(Self::QUARANTINED_READ_NUMERATOR)
                .checked_div(Self::QUARANTINED_READ_DENOMINATOR)
                .unwrap_or(0)
                .min(remaining)
        };
        Some(Budget {
            // Raised to the floor, never above what remains: `left.tokens >= MIN_CHILD_TOKENS` is
            // guaranteed by the refusal above, so the `max` can only move a share that was too
            // small and can never exceed the pool. That is the same argument `grant` makes, and it
            // is the reason the refusal has to come first rather than being folded in here.
            tokens: at_least_one_u64(share(self.tokens, left.tokens), left.tokens)
                .max(MIN_CHILD_TOKENS),
            wall_ms: at_least_one_u64(share(self.wall_ms, left.wall_ms), left.wall_ms),
            // **1, not 0, and the difference is not cosmetic.**
            //
            // [`Budget::exhausted`] compares `spent >= budget`, so a dimension set to zero reads
            // as *already exhausted* rather than *may not use*. A reader handed `tool_calls: 0`
            // paused before its first model call, returned nothing, and the parent reported "the
            // content could not be condensed" — a quarantine that silently stopped reading
            // anything at all.
            //
            // The capability is withheld structurally instead, which is stronger than a counter:
            // the profile is `ExposedSet::empty()`, so there is no tool to call, and `depth: 0`
            // means `slice_for` refuses any spawn. These numbers exist only to keep the budget
            // check from firing on the first iteration.
            tool_calls: 1,
            subagents: 1,
            depth: 0,
            // C4: `micros_usd` is the dimension the first pass at instance 17 did not
            // enumerate, and it is the one whose failure mode is money.
            micros_usd: at_least_one_u64(share(self.micros_usd, left.micros_usd), left.micros_usd),
        })
    }

    /// Accumulate a step's cost.
    pub fn add(&mut self, other: &Budget) {
        self.tokens = self.tokens.saturating_add(other.tokens);
        self.wall_ms = self.wall_ms.saturating_add(other.wall_ms);
        self.tool_calls = self.tool_calls.saturating_add(other.tool_calls);
        self.subagents = self.subagents.saturating_add(other.subagents);
        self.micros_usd = self.micros_usd.saturating_add(other.micros_usd);
    }
}

/// Why a spawn was refused a budget. **Every variant carries the numbers**, because a refusal a
/// model cannot act on produces a retry loop rather than a smaller request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum GrantRefused {
    #[error(
        "this run is at the bottom of its declared depth and cannot spawn. A master allocates          depth it was given; it cannot conjure more"
    )]
    NoDepth,
    #[error("the {dimension} pool is empty; there is nothing left to grant")]
    PoolEmpty { dimension: &'static str },
    #[error(
        "a grant of {want} tokens was asked for and {left} remain. A grant is deducted from the          parent's pool, so it cannot exceed it"
    )]
    MoreThanRemains { want: u64, left: u64 },
    /// The grant is too small for the child to make one model call.
    ///
    /// Named with both numbers because a model told only "refused" retries the same request, and
    /// this refusal is one the model can fix by editing a single field.
    #[error(
        "a grant of {want} tokens was asked for and a child needs at least {min}: its first model          call spends its whole brief before it emits a word, so a child granted less pauses          before it speaks and returns nothing. Ask for {min} or more, or omit `budget_tokens`          and take the share sized for the job"
    )]
    BelowFloor { want: u64, min: u64 },
    /// Something remains, but not enough for any child to do anything with.
    #[error(
        "{left} tokens remain and a child needs at least {min} to make one model call, so nothing          can usefully be delegated from here. Do the work in this run"
    )]
    PoolTooSmall { left: u64, min: u64 },
}

/// How much of the run's **original** budget a child is granted. §10.2's effort scaling,
/// expressed as a declared fraction rather than a heuristic the orchestrator applies invisibly.
///
/// # The numerators moved in M3 Session A, and the reason is the acceptance row
///
/// They were 1/8, 2/8, 4/8 **of what remained**. M3's acceptance requires the leaf share at
/// **depth 4** to sit inside a declared band and never fall below 1% of the root. A default of
/// 2/8 compounded four times is 0.39% — under the line before any sibling decay is counted at
/// all. `Standard` is now **3/8**, which measures **1.98%** at depth 4 from a 200k root:
/// `a_leaf_at_depth_four_is_inside_the_declared_band` is the command that prints it.
///
/// `Large` is 5/8 rather than 4/8 for the reason the old comment gave and could not enforce:
/// *never all of it* — a parent that hands over everything cannot synthesise the result it asked
/// for. 5/8 leaves 3/8.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetShare {
    /// A quick lookup. One eighth.
    Small,
    /// The default for a worker. One quarter.
    Standard,
    /// A substantial investigation. One half. Never all of it — a parent that hands over
    /// everything cannot synthesise the result it asked for.
    Large,
}

impl BudgetShare {
    fn numerator(self) -> u64 {
        match self {
            BudgetShare::Small => 1,
            BudgetShare::Standard => 3,
            BudgetShare::Large => 5,
        }
    }

    fn apply_u64(self, v: u64) -> u64 {
        v.saturating_mul(self.numerator()) / 8
    }

    fn apply_u32(self, v: u32) -> u32 {
        ((v as u64).saturating_mul(self.numerator()) / 8) as u32
    }
}


/// A granted dimension is never zero while the parent still has some of it.
///
/// See [`Budget::grant`] for why: `exhausted` compares `spent >= budget`, so zero reads as
/// *already spent*, and a child handed a zero pauses before its first call. `remaining == 0` is the
/// one case where zero is the truth, and it is passed through so the caller's own guard can see it.
fn at_least_one_u64(sliced: u64, remaining: u64) -> u64 {
    if remaining == 0 {
        0
    } else {
        sliced.max(1)
    }
}

fn at_least_one_u32(sliced: u32, remaining: u32) -> u32 {
    if remaining == 0 {
        0
    } else {
        sliced.max(1)
    }
}

fn at_least_one_u16(sliced: u16, remaining: u16) -> u16 {
    if remaining == 0 {
        0
    } else {
        sliced.max(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spent(tokens: u64) -> Budget {
        Budget { tokens, ..Budget::default() }
    }

    #[test]
    fn the_call_cap_is_what_is_left_not_a_default() {
        let b = Budget { tokens: 10_000, ..Budget::interactive() };
        assert_eq!(b.call_limits(&spent(0), ModelRoute::Worker).max_output_tokens, 10_000);
        assert_eq!(b.call_limits(&spent(9_000), ModelRoute::Worker).max_output_tokens, 1_000);
        // The property the report item names: a call issued at this point cannot come back
        // over the line, because the provider was told the line.
        assert_eq!(b.call_limits(&spent(10_000), ModelRoute::Worker).max_output_tokens, 0);
    }

    #[test]
    fn every_dimension_can_be_the_one_that_fires() {
        // A budget test that only ever exercises tokens is a budget test for one dimension.
        let b = Budget {
            tokens: 10,
            wall_ms: 10,
            tool_calls: 10,
            subagents: 10,
            depth: 2,
            micros_usd: 10,
        };
        let cases: [(Budget, &str); 5] = [
            (Budget { tokens: 10, ..Default::default() }, "tokens"),
            (Budget { wall_ms: 10, ..Default::default() }, "wall_ms"),
            (Budget { tool_calls: 10, ..Default::default() }, "tool_calls"),
            (Budget { subagents: 10, ..Default::default() }, "subagents"),
            (Budget { micros_usd: 10, ..Default::default() }, "micros_usd"),
        ];
        for (spent, dimension) in cases {
            assert_eq!(
                b.exhausted(&spent),
                Some(Dimension(dimension)),
                "spending {spent:?} should exhaust {dimension}"
            );
        }
        assert_eq!(b.exhausted(&Budget::default()), None);
    }

    #[test]
    fn depth_bounds_the_tree_rather_than_accumulating() {
        let root = Budget { depth: 2, ..Budget::interactive() };
        let child = root.grant(&Budget::default(), BudgetShare::Standard, None).unwrap();
        assert_eq!(child.depth, 1);
        let grandchild = child.grant(&Budget::default(), BudgetShare::Standard, None).unwrap();
        assert_eq!(grandchild.depth, 0);
        assert_eq!(
            grandchild.grant(&Budget::default(), BudgetShare::Standard, None),
            Err(GrantRefused::NoDepth),
            "the tree stops; a great-grandchild is refused rather than run at depth 0"
        );
    }

    #[test]
    fn a_child_never_receives_the_whole_remaining_budget() {
        let b = Budget::interactive();
        for share in [BudgetShare::Small, BudgetShare::Standard, BudgetShare::Large] {
            let child = b.grant(&Budget::default(), share, None).unwrap();
            assert!(
                child.tokens < b.tokens,
                "a parent that hands over everything cannot synthesise the answer it asked for"
            );
            assert!(child.subagents < b.subagents);
        }
    }

    /// **The acceptance row, as a command that prints a number.** M3 §11: *"Leaf budget share at
    /// depth 4 — within a declared band of the grant; never `< 1%`."*
    ///
    /// The band is declared here, in the assertion, rather than in prose: **[1%, 5%] of the
    /// root**, at the default share, over four levels.
    #[test]
    fn a_leaf_at_depth_four_is_inside_the_declared_band() {
        let root = Budget { depth: 4, ..Budget::interactive() };
        let mut b = root;
        for level in 1..=4 {
            b = b
                .grant(&Budget::default(), BudgetShare::Standard, None)
                .unwrap_or_else(|e| panic!("level {level} refused: {e}"));
        }
        let share = b.tokens as f64 / root.tokens as f64;
        println!("leaf at depth 4: {} of {} tokens = {:.2}%", b.tokens, root.tokens, share * 100.0);
        assert!(share >= 0.01, "leaf starved at {:.3}% -- the band's floor is 1%", share * 100.0);
        assert!(share <= 0.05, "leaf over-granted at {:.3}%; the band's ceiling is 5%", share * 100.0);
    }

    /// **The control for the row above.** It fails if `grant` ever goes back to taking its share
    /// of the *remainder*, which is what starved the eighth reader to 0.3%.
    ///
    /// Two spawns from the same parent, with the first one's whole budget already spent: under
    /// slicing the second sibling gets a fraction of a depleted pool; under granting it is offered
    /// the same allocation until the pool is genuinely gone.
    #[test]
    fn a_second_sibling_is_offered_the_same_allocation_as_the_first() {
        let b = Budget { depth: 2, ..Budget::interactive() };
        let first = b.grant(&Budget::default(), BudgetShare::Standard, None).unwrap();
        let after_first = Budget { tokens: first.tokens, ..Budget::default() };
        let second = b.grant(&after_first, BudgetShare::Standard, None).unwrap();
        assert_eq!(
            first.tokens, second.tokens,
            "a grant is a share of the ORIGINAL; the second sibling must not be paid out of the              first one's leftovers"
        );
    }

    #[test]
    fn an_explicit_grant_larger_than_the_pool_is_refused_with_both_numbers() {
        let b = Budget { tokens: 1_000, depth: 2, ..Budget::interactive() };
        let e = b.grant(&Budget::default(), BudgetShare::Standard, Some(5_000)).unwrap_err();
        assert_eq!(e, GrantRefused::MoreThanRemains { want: 5_000, left: 1_000 });
        // A model that is told only "refused" retries the same request. One told the numbers
        // can ask for less.
        assert!(e.to_string().contains("5000") && e.to_string().contains("1000"), "{e}");
    }

    #[test]
    fn an_explicit_grant_inside_the_pool_is_honoured_exactly() {
        // "A master with 200k spends it or hands it down." A named amount is not re-derived.
        let b = Budget { tokens: 200_000, depth: 2, ..Budget::interactive() };
        let child = b.grant(&Budget::default(), BudgetShare::Small, Some(150_000)).unwrap();
        assert_eq!(child.tokens, 150_000, "an explicit grant overrides the share, in both directions");
    }

    #[test]
    fn there_is_a_floor_below_which_a_call_is_not_worth_issuing() {
        let b = Budget { tokens: 1_000, ..Budget::interactive() };
        assert!(b.has_room_for_a_call(&spent(0)));
        assert!(!b.has_room_for_a_call(&spent(600)));
    }

    /// The regression for the live failure of 2026-08-26, asserted where the number is DECIDED.
    ///
    /// The child that died was granted 500 tokens and the loop needs 512 to issue a call, so the
    /// two constants have to be compared somewhere. This compares them: any grant `Budget::grant`
    /// returns must be one `has_room_for_a_call` accepts. **Delete the floor and this fails on
    /// the exact figure the model asked for.**
    #[test]
    fn a_granted_child_can_always_make_at_least_one_call() {
        let parent = Budget::interactive();
        let none = Budget::default();

        // What the model actually asked for, twice, in one evening.
        for want in [1_u64, 100, 500, MIN_CHILD_TOKENS - 1] {
            match parent.grant(&none, BudgetShare::Standard, Some(want)) {
                Err(GrantRefused::BelowFloor { want: w, min }) => {
                    assert_eq!(w, want);
                    assert_eq!(min, MIN_CHILD_TOKENS);
                }
                other => panic!("a grant of {want} tokens was not refused: {other:?}"),
            }
        }

        // And every grant that IS handed out clears the floor the loop enforces, whichever path
        // produced it -- explicit, or a share small enough to round under it.
        for share in [BudgetShare::Small, BudgetShare::Standard, BudgetShare::Large] {
            for tokens in [MIN_CHILD_TOKENS, 10_000, 200_000] {
                let parent = Budget { tokens, ..Budget::interactive() };
                let child = parent
                    .grant(&none, share, None)
                    .unwrap_or_else(|e| panic!("{share:?} of {tokens}: {e}"));
                assert!(
                    child.has_room_for_a_call(&none),
                    "{share:?} of {tokens} granted {} tokens, below the {MIN_CALL_TOKENS} the loop needs",
                    child.tokens,
                );
                assert!(child.tokens <= parent.remaining(&none).tokens, "granted more than remains");
            }
        }
    }

    /// The other half: a parent too poor to delegate is told so, rather than producing a child
    /// that pauses before it speaks. The refusal names both numbers because the model has to
    /// decide what to do instead.
    #[test]
    fn a_parent_below_the_floor_refuses_to_delegate_rather_than_starving_a_child() {
        let parent = Budget::interactive();
        let nearly_spent = Budget { tokens: parent.tokens - 1_000, ..Budget::default() };
        match parent.grant(&nearly_spent, BudgetShare::Standard, None) {
            Err(GrantRefused::PoolTooSmall { left, min }) => {
                assert_eq!(left, 1_000);
                assert_eq!(min, MIN_CHILD_TOKENS);
            }
            other => panic!("expected PoolTooSmall, got {other:?}"),
        }
    }

    /// **The same floor, at the OTHER function that hands out a child budget.**
    ///
    /// `grant` refused below `MIN_CHILD_TOKENS`; `slice_for_quarantined_read` did not, and the
    /// component it starves is layer 1 — the only thing standing between a fetched page and the
    /// run. Below the threshold it returned `Some` with a quarter of a small budget, the reader
    /// paused before its first model call having spent nothing, and the parent was told *"the
    /// content could not be condensed"* — a sentence about the page, not about the budget.
    ///
    /// **Every assertion here is paired with a control**, because the cheap wrong fix is to floor
    /// every reader at `MIN_CHILD_TOKENS` and the cheaper one is to refuse every reader. Either
    /// would make a one-sided test green while destroying the quarter that `QUARANTINED_READ_*`
    /// exists to give.
    #[test]
    fn a_quarantined_reader_is_refused_rather_than_handed_less_than_one_first_call() {
        // The threshold is arithmetic, not a guess: the share is 2/8, so it falls under the floor
        // exactly when the run's own budget is under four times it.
        let threshold = MIN_CHILD_TOKENS * Budget::QUARANTINED_READ_DENOMINATOR
            / Budget::QUARANTINED_READ_NUMERATOR;
        assert_eq!(threshold, 14_404, "the documented threshold moved; the doc comment must too");

        // ── the defect, at the measured figure ────────────────────────────────────────────
        //
        // A child granted 10,544 tokens gave its reader 2,636 against a 3,089-token measured
        // first call. **The pool was never the problem here — 10,544 covers the floor three times
        // over — so the answer is to RAISE, not to refuse.** The first version of this test
        // asserted `None` and was wrong about the product rather than about the defect; it is
        // recorded rather than quietly corrected, because "the share is short" and "the pool is
        // short" are two different states and only the second is a refusal.
        let starving = Budget { tokens: 10_544, ..Budget::interactive() };
        assert!(
            starving.tokens < threshold,
            "10,544 must sit below the threshold or this row measures nothing"
        );
        let raised = starving
            .slice_for_quarantined_read(&Budget::default())
            .expect("the pool covers the floor, so this is a raise and not a refusal");
        assert_eq!(
            raised.tokens, MIN_CHILD_TOKENS,
            "before the floor this was Some(2_636) — 453 tokens short of \
             MEASURED_CHILD_FIRST_CALL_TOKENS — so the reader paused before its first call having \
             spent nothing and the parent read a sentence about the page, not about the budget"
        );
        assert!(
            raised.tokens > 2_636,
            "control: the raise actually moved the number it exists to move"
        );

        // ── control 1: the floor did not swallow the quarter ──────────────────────────────
        //
        // Without this, flooring every reader at MIN_CHILD_TOKENS passes the row above.
        let generous = Budget::interactive();
        let reader = generous
            .slice_for_quarantined_read(&Budget::default())
            .expect("an interactive run must still get a reader");
        assert_eq!(
            reader.tokens, 50_000,
            "the quarter is the point; a reader pinned to the floor is a different defect"
        );
        assert!(
            reader.tokens > MIN_CHILD_TOKENS,
            "control: this reader is above the floor for its share, not because of it"
        );

        // ── control 2: refusal is not universal ───────────────────────────────────────────
        //
        // Without this, `return None` unconditionally passes both rows above.
        let exactly_enough = Budget { tokens: threshold, ..Budget::interactive() };
        let at_edge = exactly_enough
            .slice_for_quarantined_read(&Budget::default())
            .expect("a run exactly at the threshold must still read its page");
        assert_eq!(
            at_edge.tokens, MIN_CHILD_TOKENS,
            "at the threshold the share and the floor coincide, which is what makes it the threshold"
        );

        // ── control 3: the raise never exceeds the pool ───────────────────────────────────
        //
        // `.max(MIN_CHILD_TOKENS)` is only sound because the refusal above guarantees the pool can
        // cover it. A budget whose ORIGINAL is large but whose REMAINDER is thin is where that
        // argument would break if the refusal read `self` instead of `left`.
        let mostly_spent =
            Budget { tokens: generous.tokens - MIN_CHILD_TOKENS, ..Budget::default() };
        let thin = generous
            .slice_for_quarantined_read(&mostly_spent)
            .expect("exactly the floor remains, so the read is affordable");
        assert_eq!(thin.tokens, MIN_CHILD_TOKENS);
        assert!(
            thin.tokens <= generous.remaining(&mostly_spent).tokens,
            "a reader was granted more than the pool held"
        );

        // And one token less than the floor is a refusal, not an overdraft.
        let one_short = Budget { tokens: generous.tokens - (MIN_CHILD_TOKENS - 1), ..Budget::default() };
        assert_eq!(generous.slice_for_quarantined_read(&one_short), None);
    }
}
