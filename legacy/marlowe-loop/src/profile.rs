//! CONTRACTS.md §5 — `CapabilityProfile`.
//!
//! > One loop. Research, voice, coding, automation, consolidation, and quarantined reading are
//! > **capability profiles** — differing in tool exposure, budgets, and interrupt policy — not
//! > variants.
//!
//! The named constructors in this file are that sentence made executable. There is no
//! `ConsolidationLoop` and no `QuarantinedReader` type; there is one loop and
//! [`CapabilityProfile::consolidation`] / [`CapabilityProfile::quarantined_reader`].
//!
//! # The load-time error, and why it is a constructor
//!
//! CONTRACTS §5:
//!
//! > `reads_untrusted && !exposed_tools.is_empty()` is a load-time error. That is §8.2's
//! > structural trifecta break, expressed as a type invariant rather than a guideline.
//!
//! Every field is private and [`CapabilityProfile::new`] is the only way in — including for
//! `serde`, which routes through it. A public-field struct with a `validate()` method beside it
//! would leave the invalid state constructible, and a test asserting `validate()` returns an
//! error would then be measuring a function nobody has to call.

use marlowe_permission::EgressPolicy;
use marlowe_tools::{ExposedSet, ExposureError, ToolId};
use serde::{Deserialize, Serialize};

/// §9's mid-tool interrupt policy. **In code, not a config knob** — the brief is explicit that
/// tool semantics belong in code review.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterruptPolicy {
    /// A user is present and may cut in mid-turn. Idempotent reads complete; mutations cancel
    /// on contradiction.
    Interruptible,
    /// No interactive surface. Steering still applies, at iteration boundaries — a child that
    /// cannot be steered is a child you discover has failed at minute 60.
    Unattended,
}

/// ADR-008 — routing is by **task role**, declared here, never by user preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelRoute {
    /// Strong model: orchestration and synthesis.
    Orchestrator,
    /// Fast and cheap: subagent search, extraction, classification, consolidation.
    Worker,
    /// Cheapest: the compaction summarizer. Named separately because §6 requires compaction to
    /// fire at 70% rather than at exhaustion, and the whole point is that this call is cheap
    /// enough to make that affordable.
    Summarizer,
}

/// M3-DESIGN section 1's five agent levels. **Position in the organisation, not capability.**
///
/// Capability is the [`ExposedSet`]; this is what constrains which sets are *constructible*.
/// It lives on [`CapabilityProfile`] rather than on the `Run` for the reason
/// [`CapabilityProfile::grant_egress_host`] gives one field over: the invariant a level carries
/// -- today *"a master's set does not contain `ask`"*, and until the 2026-08-31 reversal
/// *"a master's set contains only management tools"* -- is a statement **about the exposed set**,
/// and the exposed set lives behind this file's validating constructor. Hold the level beside the
/// profile on the `Run` and the invariant is bypassed rather than enforced: a `Master` run could
/// be constructed with an `ask`-holding profile and every profile test would stay green. `Run::adopted_by` also mutates a run's parent, so a level held there could be
/// falsified after the fact by a lifetime decision.
///
/// # The ladder these levels are staffed from
///
/// `DECISIONS.md`'s **2026-08-30** entry settles it, and it **overrides ADR-064 and ADR-069**,
/// which were both written against `AGENT-DIRECTORY.md` section 2's older table:
///
/// | Tier | Model | Level it usually staffs |
/// |---|---|---|
/// | Secretary | the user's `models` dropdown | [`AgentLevel::Secretary`] |
/// | Agent-High | `marlowe-dusk:27b-super` | [`AgentLevel::TopAgent`] |
/// | Agent-Medium | `marlowe-dawn:9b-super` | [`AgentLevel::Master`] |
/// | Agent-Low | `marlowe-mini:4b-super` | [`AgentLevel::Worker`] |
///
/// **There is no unnamed fourth role**, and both ADRs carry that blocker forward from the old
/// table; where they conflict with the decision entry, the entry wins. **A ROLE IS A SLOT, NOT A
/// MODEL**: the human's own testing configuration points Agent-High and Agent-Medium at the same
/// tag, so two tiers resolving to one model is the ordinary case rather than a degenerate one.
/// Nothing here, and nothing downstream of it, may assume the tiers differ -- which is also the
/// shape production ships in today, where every `Routing` is `Routing::uniform` and all three of
/// [`ModelRoute`]'s columns answer one name. Anything that later counts capacity keys on the
/// **resolved model**, never on the level or the route: two roles sharing a model share weights
/// and multiply only the KV cache, where two models multiply weights.
///
/// A level is **derived by the harness** ([`AgentLevel::child_of`]) and never named by a model.
/// The model names a *disposition* and a *route*; the level is what the harness computes from
/// the parent's own level and that disposition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentLevel {
    /// Level 1. Marlowe himself. Section 1's *"full conversational set"*.
    Secretary,
    /// Level 2. What Marlowe spawns, and **the only thing he spawns**.
    ///
    /// Section 1.1: Marlowe spawns exactly one kind of thing and declares *which kind it is*.
    /// `manages` **is** that declaration, carried inside the level so it cannot be recorded and
    /// then ignored. A separate `disposition` field beside the level was the first design's
    /// fatal defect: `child_of(Secretary, Manage)` and `child_of(Secretary, Work)` both returned
    /// the same value, so nothing downstream could tell them apart and a `work` top-agent got a
    /// create grant anyway (ADR-064 section 8.1).
    TopAgent { manages: bool },
    /// Level 3. Spawned by a top-agent that was itself spawned to manage. **May hold working
    /// tools** -- section 1.2's structural withholding was reversed by the human on 2026-08-31,
    /// because the PI is the senior researcher who does the hardest part himself and spawns help.
    /// The one tool still withheld is `ask`: only a top-agent reaches the user (section 3.1), so a
    /// question raised here has nobody to arrive at. See `CapabilityProfile::new`'s `Master` arm.
    Master,
    /// Level 4. Does the work. Creates nothing.
    Worker,
    /// Level 5. Spawned by a **tool**, not by a model: layer 1's quarantined reader and
    /// SCOPED-MEMORY section 4's fact extractor. Holds no tools at all.
    ToolSpawned,
}

/// Section 1.1's one question, asked at spawn: *can one agent do this alone?*
///
/// Read by [`AgentLevel::child_of`], **whose two arms return different values**. That is the
/// whole reason this type exists rather than being inferred from whether the requested tool set
/// names `run` -- M3-DESIGN section 5 is *declared at spawn, never inferred*, and letting
/// [`CapabilityProfile::new`] refuse the disagreement is strictly better than deriving one from
/// the other, because the two cannot then silently disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    /// `worker` -- will do the task itself.
    Work,
    /// `master` -- gets a create grant and an agent budget.
    Manage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error(
    "a {parent:?} cannot create a {wanted:?} child: Marlowe creates only top-agents, a top-agent \
     creates only if it was spawned to manage, a master creates only workers, and a worker \
     creates nothing"
)]
pub struct LevelRefusal {
    pub parent: AgentLevel,
    pub wanted: Disposition,
}

impl AgentLevel {
    /// The level a child gets. **Total over the level axis, with no wildcard arm** -- a sixth
    /// variant fails to compile here rather than silently inheriting a neighbour's answer.
    ///
    /// It **never returns [`AgentLevel::ToolSpawned`]**. That level is reachable only through
    /// this file's own named constructors, so no model call can produce one: withheld
    /// structurally, in the [`ExposedSet::empty`] manner rather than by a counter set to zero
    /// (instance #17).
    ///
    /// **Total over MODEL-INITIATED spawns only, and the qualifier is load-bearing.** The
    /// harness's own children -- the quarantined reader `Engine::condense_batch` builds from
    /// [`CapabilityProfile::quarantined_reader`], and SCOPED-MEMORY section 4's fact extractor --
    /// are built by named constructor and never routed through here, so section 1's level 5 sits
    /// under any of levels 1-4. `Budget.depth` still bounds that path. A claim that the tree is
    /// *"four deep by construction"* is false of `condense_batch` and reads identically either
    /// way.
    pub fn child_of(parent: AgentLevel, d: Disposition) -> Result<AgentLevel, LevelRefusal> {
        use AgentLevel::*;
        use Disposition::*;
        match (parent, d) {
            // Section 1.1: both arms answer "a top-agent" -- but they are DIFFERENT top-agents,
            // and that difference is the whole enforcement.
            (Secretary, Manage) => Ok(TopAgent { manages: true }),
            (Secretary, Work) => Ok(TopAgent { manages: false }),
            (TopAgent { manages: true }, Manage) => Ok(Master),
            (TopAgent { manages: true }, Work) => Ok(Worker),
            // Section 1's table: level 3 is "spawned by a top-agent WITH THE CREATE GRANT".
            (TopAgent { manages: false }, _) => Err(LevelRefusal { parent, wanted: d }),
            (Master, Work) => Ok(Worker),
            (Master, Manage) | (Worker, _) | (ToolSpawned, _) => {
                Err(LevelRefusal { parent, wanted: d })
            }
        }
    }

    /// Which levels may hold the create grant. **One definition, read only by
    /// [`CapabilityProfile::new`]**, and deliberately not consulted a second time at the spawn
    /// gate: if `new` refuses `run` at every level that may not hold it, then by construction no
    /// such profile contains `run`, and a second conjunct could only ever be redundant -- or, if
    /// it ever disagreed with the constructor, silently wrong.
    fn may_hold_create_grant(self) -> bool {
        matches!(self, Self::Secretary | Self::TopAgent { manages: true } | Self::Master)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProfileError {
    #[error(
        "a profile with `reads_untrusted` exposes {count} tool(s). The component that reads \
         untrusted content has no tool access and returns structured analysis only (brief §8.2). \
         This is the structural trifecta break and it is a load-time error, not a warning"
    )]
    QuarantineWithTools { count: usize },

    #[error(
        "a profile with `reads_untrusted` also sets `may_write_memory`. A quarantined reader \
         that can write beliefs is memory laundering with the derivation step built in (§14.6, \
         HP6)"
    )]
    QuarantineMayWriteMemory,

    #[error(
        "a profile with `reads_untrusted` also grants egress. Reading untrusted content and \
         reaching the network are two legs of the trifecta in one component (§8.1)"
    )]
    QuarantineWithEgress,

    #[error(
        "a child profile exposes `{tool}`, which its parent does not have. Privilege must not \
         grow with depth — a spawn is a narrowing, and there is no widening path"
    )]
    WidenedPastParent { tool: ToolId },

    #[error(
        "a profile at level {level:?} exposes `run`. The create grant IS holding `run`, and \
         M3-DESIGN section 1 gives it to Marlowe, to a top-agent that was spawned to MANAGE, and \
         to a master -- nobody else. A worker that can create agents is a level that exists and \
         a rule that does not"
    )]
    CreateGrantNotHeldAtThisLevel { level: AgentLevel },

    #[error(
        "a tool-spawned agent exposes {count} tool(s). Section 1's level 5 is spawned by a TOOL \
         and holds nothing: this is strictly wider than the quarantine check, because \
         SCOPED-MEMORY section 4's fact extractor is a level-5 agent that does not set \
         `reads_untrusted` and would otherwise be constructible with tools"
    )]
    ToolSpawnedWithTools { count: usize },

    // **`MasterHoldsWorkingTool` was here and is gone, 2026-08-31.** It carried §1.2's rule that a
    // master holds no working tools. The human reversed §1.2: the PI is the senior researcher who
    // does the hardest part himself and spawns help, so the refusal it named no longer exists and
    // an error variant nothing can construct is instance #16 with a `thiserror` derive on it. The
    // reasoning and its cost are at the `AgentLevel::Master` arm in `CapabilityProfile::new`.
    #[error(
        "a {level:?} exposes `ask`, and only a top-agent reaches the user. M3-DESIGN section 3.1: \
         escalation is scoped to the top-agent's subtree and a worker can never address Marlowe, \
         so a question raised here has nobody to arrive at. The tool is withheld rather than \
         refused at the call, per section 1.2's rule that a capability is absent from the set \
         rather than forbidden by instruction -- a door visible in the exposed set and locked at \
         every call costs the model the calls it spends discovering that"
    )]
    OnlyATopAgentMayAsk { level: AgentLevel },

    #[error(transparent)]
    Exposure(#[from] ExposureError),
}

/// CONTRACTS.md §5.
///
/// `exposed_tools` is an [`ExposedSet`] rather than a bare `Vec<ToolId>`. That is the pinned
/// field's own invariant — §5 annotates it `INVARIANT: len() <= 12` and §7.2 says the
/// constructor is what enforces it — and the serialized form is identical, because
/// `ExposedSet` is `serde(transparent)` over the same vector. The schema is unchanged; the
/// invariant moved from a comment into the type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CapabilityProfile {
    exposed_tools: ExposedSet,
    egress: EgressPolicy,
    interrupt: InterruptPolicy,
    model_route: ModelRoute,
    /// M3-DESIGN section 1. **Private, and set only through [`CapabilityProfile::new`]** --
    /// including through `serde`, which routes there. Read by `new`'s three level checks and by
    /// `AgentLevel::child_of` via `Engine::spawn`.
    level: AgentLevel,
    may_write_memory: bool,
    reads_untrusted: bool,
}

impl CapabilityProfile {
    /// The only constructor.
    pub fn new(
        exposed_tools: ExposedSet,
        egress: EgressPolicy,
        interrupt: InterruptPolicy,
        model_route: ModelRoute,
        level: AgentLevel,
        may_write_memory: bool,
        reads_untrusted: bool,
    ) -> Result<Self, ProfileError> {
        if reads_untrusted {
            // The pinned check.
            if !exposed_tools.is_empty() {
                return Err(ProfileError::QuarantineWithTools { count: exposed_tools.len() });
            }
            // Two checks the contract does not state. They are strictly narrower than what §5
            // requires — no profile that satisfied the original is rejected here unless it also
            // recombined a trifecta leg — and both close a hole the empty tool set alone does
            // not: `remember` is not the only way to write memory (the loop's MemoryWrite step
            // is), and egress needs no tool at all if the profile grants it.
            if may_write_memory {
                return Err(ProfileError::QuarantineMayWriteMemory);
            }
            if egress != EgressPolicy::DenyAll {
                return Err(ProfileError::QuarantineWithEgress);
            }
        }

        // ---- M3-DESIGN section 1's three level rules, as load-time errors -----------------
        //
        // **The create grant IS holding `run`.** There is no second flag and no second
        // consultation: `may_create_agents` below reads the set, and this is what guarantees
        // that reading the set is enough.
        if exposed_tools.contains(&ToolId::new("run")) && !level.may_hold_create_grant() {
            return Err(ProfileError::CreateGrantNotHeldAtThisLevel { level });
        }
        // **No wildcard arm.** A sixth `AgentLevel` variant is a compile error here rather than
        // a level that quietly inherits whichever neighbour `_` happened to cover -- the
        // #19-safe shape, where the check's coverage is not a list somebody maintains.
        match level {
            AgentLevel::ToolSpawned if !exposed_tools.is_empty() => {
                return Err(ProfileError::ToolSpawnedWithTools { count: exposed_tools.len() });
            }
            AgentLevel::ToolSpawned => {}
            // ── §1.2 IS REVERSED, BY THE HUMAN, 2026-08-31 ────────────────────────────────
            //
            // This arm refused any tool outside `MANAGEMENT_TOOLS`, implementing §1.2's
            // *"masters hold no working tools, structurally"* — *"a master with an `edit` tool
            // will eventually edit. Not because it is disobedient, because it is capable and the
            // work is right there."*
            //
            // **The human overruled it, and the reason is the org chart the ladder actually
            // describes.** A master is not a boss who must be kept away from the work; it is the
            // senior researcher — the most capable model in the tree — who *does the hardest part
            // himself* and spawns assistants and interns because the job is larger than one
            // context. Delegation is leverage, not a job description. Forbidding the AAII-52
            // model from touching the work spends the best model in the ladder on coordination.
            //
            // **What that costs, recorded rather than argued away.** §1.2's real argument was
            // containment, not hierarchy: a master reads every worker's report, so it holds the
            // most attacker-exposed context in the tree, and `edit`/`bash` there puts tool access
            // exactly where exposure concentrates. That is `SECURITY-AUDIT.md`'s finding #1 — a
            // child's note crosses into the parent at `AgentInferred`, **above**
            // `blocks_composed_targets`'s threshold, so layer 3 does not catch it. This decision
            // does not create that hole; it moves the product onto it, and it is why red-team
            // pass 2 matters more after this change rather than less.
            //
            // **Nothing replaces it for WORKING tools, and that is deliberate.** A master holding
            // no `run` simply cannot spawn — `may_create_agents` reads the exposed set — which is
            // already a coherent state needing no new error. Adding *"a master MUST hold `run`"*
            // here would be inventing a rule nobody asked for in the same edit that removes one.
            //
            // # ONE REFUSAL SURVIVES, AND IT NEARLY DIED BY ACCIDENT
            //
            // `MANAGEMENT_TOOLS` was doing **two** jobs under one whitelist: it withheld working
            // tools (§1.2, now reversed) *and* it withheld `ask`, which is a **different decision
            // the human took the same day** — *"only top-agents get the ask"*, because §3.1 says
            // only a top-agent reaches the user. Deleting the whitelist would have dropped the
            // second rule silently while removing the first.
            //
            // So `ask` is refused here **by name**, and the reason is the level's reach rather
            // than its job description: a `Master` is level 3, one below the top-agent, and a
            // question from it has nobody to arrive at. §1.2's own principle decides the shape —
            // *"the tool is absent from the set, not forbidden by instruction"* — because
            // `Engine`'s `Ask` refusal would otherwise leave a master holding a door that is
            // visible in its exposed set, described in its schema, and locked at every call.
            AgentLevel::Master if exposed_tools.contains(&ToolId::new("ask")) => {
                return Err(ProfileError::OnlyATopAgentMayAsk { level });
            }
            AgentLevel::Master => {}
            // Section 1's table says the secretary holds a "full conversational set", and
            // `interactive()` currently holds `bash`, `edit`, `write` and `web`. **Whether
            // section 1.2's structural rule extends upward to level 1 is a question M3-DESIGN
            // does not answer**, and answering it with a `_` is how a decision gets made by
            // nobody -- so the arm is NAMED and empty, and the question is ADR-064 section 9
            // item 4, the human's.
            AgentLevel::Secretary => {}
            // Per-type sets are section 1.3 configuration, not a constructor rule.
            AgentLevel::TopAgent { .. } | AgentLevel::Worker => {}
        }

        Ok(Self {
            exposed_tools,
            egress,
            interrupt,
            model_route,
            level,
            may_write_memory,
            reads_untrusted,
        })
    }

    /// §8.2's quarantined reader: reads untrusted content, has **no tool access**, and returns
    /// structured analysis only. The one profile ADR-002 keeps a kernel sandbox for.
    pub fn quarantined_reader() -> Self {
        Self::new(
            ExposedSet::empty(),
            EgressPolicy::DenyAll,
            InterruptPolicy::Unattended,
            ModelRoute::Worker,
            // Section 1's level 5: spawned by a TOOL. `AgentLevel::child_of` can never return
            // this, so the only way to be one is to be built here.
            AgentLevel::ToolSpawned,
            false,
            true,
        )
        .expect("the quarantined reader is the shape the invariant describes")
    }

    /// ARCHITECTURE §2.6 — consolidation is a run on the one loop with a memory-only profile
    /// and a hard step budget. It is not a second loop.
    pub fn consolidation() -> Self {
        Self::new(
            // `done` is gone: a run ends when the model replies without calling a tool (M2 C2e).
            // Exposing a tool whose only job was ending would advertise a control token the loop
            // no longer reads.
            ExposedSet::new(vec![ToolId::new("recall"), ToolId::new("remember")])
                .expect("two tools"),
            EgressPolicy::DenyAll,
            InterruptPolicy::Unattended,
            ModelRoute::Worker,
            // It does the work itself and creates nothing. Not `ToolSpawned`: its set is
            // non-empty, and level 5 holds no tools.
            AgentLevel::Worker,
            true,
            false,
        )
        .expect("consolidation reads nothing untrusted")
    }

    /// The interactive coding profile: all eleven, no egress until the user grants it.
    pub fn interactive() -> Self {
        // **Only what can actually run.** `recall` and `use` are registered and unimplemented —
        // exposing a tool the host cannot execute gave the model something it could see, call
        // correctly, and never run, which is what made "check the weather" produce a model
        // arguing with an error it could not read.
        //
        // **`web` came back in M2 C2f**, which is the rule working rather than an exception to it:
        // it has an executor now (`marlowe-exec`, fetch-only over `marlowe-net`), so
        // `verify_every_exposed_tool_is_runnable` permits it. It was removed and restored by the
        // same check, without anyone having to remember either time.
        // **`recall` joined in M2 Session D**, by the same rule that brought `web` back in C2f: it
        // has an executor now (`crate::recall` in the daemon, over the belief store), so
        // `verify_every_exposed_tool_is_runnable` permits it. Exposing it is what makes memory
        // reachable at all — auto-injection is gated at 10% coverage and withholds unmatured
        // beliefs entirely, so without this tool a memory written a minute ago is unreachable by
        // any path. §5.5: *"recall recovered by making the agent's explicit memory search tool
        // excellent."*
        //
        // **`use` joined in M2 C3 (ADR-051)**, by the identical rule a third time. It has an
        // executor now — `marlowe_daemon::skills::SkillTools`, over the installed skill registry —
        // so `verify_every_exposed_tool_is_runnable` permits it. It had been
        // registered-and-unrunnable since Session A, and exposing it is what makes an installed
        // skill reachable at all: a skills library the model cannot see is a directory.
        //
        // Three tools have now been restored by this guard and none by anybody remembering to.
        // That is the property: the exposed set is DERIVED from what is runnable rather than
        // maintained beside it.
        // **`write` joined when `edit` was split in two.** `edit` used to carry both modes,
        // separated by an optional parameter, and a model asked to write a file reached for the
        // shell instead -- no builtin was named for the verb. Splitting them is only useful if
        // BOTH are exposed, which is this line.
        let tools = [
            "read", "write", "edit", "glob", "grep", "bash", "web", "recall", "use", "ask",
            "remember", "run",
        ]
        .iter()
        .map(|t| ToolId::new(*t))
        .collect();
        Self::new(
            ExposedSet::new(tools).expect("twelve fits in fourteen"),
            // **ADR-032 §3.1: nothing reachable by default, each host by human approval.**
            //
            // Not `DenyAll`, which is structural and unwidenable — the quarantined reader holds
            // that, and §5's narrowing rule depends on it. Not `Allow { hosts }`, which is a list
            // somebody guessed at in advance. The set starts **empty**, so brief §8's
            // allowlist-by-default holds with an empty default rather than a `*`, and it grows one
            // host at a time by a person who was shown the blast radius.
            EgressPolicy::AllowApproved { granted: Vec::new() },
            InterruptPolicy::Interruptible,
            ModelRoute::Orchestrator,
            // Level 1. Marlowe himself, and the only level that holds `run` without having been
            // spawned to manage.
            AgentLevel::Secretary,
            true,
            false,
        )
        .expect("the interactive profile reads nothing untrusted")
    }

    /// [`Self::interactive`] plus the tools an installed MCP server contributes. ADR-052 §5.
    ///
    /// # Why widening exists here, and the one other place it now exists
    ///
    /// `narrowed` has no counterpart on purpose: a **spawn** may only narrow, because privilege
    /// must not grow with depth, and `WidenedPastParent` enforces it. This is not that. It is how
    /// a top-level profile is *built* — the exposed set has always been chosen at construction —
    /// and it takes no parent, so there is nothing for it to grow past. A child of the profile
    /// this returns is still narrowed against it, unchanged.
    ///
    /// **This heading used to read *"and nowhere else"*, and since ADR-032 §3.1 was wired that is
    /// false.** [`Self::grant_egress_host`] is the second, and it is a *runtime* widening rather
    /// than a construction-time one. It is bounded differently and deliberately: it moves one
    /// field, only on `AllowApproved`, only by a validated host, only after a human said yes. A
    /// list that omits it is the failure the §13 enforcement table produced two milestones
    /// running — the guard held and the documentation went silent.
    ///
    /// # The budget refuses, and the message says which tool to drop
    ///
    /// The interactive set is eleven of ARCHITECTURE §5's thirteen, so **two MCP tools fit and a
    /// third does not**. The cap moved with ADR-058 precisely so that splitting `write` out of
    /// `edit` did not take that two down to one: a fix to Marlowe's own surface must not be paid
    /// for out of a user's server allowance. That is a real constraint on a real product and it refuses at load rather than
    /// silently dropping the overflow: a server whose third tool quietly vanished would look like
    /// a server with a broken tool. `ExposureError::TooMany` carries the count and the remedy.
    ///
    /// A user who needs more MCP tools than that has to give something up, and it should be their
    /// choice which — so the refusal names the budget rather than this function picking a victim.
    pub fn interactive_with(extra: Vec<ToolId>) -> Result<Self, ProfileError> {
        let base = Self::interactive();
        let mut tools: Vec<ToolId> = base.exposed_tools.iter().cloned().collect();
        tools.extend(extra);
        Ok(Self::new(
            ExposedSet::new(tools)?,
            base.egress,
            base.interrupt,
            base.model_route,
            // Still Marlowe: an MCP server contributes tools, not a position in the tree.
            base.level,
            base.may_write_memory,
            base.reads_untrusted,
        )?)
    }

    pub fn exposed_tools(&self) -> &ExposedSet {
        &self.exposed_tools
    }
    pub fn egress(&self) -> &EgressPolicy {
        &self.egress
    }
    pub fn interrupt(&self) -> InterruptPolicy {
        self.interrupt
    }
    pub fn model_route(&self) -> ModelRoute {
        self.model_route
    }
    pub fn level(&self) -> AgentLevel {
        self.level
    }

    /// **The create grant IS holding `run`.** [`CapabilityProfile::new`] guarantees no profile
    /// at a level that may not hold it contains it, so there is nothing else to consult and
    /// there is deliberately no `level.may_hold_create_grant()` conjunct here: two definitions
    /// of *may this run create agents* is this project's most-logged shape, and the redundant
    /// one is the one that goes silently wrong when a sixth level is added to one and not the
    /// other.
    ///
    /// Read by `Engine::spawn`, as its **first** refusal.
    pub fn may_create_agents(&self) -> bool {
        self.exposed_tools.contains(&ToolId::new("run"))
    }
    pub fn may_write_memory(&self) -> bool {
        self.may_write_memory
    }
    pub fn reads_untrusted(&self) -> bool {
        self.reads_untrusted
    }

    /// **ADR-032 §3.1: record a host a human approved, for the remainder of this run.**
    ///
    /// The one mutable route into this struct, and it can do exactly one thing.
    ///
    /// # Why not a `&mut EgressPolicy` accessor
    ///
    /// A broad mutable accessor routes around the validating constructor, which is instance #12
    /// and has already bitten this project once through `Deserialize`. There is no `egress_mut`,
    /// there is no `set_egress`, and this method takes a parsed [`Host`] rather than a string, so
    /// the only reachable state change is *`AllowApproved`'s set grew by one validated host*.
    ///
    /// # Why the set lives on the PROFILE and not on the `Run`
    ///
    /// The `Run` was the tempting home — it already carries ADR-023's latched trust floor, which
    /// is a per-run narrowing of exactly this kind, and it has no validating constructor to route
    /// around. Three things decided against it, and the third is the one that matters:
    ///
    /// 1. **The adjudicator reads `run.profile.egress()`.** A set held beside the policy would
    ///    have to be merged into one at the call site, which is a second definition of *what this
    ///    run may reach* — the two-sides-silently-disagree shape.
    /// 2. **Child propagation already exists and is already right.** `Engine::spawn` hands a
    ///    quarantined child `DenyAll` and every other child a clone of the parent's policy. A
    ///    `Run`-side set would need that decision written a second time, in a file where getting
    ///    it wrong is silent.
    /// 3. **`CapabilityProfile::new` holds the invariant `reads_untrusted ⟹ DenyAll`
    ///    (`QuarantineWithEgress`), and this widening provably cannot break it** — because
    ///    [`EgressPolicy::grant`] is a no-op on every variant except `AllowApproved`, so a
    ///    quarantined reader's `DenyAll` stays `DenyAll` no matter what is granted to it. Hold
    ///    the set on the `Run` and consult it *beside* the policy and that invariant is bypassed
    ///    rather than enforced: the profile would still read `DenyAll` while the run reached the
    ///    network, and every existing test of the quarantine would stay green. **The widening
    ///    belongs inside the type that holds the invariant it could otherwise violate.**
    ///
    /// The no-op on non-`AllowApproved` policies is therefore load-bearing rather than defensive,
    /// and it is asserted through the loop in
    /// `marlowe-loop/tests/egress_grant.rs::a_deny_all_run_cannot_be_widened_by_an_approval`.
    pub fn grant_egress_host(&mut self, host: &marlowe_permission::Host) {
        self.egress.grant(host);
    }

    /// Narrow a profile for a child. **Widening is not offered** -- there is no method that
    /// hands a child a tool the parent did not have, so privilege cannot grow with depth.
    ///
    /// `level` is taken **explicitly** rather than inherited. A child is at a different position
    /// in the organisation from its parent by definition, and inheriting one would be the
    /// silent-default family at a security invariant: a narrowed `Secretary` would still be a
    /// `Secretary`, and would still be allowed to hold `run`.
    ///
    /// The model route is likewise the caller's: a narrowing of the tool set says nothing about
    /// which model serves the run.
    ///
    /// **This method has zero production callers** (`Engine::spawn` builds the child profile
    /// directly, so that the load-time refusal happens before anything is constructed), so both
    /// of those changes are currently unobservable outside tests. Said here rather than implied,
    /// because a reader looking for where levels are decided must not stop at this function.
    pub fn narrowed(
        &self,
        tools: Vec<ToolId>,
        level: AgentLevel,
        model_route: ModelRoute,
    ) -> Result<Self, ProfileError> {
        for t in &tools {
            if !self.exposed_tools.contains(t) {
                return Err(ProfileError::WidenedPastParent { tool: t.clone() });
            }
        }
        Self::new(
            ExposedSet::new(tools)?,
            self.egress.clone(),
            self.interrupt,
            model_route,
            level,
            self.may_write_memory,
            self.reads_untrusted,
        )
    }
}

/// Deserialization routes through [`CapabilityProfile::new`].
///
/// Without this the invariant would hold for every profile built in code and fail for every
/// profile read from a file — which is the only place an attacker-shaped one could come from.
impl<'de> Deserialize<'de> for CapabilityProfile {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            exposed_tools: ExposedSet,
            #[serde(default)]
            egress: EgressPolicy,
            interrupt: InterruptPolicy,
            model_route: ModelRoute,
            /// **No `#[serde(default)]`, and that is the decision rather than an omission.**
            /// A default here would silently restore a master holding working tools -- the
            /// "defaults that make a mismatch unobservable" family, at a security invariant.
            /// The cost is real and it is stated: a checkpoint written before M3 Session C
            /// fails to resume with a named serde error rather than resuming as something the
            /// constructor would have refused.
            level: AgentLevel,
            may_write_memory: bool,
            reads_untrusted: bool,
        }
        let r = Raw::deserialize(d)?;
        CapabilityProfile::new(
            r.exposed_tools,
            r.egress,
            r.interrupt,
            r.model_route,
            r.level,
            r.may_write_memory,
            r.reads_untrusted,
        )
        .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one_tool() -> ExposedSet {
        ExposedSet::new(vec![ToolId::new("read")]).unwrap()
    }

    #[test]
    fn a_spawn_that_reads_untrusted_with_any_tool_fails_at_load_time() {
        // CONTRACTS §5's pinned load-time error, and M2's first report item.
        let e = CapabilityProfile::new(
            one_tool(),
            EgressPolicy::DenyAll,
            InterruptPolicy::Unattended,
            ModelRoute::Worker,
            AgentLevel::ToolSpawned,
            false,
            true,
        )
        .unwrap_err();
        assert_eq!(e, ProfileError::QuarantineWithTools { count: 1 });
        assert!(
            e.to_string().contains("trifecta"),
            "the refusal must name what it is protecting: {e}"
        );
    }

    #[test]
    fn the_empty_tool_set_is_the_only_quarantined_shape() {
        // ...and the empty set alone is not sufficient. Both extra refusals close a leg that
        // needs no tool.
        assert!(CapabilityProfile::new(
            ExposedSet::empty(),
            EgressPolicy::DenyAll,
            InterruptPolicy::Unattended,
            ModelRoute::Worker,
            AgentLevel::ToolSpawned,
            false,
            true,
        )
        .is_ok());

        assert_eq!(
            CapabilityProfile::new(
                ExposedSet::empty(),
                EgressPolicy::DenyAll,
                InterruptPolicy::Unattended,
                ModelRoute::Worker,
                AgentLevel::ToolSpawned,
                true, // may_write_memory
                true,
            )
            .unwrap_err(),
            ProfileError::QuarantineMayWriteMemory
        );

        assert_eq!(
            CapabilityProfile::new(
                ExposedSet::empty(),
                EgressPolicy::allow(&["example.com"]),
                InterruptPolicy::Unattended,
                ModelRoute::Worker,
                AgentLevel::ToolSpawned,
                false,
                true,
            )
            .unwrap_err(),
            ProfileError::QuarantineWithEgress
        );
    }

    #[test]
    fn deserialization_cannot_bypass_the_invariant() {
        // The path that matters: a profile arriving from a file. Without a constructor-routed
        // Deserialize, this is where a quarantined reader with tools would be born, and every
        // in-code test would still pass.
        let json = r#"{
            "exposed_tools": ["read"],
            "egress": "deny_all",
            "interrupt": "unattended",
            "model_route": "worker",
            "level": "tool_spawned",
            "may_write_memory": false,
            "reads_untrusted": true
        }"#;
        let err = serde_json::from_str::<CapabilityProfile>(json).unwrap_err().to_string();
        assert!(err.contains("trifecta"), "{err}");
    }

    #[test]
    fn the_named_profiles_are_the_capability_profiles_not_variants() {
        let q = CapabilityProfile::quarantined_reader();
        assert!(q.reads_untrusted() && q.exposed_tools().is_empty() && !q.may_write_memory());
        assert_eq!(q.level(), AgentLevel::ToolSpawned);
        assert!(!q.may_create_agents());

        let c = CapabilityProfile::consolidation();
        assert!(c.may_write_memory() && !c.reads_untrusted());
        assert_eq!(c.level(), AgentLevel::Worker);
        assert!(!c.may_create_agents());
        // Two since `done` was removed — consolidation recalls and remembers, and ends by replying.
        //
        // **`recall` has no executor yet**, so this profile would fail
        // `verify_every_exposed_tool_is_runnable` against `FileSystemTools`. It is left as
        // declared because consolidation is not wired until M2 D, and the guard firing at that
        // point is the guard working — not a surprise to design around now.
        assert_eq!(c.exposed_tools().len(), 2);

        let i = CapabilityProfile::interactive();
        assert_eq!(i.level(), AgentLevel::Secretary);
        assert!(i.may_create_agents(), "Marlowe holds the create grant: it IS holding `run`");
        // **Eleven as of the write/edit split, and ONE under the budget.** `web` rejoined in C2f,
        // `recall` in Session D, `use` in C3 -- each at the moment it gained an executor, added by
        // the same guard without anyone having to remember. That is the point of
        // `verify_every_exposed_tool_is_runnable`: the exposed set is derived from what is
        // runnable rather than maintained beside it.
        //
        // **The eleventh is `write`, and it did NOT cost an MCP slot.** Against the old cap of
        // twelve it would have taken a server from two tools to one; ADR-058 moved the cap to
        // thirteen instead, because a fix to Marlowe's own surface must not be paid for out of a
        // user's server allowance. `composition_root.rs` asserts the two slots rather than the
        // total, so the next builtin that would eat one fails the suite.
        assert_eq!(i.exposed_tools().len(), 12);
        assert!(
            i.exposed_tools().iter().any(|t| t.as_str() == "recall"),
            "recall is what makes a memory written a minute ago reachable at all: auto-injection \
             withholds unmatured beliefs and covers 10% of queries even after they mature"
        );
        assert!(
            i.exposed_tools().iter().any(|t| t.as_str() == "use"),
            "`use` is what makes an installed skill reachable: nothing else loads a SKILL.md, \
             and a skills library the model cannot see is a directory"
        );
        assert_eq!(
            *i.egress(),
            EgressPolicy::AllowApproved { granted: Vec::new() },
            "the set starts EMPTY: egress is granted per host by a human, never assumed. This is              not DenyAll — that is structural and unwidenable, and the quarantined reader holds it"
        );
        assert!(i.egress().may_ask(), "an ungranted host is a question here, not a refusal");
        assert!(
            !CapabilityProfile::quarantined_reader().egress().may_ask(),
            "the quarantined reader may never ask its way onto the network"
        );
    }

    #[test]
    fn a_child_cannot_be_widened_past_its_parent() {
        let narrow = CapabilityProfile::new(
            ExposedSet::new(vec![ToolId::new("read"), ToolId::new("grep")]).unwrap(),
            EgressPolicy::DenyAll,
            InterruptPolicy::Unattended,
            ModelRoute::Orchestrator,
            AgentLevel::TopAgent { manages: true },
            false,
            false,
        )
        .unwrap();
        assert!(
            narrow
                .narrowed(vec![ToolId::new("read")], AgentLevel::Worker, ModelRoute::Worker)
                .is_ok()
        );
        assert!(
            narrow
                .narrowed(vec![ToolId::new("bash")], AgentLevel::Worker, ModelRoute::Worker)
                .is_err(),
            "privilege must not grow with depth"
        );
    }
}

/// **A tool in the exposed set with no executor is unrepresentable, not merely wrong.**
///
/// # The fifth instance
///
/// The model can see the tool, call it correctly, and get back `has no executor in this build` —
/// a failure it cannot interpret and cannot route around. It has happened five times:
///
/// | Tool | What the model got |
/// |---|---|
/// | `done` (M2 C2b) | routed to the tool host, which had no executor; the model retried for 155 s |
/// | `web` | same, and it is what made "check the weather" unanswerable |
/// | `recall` | same |
/// | `use` | same |
///
/// Every one of them passed every unit test, because each half was right in isolation: the
/// registry registered the tool, the profile exposed it, and the host correctly reported that it
/// could not run it. **Nothing owned the seam.** This does.
///
/// The check runs where the two sides meet — a profile and a host — and it is a **load-time
/// error**. A daemon that starts and then fails every third tool call presents as a broken model;
/// a daemon that refuses to start names the tool and the fix in one line.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "the capability profile exposes {} the tool host cannot run: {}.
     A tool the model can see and call but never execute returns an error it cannot interpret.      Either give it an executor or stop exposing it.
     Loop-control tools ({}) are exempt: ARCHITECTURE §3 handles them as ModelStep variants.",
    if .0.len() == 1 { "a tool".to_string() } else { format!("{} tools", .0.len()) },
    .0.join(", "),
    crate::driver::CONTROL_TOOLS.join(", ")
)]
pub struct UnrunnableTools(pub Vec<String>);

/// Verify that every exposed tool can actually be reached.
///
/// Call this once, at startup, before a model is ever offered the set.
pub fn verify_every_exposed_tool_is_runnable(
    exposed: &ExposedSet,
    host: &dyn crate::driver::ToolHost,
) -> Result<(), UnrunnableTools> {
    let runnable: std::collections::BTreeSet<String> =
        host.executes().into_iter().map(|t| t.as_str().to_string()).collect();
    let unrunnable: Vec<String> = exposed
        .iter()
        .map(|t| t.as_str().to_string())
        .filter(|name| {
            !runnable.contains(name) && !crate::driver::CONTROL_TOOLS.contains(&name.as_str())
        })
        .collect();
    if unrunnable.is_empty() {
        Ok(())
    } else {
        Err(UnrunnableTools(unrunnable))
    }
}
