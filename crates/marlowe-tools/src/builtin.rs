//! ADR-006 — the eleven model-visible tools, with their manifests.
//!
//! `bash · read · write · edit · glob · grep · web · recall · remember · use · run · ask`
//!
//! Eleven against a budget of twelve. **The spare slot is for a situational tool a profile
//! adds, not for a twelfth permanent one.**
//!
//! # The consequence column, and why each is what it is
//!
//! | Tool | Consequence | Reason |
//! |---|---|---|
//! | `read` `glob` `grep` `recall` `ask` | `Inert` | Pure reads and escalations. §9 exempts `Inert` from *target-provenance* checking on the stated grounds that following a link found on a page is how research works. **Path scoping still applies** — that is a different check, and it applies at every level. |
//! | `web` | `Inert` | Deliberate, and it is §9's own example. Containment is three non-kernel mechanisms: the result returns `UntrustedContent`, it returns by reference, and egress allowlisting closes the exfiltration leg. ADR-002 records that if any one weakens, this exemption is revisited rather than inherited. |
//! | `edit` `use` | `Reversible` | §9 checks `Reversible` targets, and both are here for that reason. A workspace write is a durable channel into a later run's context (§8.3's sandbox-boundary-redefinition class); a skill load chosen by untrusted content is supply-chain steering. |
//! | `remember` `run` | `Consequential` | A memory write is the highest-privilege operation in the system (§8.2). A spawn commits budget and acts through a child. |
//! | `bash` | `Irreversible` | The declared level is the **ceiling a tool can reach**, and an arbitrary shell command can reach anything. Refining it per-command means parsing the command to decide whether it is safe, which is the Cursor CVE exactly — an allowlist that auto-approved the commands the attacker needed. Any such refinement is a separate argued change with its own ADR, never a convenience. |
//!
//! # `hosts` in a manifest is a declared maximum, not a grant
//!
//! The manifest says what a tool may *ever* reach; `EgressPolicy` on the run's
//! `CapabilityProfile` says what this run *does* grant, deny-by-default. The effective set is
//! the intersection. `web` therefore declares `*` — it is the general-purpose fetch tool and
//! its audit surface is honestly "anywhere" — and the run's policy is where the real
//! allowlist lives. A connector declares its provider's hosts and nothing else.

use crate::manifest::{
    load, CapabilityManifest, ConsequenceLevel, LoadError, ManifestProvenance, ParamType,
    RawManifest, RawParamSpec, ToolId,
};
use crate::registry::{Description, ToolRegistration, ToolRegistry, Transport};
use crate::summary::SummarySpec;
use crate::ArgumentRole;

/// The eleven ids, in ADR-006's order. Used by the HP10 budget test and by profile
/// construction, so the list exists once.
pub const BUILTIN_TOOLS: [&str; 12] = [
    "bash", "read", "write", "edit", "glob", "grep", "web", "recall", "remember", "use", "run",
    "ask",
];

// **`MANAGEMENT_TOOLS` LIVED HERE AND IS GONE, 2026-08-31.** It was M3-DESIGN section 1.2's
// management set -- the only tools a `Master` was permitted to hold -- read by
// `CapabilityProfile::new`'s `Master` arm and by nothing else.
//
// The human reversed section 1.2: the PI is the senior researcher who does the hardest part himself
// and spawns help, not a boss kept away from the work. With the working-tool refusal gone the
// constant had **no reader at all**, which is instance #16 -- a declared control nothing consults
// -- so it is deleted rather than left as an enumeration of a policy that no longer exists.
// `ProfileError::MasterHoldsWorkingTool` went with it.
//
// **One of its two jobs survives and did not survive by accident.** The same whitelist also
// withheld `ask`, which is a separate decision taken the same day -- only a top-agent reaches the
// user. That is now `ProfileError::OnlyATopAgentMayAsk`, refused by name at the `Master` arm, so
// deleting this constant did not silently drop it.

/// The workspace-relative glob every filesystem tool declares.
///
/// `.` is resolved against the run's workspace root by path scoping, which is the only
/// component that may turn a declaration into a decision. A manifest holding an absolute path
/// would be a manifest that means something different on another machine.
const WORKSPACE: &str = "./**";

/// **The ONE constructor, and the four undocumented ones are deleted rather than deprecated.**
///
/// `param`, `target_req`, `target_opt`, `payload_req` and `payload_opt` all built a `RawParamSpec`
/// with `description: None`, and a parameter with no description does not reach the model blank --
/// `param_description` generates a sentence from its type and arity. For a `Text` payload that
/// sentence is **"Optional. text."**: it looks like documentation, fills the slot documentation
/// would fill, and says nothing. Fifteen of eighteen builtin parameters were in that state, and
/// live on 2026-08-26 the model called `read` with neither `path` nor `ref` -- both optional, both
/// described identically and uselessly.
///
/// A test catches that (`every_builtin_parameter_says_what_it_is_for`), and a test is the weaker
/// half. **Removing the convenient way to declare an undocumented parameter is the stronger one**,
/// which is the same reasoning `ExposedSet::empty()` and `depth: 0` use: withhold the capability
/// structurally rather than checking for its misuse afterwards. A `RawParamSpec` literal can still
/// be written by hand with `description: None`, and that is fine -- it is visibly deliberate,
/// which "forgot to add a description" never was.
///
/// **Role and arity stay two arguments because they are two independent questions.**
/// `Target`/`Payload` answers *may untrusted content shape this* (§9); `required` answers *does
/// the executor need it*. They were one switch until M2, and the eleven mismatches that produced
/// are recorded in [`marlowe_tools::ParamSpec::required`].
fn documented(
    name: &str,
    role: ArgumentRole,
    ty: ParamType,
    required: bool,
    description: &str,
) -> RawParamSpec {
    RawParamSpec {
        name: name.to_string(),
        role: Some(role),
        ty,
        required,
        description: Some(description.to_string()),
    }
}

fn manifest(
    tool: &str,
    consequence: ConsequenceLevel,
    paths: &[&str],
    hosts: &[&str],
    params: Vec<RawParamSpec>,
) -> Result<CapabilityManifest, LoadError> {
    load(
        RawManifest {
            tool: ToolId::new(tool),
            paths: paths.iter().map(|s| (*s).to_string()).collect(),
            hosts: hosts.iter().map(|s| (*s).to_string()).collect(),
            creds: vec![],
            consequence: Some(consequence),
            params,
        },
        // First-party, because these are built here in code. Everything arriving as bytes
        // loads as `ThirdParty` — see `manifest`'s header.
        ManifestProvenance::FirstParty,
    )
}

/// One builtin's registration: manifest, description, and its §8 one-line summary spec.
fn registration(
    tool: &'static str,
    description: &'static str,
    verb: &'static str,
    inline_threshold_bytes: u64,
    consequence: ConsequenceLevel,
    paths: &[&str],
    hosts: &[&str],
    params: Vec<RawParamSpec>,
) -> Result<ToolRegistration, LoadError> {
    let transport = Transport::Builtin;
    Ok(ToolRegistration {
        id: ToolId::new(tool),
        manifest: manifest(tool, consequence, paths, hosts, params)?,
        description: Description::new(description),
        summary: SummarySpec::new(verb, inline_threshold_bytes),
        transport,
    })
}

/// What `bash` actually is, per platform, in the words the model reads.
///
/// Split by `cfg` on the same condition `marlowe_exec::spawn_shell` splits on, so the two cannot
/// disagree about which interpreter runs. **Naming the network is deliberate**: the absence of any
/// statement was read by a live session as evidence that there was none, and it reported a
/// non-existent egress boundary rather than a quoting problem.
// **One description, because there is now one shell.** This was `cfg`-split while Windows
// ran `cmd /C` and everything else ran `sh -c`; the note that lived here said a
// description naming two shells "makes the model guess which one it has", which was right.
// The split is gone rather than left as two strings that can drift, because the answer is
// the same on both: it is bash. See `marlowe_exec::spawn_shell`.
const SHELL_DESCRIPTION: &str = "Run one command line through bash — really bash, including on Windows, where it is Git Bash. So `ls`, `head`, `tail`, `sed`, `awk`, `sort`, `&&`, `|`, `2>/dev/null` and forward slashes all work as you expect. Paths are POSIX: `docs/design`, and the workspace is the working directory. **Reach for a real tool first** — `glob` lists files by name, `grep` searches inside them, `read`, `write` and `edit` handle files, and none of those interrupt the user, whereas EVERY call to this one STOPS AND ASKS THEM for approval, so a wrong guess costs them a prompt. Each call is a fresh shell: no `cd` or variable carries over, use `cwd`. It reaches the network normally. For something the user already told Marlowe, try `recall` first.";

/// Every builtin, registered. **Registration, not exposure** — a profile still selects ≤12.
pub fn builtin_registry() -> Result<ToolRegistry, LoadError> {
    use ConsequenceLevel::*;
    use ParamType::*;

    let mut r = ToolRegistry::new();
    let regs = [
        registration(
            "bash",
            // Every clause is checkable: `spawn_shell` runs one `cmd /C` (or `sh -c`) per call, so
            // nothing carries over; `adjudicate` returns `NeedsApproval` for `Irreversible`
            // unconditionally, before any tier comparison, so the approval is not tier-dependent.
            // The last sentence is routing advice, not a capability claim — a shell CAN read files,
            // which is exactly why a model reaches for it when the answer is in memory instead.
            //
            // **THE TOOL IS CALLED `bash` AND ON WINDOWS IT IS NOT BASH.** `spawn_shell` is
            // `cfg`-split: `cmd /C` on Windows, `sh -c` elsewhere. The name was the whole of what
            // the model had to go on, and the name is wrong on this platform — so a model writes
            // `'single quotes'`, `grep`, `&&`, `2>/dev/null`, and gets failures that look like
            // anything but a different interpreter. A real session read a string of them as *"the
            // harness has no network egress"* and reported a security boundary that does not
            // exist: `bash` reaches the network exactly as any other process on this machine does
            // — measured, `curl` to arxiv.org returns 200 — and no `EgressPolicy` is consulted on
            // this path at all. ADR-049 §4.
            //
            // The interpreter is therefore **named**, per platform, in the text the model reads.
            // [`SHELL_DESCRIPTION`] is `cfg`-selected rather than one string mentioning both,
            // because a description listing two shells makes the model guess which one it has.
            SHELL_DESCRIPTION,
            "bash",
            2_048,
            Irreversible,
            &[WORKSPACE],
            &[],
            // `command` is a Target and not a Payload. It is not body content that a tool
            // happens to carry — it *is* the action, and untrusted content choosing it is the
            // whole attack.
            vec![
                documented(
                    "command",
                    ArgumentRole::Target,
                    Text,
                    true,
                    "One command line, exactly as you would type it at a bash prompt. Quoting reaches the shell verbatim, so quote normally. Chain with `&&`, `|` and `;` within this one call rather than expecting several. Killed after 120 seconds, and a killed command says so at the end of its output.",
                ),
                documented(
                    "cwd",
                    ArgumentRole::Target,
                    ParamType::Path,
                    false,
                    "Directory to run in, workspace-relative. Defaults to the workspace root, which is usually what you want -- pass this only when the command itself depends on where it starts.",
                ),
            ],
        ),
        registration(
            "read",
            // `range`'s format was undocumented anywhere the model could see it: the generated
            // parameter description for a `Text` payload is "Optional. text." `slice_lines` splits
            // on `-`, parses both sides, and takes `skip(a-1).take(b-a+1)` — 1-based and inclusive.
            "Read a file in the workspace by workspace-relative `path`, OR a fetched document by `ref` (the id `web` returns). **Every line of a `path` read comes back prefixed with its line number, right-aligned in 6 characters, followed by a tab, then the line itself.** Those are the file's own line numbers, absolute, whatever window you asked for — so a `grep` hit at `src/x.rs:512` is read with `range` \"500-540\". **The prefix is NOT part of the file**: strip it from any text you hand to `edit`, which matches the file byte for byte. An unnumbered line in square brackets is the harness speaking, not file content. A `ref` read is NOT numbered. **A read returns a WINDOW**: at most 2000 lines, and at most 32KB counted WITH the prefixes, so a numbered window is often fewer than 2000. When more remains, the result says which lines you got and which range to ask for next, in the file's own numbers. `range` is `first-last`, 1-based and inclusive, e.g. \"20-60\". A file that does not exist is REFUSED with a message saying so, so a result of `0 lines · 0 B` means the file is there and is empty.",
            "read",
            8_192,
            Inert,
            &[WORKSPACE],
            &[],
            // **`path` became optional when `ref` arrived, and ADR-034's rule is preserved rather
            // than broken.** That rule is *a parameter the executor cannot run without is
            // required*; `read` now has two ways to name its subject, so neither one alone is
            // structurally mandatory. A call supplying neither is refused by the executor with a
            // message naming both — the one case ADR-034 exists to prevent is a call that is
            // schema-valid and can NEVER succeed, and that is not this.
            //
            // `ref` is a **Target**, not a payload: it selects which document is read, and §9's
            // whole point is that untrusted content may not choose a target.
            //
            // **AND THE CHECK DOES NOT FIRE ON IT. Audit finding A6.** An earlier version of this
            // comment ended *"a ref composed out of a fetched page's own text would carry that
            // page's class and be blocked by the same check as any other target"*. That is false in
            // this build. The target-provenance loop in `adjudicate` is guarded by
            // `if manifest.consequence() > ConsequenceLevel::Inert`, and `read` is `Inert` — the
            // loop never runs, so `role_of("ref")` has no reader on any enforcing path. A test
            // asserting the role would be green on a build where the check cannot execute, which is
            // the sixteenth-instance family exactly.
            //
            // What actually holds today: `read_ref` returns `UntrustedContent`, so layer 1 fires on
            // the content, and the store's ids are BLAKE3 and unguessable. What is NOT covered is
            // **steering** — text surviving into a condensed summary can name which of N refs the
            // parent dereferences next, and no provenance check sees that choice. The `Inert`
            // branch names its own revisit condition ("egress allowlisting closes the exfiltration
            // leg"), and layer 4 is approved-but-not-shipped, so that condition is already unmet.
            //
            // Left as a documented gap rather than silently widened: changing it is a §13 boundary
            // change and needs a `DECISIONS.md` entry.
            vec![
                documented(
                    "path",
                    ArgumentRole::Target,
                    ParamType::Path,
                    false,
                    "The file to read, workspace-relative, e.g. `src/main.rs`. Give this OR `ref`; a call that gives neither is refused. A file that does not exist is refused too, and says so — it does not come back empty. Reading a large file is fine: you get the first window and the result tells you how to ask for the rest.",
                ),
                documented(
                    "ref",
                    ArgumentRole::Target,
                    Text,
                    false,
                    "The id of a document `web` already fetched, to read it again without another request. Only ever an id a tool result gave you -- never invent one. Takes precedence over `path` if both are given.",
                ),
                documented(
                    "range",
                    ArgumentRole::Payload,
                    Text,
                    false,
                    "Which window to read: `first-last`, 1-based and inclusive, so \"20-60\" is line 20 through line 60. Omit it for the first window (2000 lines, 32KB). The line numbers printed in the result are the FILE's, not the window's, so \"500-600\" comes back starting at 500 — you can hand any number you see straight back as a range. A range is still bounded by that window, and the result says what you got and what to ask for next. Every other shape is REFUSED rather than guessed at — a single number, a range with no `-`, a `0` start, a backwards range, and a range past the end all come back with the reason and the line count.",
                ),
            ],
        ),
        registration(
            "write",
            // **A tool's NAME is the first thing a model matches against the verb in a request.**
            //
            // Told to write a file, the model looked for a write verb, found none among the ten
            // builtins, and reached for the shell -- `cat > session-handoff.md << 'EOF'`, journal
            // seq 4806, which fails under `cmd /C` with a bare exit code. Only after that did it
            // try `edit`, and then it chose `edit`'s wrong mode.
            //
            // `edit` used to be two tools wearing one name, separated by an OPTIONAL parameter:
            // pass `replacing` and it patches, omit it and it overwrites. A model had to infer a
            // mode from a schema that offered both, and no wording fixes an ambiguity that is in
            // the shape rather than the prose. The modes are now two tools, and every parameter
            // of each is required, so neither has a mode to guess.
            "Create a file, or replace all of its contents. Both arguments are required and there \
             is nothing else to decide: `content` becomes the whole file. To change one part of an \
             existing file and leave the rest alone, use `edit`. The parent directory must already \
             exist.",
            "edit",
            2_048,
            Reversible,
            &[WORKSPACE],
            &[],
            vec![
                // WritePath: `write` may create its target, which is most of the point.
                documented(
                    "path",
                    ArgumentRole::Target,
                    ParamType::WritePath,
                    true,
                    "The file, workspace-relative, e.g. `notes/handoff.md`. Created if it does not \
                     exist; its parent directory must already exist.",
                ),
                // Payload by design: section 9 is explicit that untrusted prose may fill an inert
                // body freely. The danger is the pair, not the text.
                documented(
                    "content",
                    ArgumentRole::Payload,
                    Text,
                    true,
                    "The ENTIRE contents of the file. Anything already there is replaced. It is written exactly as given, so text carrying `read`'s line-number prefix is stored WITH the numbers in it — that is allowed, because a file may legitimately contain numbered output, and the result says `line-numbered` when it happens so you can tell a deliberate one from a pasted `read`.",
                ),
            ],
        ),
        registration(
            "edit",
            // Three claims, each from the executor, and all three moved out from under this
            // comment: `replacing` must occur EXACTLY ONCE — `existing.find` locates the site and
            // `match_indices` then refuses a second one, naming the lines, so the description's
            // "refused and the result lists the line numbers" is the behaviour and "takes the
            // FIRST occurrence" no longer is; a miss returns `failed("edit", replacing_miss(…))`,
            // which diagnoses the cause it can check — a line-number prefix, CRLF, whitespace —
            // rather than one fixed sentence; and `replacing` is REQUIRED, so the truncate-and-
            // rewrite mode this once described is `write`, which is why the description sends a
            // whole-file replacement there. The parent
            // directory is the walk's rule, not this tool's — only the LAST component is opened
            // `CreateOrOpen`, so a missing parent is `Unopenable`, which
            // `executors.rs::edit_writes_through_the_handle_and_can_create` shows happening.
            "Change ONE snippet inside an existing file, leaving the rest untouched. To create a file, or to replace all of it, use `write` instead — this tool cannot. `read` the file first and copy `replacing` out of what comes back, byte for byte, **with `read`'s line-number prefix and its tab removed from every line** — the numbers are not in the file, and a `replacing` that still carries them is refused by name. `replacing` must appear EXACTLY ONCE in the file: if it occurs more than once the call is refused and the result lists the line numbers, so add a neighbouring line to make it unique or edit each site in its own call. Keep it as SHORT as that allows: `replacing` is DELETED and `content` put in its place, so a large `replacing` with a short `content` silently throws the difference away. The parent directory must already exist.",
            "edit",
            2_048,
            Reversible,
            &[WORKSPACE],
            &[],
            vec![
                // WritePath, not Path: `edit` is the one builtin that may create its target.
                documented(
                    "path",
                    ArgumentRole::Target,
                    ParamType::WritePath,
                    true,
                    "The file to change, workspace-relative. It must already exist and must not be empty -- `write` is the tool that creates one.",
                ),
                // Content is Payload by design: §9 is explicit that untrusted prose may fill
                // an inert body freely. The danger is the pair, not the text.
                documented(
                    "content",
                    ArgumentRole::Payload,
                    Text,
                    true,
                    "What `replacing` becomes. It must contain everything you still want from the text you put in `replacing`, because that text is gone -- if `replacing` is three lines and only the middle one changes, `content` is all three lines with the middle one edited. Write the lines as they belong in the file: `content` carrying `read`'s line-number prefix on 2 or more lines is refused, because writing it would put the numbers into the source. Use `write` if you genuinely mean to store numbered text.",
                ),
                documented(
                    "replacing",
                    ArgumentRole::Payload,
                    Text,
                    true,
                    "REQUIRED: the exact existing text to replace, copied from a `read` with the indentation kept and **`read`'s line-number prefix and tab removed from every line** — a value that still carries the prefix is REFUSED, and the refusal says where the stripped text really is. It must appear EXACTLY ONCE in the file; more than once is refused with the line numbers of every occurrence. Keep it SMALL — the smallest snippet that is unique, usually one line or a few. Do NOT paste the whole file: everything you put here is deleted and replaced by `content`, so a large `replacing` with a short `content` destroys the rest of the file. The call FAILS if it is not found, and an empty string is refused rather than inserting at the start.",
                ),
            ],
        ),
        registration(
            "glob",
            // **There was no way to list a directory, and the model invented what was in one.**
            //
            // `grep` searches file CONTENTS and needs a pattern. `read` needs a path already
            // known. `bash` is `Irreversible`, so every attempt stops and asks the user. Asked
            // what was inside `docs/requirements`, the model made SEVEN shell calls across two
            // sessions -- `dir "docs/requirements" /s`, `dir "docs\*" /b`, `list "docs"`,
            // `ls -la docs/requirements/`, and three more -- and then reported the directory
            // *"appears empty"*. It has four files in it.
            //
            // Names only: this opens nothing and reads no bytes, which is what lets it be `Inert`
            // and run without asking while `bash` cannot.
            "List the files under a directory, by name. This is how you see what EXISTS — `grep` \
             searches inside files and `read` needs a path you already have. Returns \
             workspace-relative paths, one per line, sorted. Build output and version-control \
             directories are not listed; name one as `path` to look inside it. It never reads a \
             file's contents, so it does not ask for approval.",
            // **The verb was the literal string `"find"` on the `glob` registration, and nothing
            // noticed** — because nothing READS `SummarySpec::verb`: a grep for uses outside its
            // own module returns constructors and one test, and the §B6 verb a user sees comes
            // from `tool.to_string()` on the wire. Corrected here rather than asserted on —
            // asserting a dead field's value is the sixteenth-instance family, which is how the
            // wrong value got here in the first place.
            "glob",
            8_192,
            Inert,
            &[WORKSPACE],
            &[],
            vec![
                documented(
                    "path",
                    ArgumentRole::Target,
                    ParamType::Path,
                    true,
                    "The DIRECTORY to list, workspace-relative. Pass \".\" for the whole \
                     workspace. Everything beneath it is included, not just its immediate \
                     children.",
                ),
                documented(
                    "pattern",
                    ArgumentRole::Payload,
                    Text,
                    false,
                    "Filename pattern. `*` matches any run of characters, `?` matches exactly \
                     one, and NOTHING ELSE is special — no `**`, no `[a-z]`, no `{a,b}`. With no \
                     `/` it matches the file's NAME anywhere beneath `path`, so `*.rs` finds every \
                     Rust file; with a `/` it matches the whole workspace-relative path, so \
                     `src/*.rs` finds only those directly in `src`. Case-insensitive. Omit it to \
                     list everything.",
                ),
            ],
        ),
        registration(
            "grep",
            // **RENAMED FROM `find`, ADR-059, and the rename is the bug fix.**
            //
            // `SHELL_DESCRIPTION` asserted both readings of the word in one paragraph — the unix
            // list promising `find` matches NAMES and `grep` matches CONTENTS, and the routing
            // sentence promising the exact inverse. Two definitions of one word in one request
            // body is stronger evidence than any appeal to a model's priors, which is why the fix
            // also DELETED `grep` and `find` from that list rather than only renaming the tool.
            //
            // The engine is `regex`, so `\d`, `(?i)` and alternation all work, and backreferences
            // and lookaround fail to COMPILE rather than being silently ignored. The result grammar
            // is `path:line:text` for a match and `path-line-text` for a context line, with the
            // line's own indentation intact so it can be handed straight to `edit`'s `replacing`.
            //
            // **No number appears in this text.** The caps are stated in the RESULT, where they are
            // actionable and where `format!` derives them from the constants — a number typed here
            // would be a second declaration of a bound, and this repo has that failure six times
            // over in its test suite alone.
            "Search the CONTENTS of files under a directory with a regular expression, line by line. `glob` is the tool for finding files by NAME; this one opens them and looks inside. `pattern` is a Rust regular expression — `grep -E` syntax plus `\\d` `\\w` `\\s` and `(?i)`, but with NO backreferences and NO lookaround, which fail to compile rather than being ignored. Matches come back one per line as `path:line:text`, with each line's original indentation intact, so a line can be copied straight into `edit`'s `replacing`. Build output and version-control directories are not searched; name one as `path` if you need it. **Set `glob` whenever you know the file type** — without it the walk opens whatever it reaches first and stops before it has seen everything, and the result says so when that happens. It opens files and changes nothing, so it does not ask for approval.",
            "grep",
            8_192,
            Inert,
            &[WORKSPACE],
            &[],
            // **`path` is REQUIRED, and that is the executor's demand rather than the role's.**
            // `grep` opens `handle_for(a, "path")` and returns "no adjudicated handle for `path`"
            // when it is absent — and the adjudicator only opens a handle for an argument that was
            // supplied. So a call omitting `path` was schema-valid and could never succeed, which
            // is the mismatch ADR-034 separated the two fields to make visible.
            //
            // **Four parameters and no more, and an undeclared one is REFUSED by the executor.**
            // `-i`, `--type`, `-l`, `output_mode` and `head_limit` are all things a model has in
            // hand from somewhere else; `recall`'s removed `payload_kind` is the precedent for what
            // accepting-and-ignoring one costs. `marlowe_exec::GREP_PARAMS` is that list, and
            // `every_declared_parameter_is_accepted_by_the_executor` pins the two together.
            vec![
                documented(
                    "pattern",
                    ArgumentRole::Payload,
                    Text,
                    true,
                    "The regular expression to look for inside each line. Rust `regex` syntax: `.` any character, `*` `+` `?` repetition, `[a-z]` a class, `|` alternation, `^` `$` the ends of a line, `\\d` `\\w` `\\s`, `(...)` a group. Backreferences (`\\1`) and lookaround (`(?=`, `(?<=`) do NOT exist here and fail to compile — there is no `\\Q...\\E` either. To search for text containing `. * + ? ( ) [ ] { } | ^ $ \\`, put a backslash before each one: `Cargo\\.toml`, `foo\\(bar\\)`. Unescaped they mean something else and will match text you did not intend. Start the pattern with `(?i)` to ignore case — `(?i)todo`. An invalid pattern is REFUSED with the syntax error and is never quietly searched as plain text. An empty string is refused rather than matching every line.",
                ),
                documented(
                    "path",
                    ArgumentRole::Target,
                    ParamType::Path,
                    true,
                    "The DIRECTORY to search, workspace-relative. Pass \".\" for the whole workspace. A file here is refused — use `read` for one file.",
                ),
                documented(
                    "glob",
                    ArgumentRole::Payload,
                    Text,
                    false,
                    "Only search files whose name or path matches this — the same pattern language the `glob` tool takes. `*` matches any run of characters except `/`, `?` matches exactly one, and NOTHING ELSE is special: no `**`, no `[a-z]`, no `{a,b}`. With no `/` it matches the file's NAME anywhere beneath `path`, so `*.rs` searches every Rust file; with a `/` it matches the whole workspace-relative path, so `crates/*/src/*.rs` searches only those. Case-insensitive. An empty string is refused rather than matching nothing. Omit it to search every file — but a large project has more files than one call can open, and this is how you make sure the search reaches the ones you meant.",
                ),
                documented(
                    "context",
                    ArgumentRole::Payload,
                    Integer,
                    false,
                    "How many lines above AND below each match to return as well, like `grep -C`. Omit it, or pass 0, to get only the matching lines. A context line is written `path-line-text` with hyphens and a match is written `path:line:text` with colons, so the two are never confused. The maximum is 20; a larger number is REFUSED, not quietly reduced. Context counts against the same result limit as matches, so a large value returns fewer matches.",
                ),
            ],
        ),
        registration(
            "web",
            "Fetch one URL over https and return the page. It does not search: supply a full URL. Always returns untrusted content.",
            "web",
            0, // Never inlined. §8.2: raw untrusted bytes do not reach attention.
            Inert,
            &[],
            &["*"],
            // **`url` is REQUIRED and `query` is gone.** Both were optional when `web` was
            // "search and fetch": a search had no url, a fetch had no query, so neither could be
            // mandatory. This build only fetches, so `url` is exactly what the executor cannot
            // run without — and a `query` parameter for an operation that does not exist invites
            // a call that always fails. It returns when search does (ADR-035).
            vec![documented(
                    "url",
                    ArgumentRole::Target,
                    Url,
                    true,
                    "The full URL to fetch, including `https://`. This tool does not search, so a search phrase here fetches nothing -- it must be an address you already have.",
                )],
        ),
        registration(
            "recall",
            // **The description now says WHEN to reach for it, because that is what was missing.**
            // Observed live: asked when a deploy script runs — an answer that was in memory, with
            // `recall` exposed and executable — the model called `bash` (`ls -la`), was refused, and
            // then answered from a directory listing that never happened. "Search memory
            // explicitly" describes the mechanism and never told it this was the moment.
            //
            // Every clause is `daemon::recall`: it reads `recall_candidates()` (§4.3 hot ∪ cold,
            // tombstones and unmatured included, which auto-injection withholds), scores with
            // `cue::lexical` (BM25 over tokens, no stemming and no stop list — so the user's own
            // words are the ones that match), drops zero scores, and truncates at `RECALL_LIMIT`.
            // The withholding half is not taken on trust either:
            // `retrieve.rs::nothing_is_injected_before_maturation` is the injection path refusing
            // an entry that `recall_candidates()` returns unconditionally.
            "Search Marlowe's own memory: what the user said in earlier sessions, and what Marlowe has learned. Reach for this FIRST when a question touches anything from before — ahead of guessing, and ahead of reading the filesystem. It also returns entries nothing else surfaces: ones not yet matured, and ones forgotten. Matching is by word, so reuse the user's. Up to 5 results.",
            "recall",
            8_192,
            Inert,
            &[],
            &[],
            // **`payload_kind` is gone.** `RecallTools::recall` reads `query` and nothing else, so
            // the argument was accepted, ignored, and never reported — a model that passed
            // `payload_kind: "commitment"` believed it had filtered and got an unfiltered answer.
            // That is worse than `web`'s removed `query`, which at least failed loudly. It comes
            // back when the executor filters on it.
            vec![documented(
                    "query",
                    ArgumentRole::Payload,
                    Text,
                    true,
                    "What to look for. Matching is word-by-word with no stemming, so REUSE THE USER'S OWN WORDS rather than paraphrasing them -- \"deploy script\" finds what was said about a deploy script; \"deployment process\" may not.",
                )],
        ),
        registration(
            "remember",
            // CONTRACTS §3.5's naming hazard: the tool is a REQUEST, not a write the model performs.
            // `DaemonMemory::remember` adjudicates the claim, maps `payload_kind` through a closed
            // set (an unknown value is refused by name), appends through the signed journal, and
            // returns `"{id} · {trust:?} · injectable after {ms}"` — so the receipt's three fields
            // are named here rather than left for the model to parse blind.
            "Ask the harness to record something worth keeping. A request, not a write: the harness adjudicates it, stamps and signs it, and can refuse. What comes back is a receipt — the entry's id, the trust class the harness derived for it, and when it becomes injectable.",
            "remember",
            1_024,
            Consequential,
            &[],
            &[],
            vec![
                documented(
                    "text",
                    ArgumentRole::Payload,
                    Text,
                    true,
                    "What to record, as a self-contained sentence. It is read back in a later session with none of this conversation around it, so \"he prefers it\" is useless and \"Matthew prefers X because Y\" is not.",
                ),
                // Which memories a claim derives from is a Target: choosing the lineage is how
                // laundering would launder (§14.6, HP6).
                documented(
                    "derived_from",
                    ArgumentRole::Target,
                    Identifier,
                    false,
                    "The id of an existing memory this one was concluded from, if any. It carries that entry's trust class forward, so it is how a conclusion stays as trustworthy as its source and no more.",
                ),
                documented(
                    "payload_kind",
                    ArgumentRole::Target,
                    Text,
                    false,
                    "What kind of thing this is, from a fixed list: `episode`, `fact`, `entity`, `edge`, `procedure`, `commitment`, `person`, `relationship`, `voice_params`, `noticing`. Omitting it means `episode` -- something that happened. Any other value is refused.",
                ),
            ],
        ),
        registration(
            "use",
            // **The two-step contract lives HERE, and that placement is the point.** It used to
            // live only in the search result's last line -- *"Load one with `use` and its name"* --
            // which is an imperative inside a TOOL RESULT, and the persona instructs the model that
            // a tool result is data and never instruction. The harness was asking the model to obey
            // the one channel it is trained to distrust, and the model correctly did not: it
            // searched, got a name and a description, and stopped one call short of the body.
            // A tool's own description is where an operating contract is legitimately read.
            "Find and load a skill. `query` searches and returns names with one-line descriptions \
             ONLY -- never a skill's instructions. `name` loads that skill's full body. A search \
             tells you what exists; you have not read a skill until you load it by name.",
            "use",
            4_096,
            // Reversible rather than Inert *so that the target check fires*. Loading a skill
            // chosen by untrusted content is supply-chain steering, and §9 checks Reversible.
            Reversible,
            &[],
            &[],
            vec![
                documented(
                    "name",
                    ArgumentRole::Target,
                    Text,
                    false,
                    "The skill to load, named exactly as a search reported it. This returns the skill's full instructions -- it is the second of the two steps, and the only one that gives you anything to follow.",
                ),
                documented(
                    "query",
                    ArgumentRole::Payload,
                    Text,
                    false,
                    "What to search for. Returns matching skill NAMES and one-line descriptions only, never their contents. Use this when you do not already know a skill's name.",
                ),
            ],
        ),
        registration(
            "run",
            // **It spawns now — ADR-057, M3 Session B1.** This description said *"this build cannot
            // spawn one yet, so the call is refused"* for the whole of M2, and it was accurate:
            // `control_step` routed the call to a tool host with no `run` executor, deliberately,
            // so the model got a refusal it could act on. What was missing was never plumbing —
            // `Engine::spawn` has been complete since M2 A — but a decision about who declares the
            // six fields of the contract the model does not supply. ADR-057 is that decision.
            //
            // **Still no steer and no await.** There is no run id among these parameters and the
            // model cannot name one, so `run` spawns and blocks. Saying so is the
            // `web`-does-not-search precedent: a description that names an operation the model
            // cannot reach is the one thing worse than a missing feature.
            //
            // The defaults named here are ADR-057 §1's, restated where the model actually reads
            // them. A model that has to guess whether omitting `exposed_tools` means "none" or
            // "everything I have" will guess the generous one.
            // **`budget_tokens` is stated in the description because that is where the model
            // reads its constraints.** Watched live 2026-08-26: the model asked for
            // `budget_tokens: 100`, the harness granted exactly that — ADR-057 makes the field
            // model-supplied — and the child paused on its first iteration having spent nothing,
            // because 100 tokens cannot carry one model call. Nothing was broken; the budget
            // backstop reported the pause honestly. But the model had no way to know the number
            // was unusable, and a receipt saying `budget: 100 tokens` reaches its context after
            // the fact rather than before the choice.
            //
            // **That guidance was tried alone, and it did not hold. There is now a floor in
            // `Budget::grant`, and the paragraph above is what the model reads BEFORE choosing.**
            //
            // The argument for advice-only was that a refusal threshold would be *"a constant
            // nobody has measured"*. It was wrong on its own terms: `MIN_CALL_TOKENS` had been
            // the measured, enforced floor since M2, read by `has_room_for_a_call` at the moment
            // of SPENDING and by nothing at the moment of GRANTING. So the two halves of the
            // system disagreed silently, and `grant` handed out budgets the loop was certain to
            // reject. See `budget::MIN_CHILD_TOKENS`, which derives the figure rather than
            // choosing it.
            //
            // Two of the next three live spawns asked for 100 and 500 (journal seq 4584, 4615).
            // Both children died before their first word. Advice the model can ignore is not a
            // control, and the number in it was not even the enforced one.
            "Delegate a sub-task to a child run and wait for it. The child is a SEPARATE agent with an EMPTY window: it cannot see this conversation, your files, your memory, or anything you have already learned. It knows only what `task` says, and it can only use the tools you name in `exposed_tools` — there is no other way for it to get one. Whatever it needs must be IN the task. It works, returns one structured result, and is gone — its reasoning and tool calls never enter this conversation. Use it to parallelise work you could describe to a competent stranger. Do NOT use it to answer a question about this conversation, this codebase, or your own tools: the child knows none of that and will spend its whole budget discovering it cannot.",
            "run",
            1_024,
            Consequential,
            &[],
            &[],
            vec![
                documented(
                    "task",
                    ArgumentRole::Payload,
                    Text,
                    true,
                    "The child's ENTIRE brief. It has no other context, so include the background, the question, and any text it must work from. `summarise the run tool` fails; `summarise this text: <text>` works.",
                ),
                documented(
                    "output_contract",
                    ArgumentRole::Payload,
                    Text,
                    false,
                    "Comma-separated field names the child must return, e.g. `findings` or `summary,risks`. Defaults to one `findings` field. The child's reply is REJECTED if it does not fill every field you name.",
                ),
                // The child's capability profile and budget are Targets. Untrusted content
                // choosing a child's tool set is the trifecta reassembling itself one level
                // down.
                //
                // **And that declaration was enforced by nothing until ADR-057 §4.**
                // `ModelStep::Spawn` never reached the adjudicator — only `tool_batch` calls it —
                // so these three were Targets in a manifest and free-for-all in the loop. It was
                // invisible because the path was dead: no model call could produce a spawn, so
                // every spawn in the workspace was hand-built at `UserAsserted`, where the check
                // would not have fired either way. `Engine::spawn` now applies
                // `blocks_composed_targets` itself.
                documented(
                    "exposed_tools",
                    ArgumentRole::Target,
                    Text,
                    true,
                    "REQUIRED. **This parameter is the only way a child ever gets a tool.** There is no other mechanism: nothing written in `task` can request, grant or imply access to anything, and the child cannot ask for more once it starts. Give a comma-separated list of names taken from the tools you hold — a name you do not have is refused. A child given an empty string CANNOT read a file, list a directory, search, or run a command no matter what `task` says; it can only think about the text you put in `task` and reply. So if the child must look at anything at all, name the tools here — `read, glob` for reading files, plus `grep` to search inside them.",
                ),
                // **`budget_micros_usd` -> `budget_tokens`, ADR-057 §6.** `SpawnRequest::grant_tokens`
                // and `Budget::grant`'s `explicit` are tokens. Wiring the old name to the field it
                // names would have handed a micro-dollar count to a token grant: a correct number
                // about the wrong quantity, which is the family this file is full of warnings about.
                // `Amount` goes with it — it is documented as money, and a token count is not money.
                documented(
                    "budget_tokens",
                    ArgumentRole::Target,
                    Integer,
                    false,
                    "Tokens the child may spend, taken from yours. OMIT IT to get a share sized for the job, which is almost always right. If you do set it, the MINIMUM is 3601 and anything smaller is refused: a child spends its whole brief on its first call before it emits a word. Think tens of thousands, not hundreds.",
                ),
                documented(
                    "orphan_policy",
                    ArgumentRole::Target,
                    Text,
                    false,
                    "`terminate` (default) ends the child when this run ends; `detach` lets it outlive this run. Omit unless you specifically want it to survive you.",
                ),
                // ---- M3 Session C: the two fields a SPAWNER names, both Targets ------------
                //
                // **A role name is a target under ADR-023 (ADR-069), and so is a kind.** The
                // decisive argument is one field up: `orphan_policy` is a closed two-value enum
                // that chooses only a child's LIFETIME and it is already a Target. A value that
                // chooses the inference engine executing every subsequent decision the child
                // makes is a target a fortiori, and calling a role a payload would de-target
                // `orphan_policy` by the identical argument. A downgrade ("make it dumber
                // before an attack") and an upgrade ("burn the budget") are both
                // attacker-useful.
                //
                // **Declaring them here is necessary and NOT sufficient**, and that sentence is
                // the reason `run` has a test asserting its Target subset literally.
                // `ModelStep::Spawn` never reaches `adjudicate` -- only `tool_batch` calls it --
                // so `adjudicate`'s target loop, where `ArgumentRole::Target` is actually read,
                // does not run for a spawn. The enforcement is `composes_spawn_targets` in
                // `marlowe-loop/src/engine.rs`, a HAND-WRITTEN MIRROR of this list with nothing
                // structural holding the two together. A parameter added here without a clause
                // there is invisible to layer 3.
                documented(
                    "role",
                    ArgumentRole::Target,
                    Text,
                    false,
                    "Which model tier serves the child: `worker` (default, the fast cheap tier -- almost always right), `orchestrator` (the strong tier: costly, for a child that must plan rather than fetch), or `summarizer` (the cheapest). Omit it unless the child genuinely needs a different tier from the default. A word this list does not contain is granted as `worker`, and the receipt says which tier it actually got.",
                ),
                documented(
                    "kind",
                    ArgumentRole::Target,
                    Text,
                    false,
                    "`worker` (default) does the task itself. `master` runs a team: it may hold NO working tools, only `run` and `ask`, and it can only actually delegate if you ALSO put `run` in its `exposed_tools` -- naming `master` alone gets you a manager with nobody to manage, and the receipt will show it. Ask for `master` only when the job genuinely needs a team; a master that was going to do the work itself cannot.",
                ),
            ],
        ),
        registration(
            "ask",
            // What actually happens: `control_step` builds `ModelStep::Ask(question)`, the engine
            // records `ApprovalRequested`, sets `RunStatus::Paused { AwaitingAnswer }` and returns
            // `LoopOutcome::Escalated`. So the run stops until the user answers — which is the part
            // worth knowing before calling it. "A decision package" named a shape nothing builds.
            "Put a question to the user and wait. The run pauses until they answer. For a choice that is theirs to make — not for confirming work you could simply do.",
            "ask",
            2_048,
            Inert,
            &[],
            &[],
            // **`options` is gone, for `web`'s `query` reason.** `control_step` reads `question`
            // (falling back to the message body) and nothing else, and `ModelStep::Ask` carries a
            // single `String` — so a model that listed options had them silently dropped on the way
            // to a user who never saw them.
            vec![documented(
                    "question",
                    ArgumentRole::Payload,
                    Text,
                    true,
                    "What to put to the user, as one plain question. The run STOPS here until they answer, so ask only what you cannot determine yourself. If there are options, put them in this sentence -- there is no separate field for them.",
                )],
        ),
    ];

    for reg in regs {
        let reg = reg?;
        r.register(reg).expect("builtin ids are distinct and each manifest names its own tool");
    }
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exposure::MAX_EXPOSED_TOOLS;

    #[test]
    fn eleven_tools_against_a_budget_of_twelve() {
        let r = builtin_registry().expect("the builtin manifests load");
        assert_eq!(r.len(), BUILTIN_TOOLS.len());
        // **Ten since M2 C2e removed `done`; eleven since `write` was split out of `edit`.**
        // ADR-006 named eleven, lost one when `done` went, and spends the slot again here --
        // deliberately. The reason is in `FileSystemTools::write`: `edit` was two tools wearing
        // one name, separated by an OPTIONAL parameter, and a model asked to write a file reached
        // for the SHELL because no builtin was named for the verb.
        //
        // **One spare slot left, and spending it needs an ADR.** This comment is the record that
        // the last one was spent on purpose.
        assert_eq!(BUILTIN_TOOLS.len(), 12, "ADR-006's eleven, less `done`, plus `write` and `glob`");
        assert!(
            BUILTIN_TOOLS.len() < MAX_EXPOSED_TOOLS,
            "the spare slot is the design; spending it here needs an ADR"
        );

        let ids: Vec<ToolId> = BUILTIN_TOOLS.iter().map(|t| ToolId::new(*t)).collect();
        assert!(r.expose(&ids).is_ok(), "all twelve fit in one exposed set");
    }

    /// **A description that hit the cap was cut mid-sentence and nothing said so.**
    ///
    /// `Description::new` truncates at [`MAX_DESCRIPTION_CHARS`] silently — correctly, because the
    /// budget must hold for third-party prose that arrives as bytes. But for the builtins the cap
    /// is a drafting error rather than an attack, and the failure is invisible: the tool list still
    /// renders, the model still gets a description, and the half of the sentence that said *"the
    /// call fails if it is not found"* is simply gone.
    ///
    /// Strictly less than the cap, not `<=`: a truncated description is exactly `MAX` characters,
    /// so `<=` would pass on the one case this exists to catch.
    #[test]
    fn no_builtin_description_is_silently_truncated() {
        use crate::registry::MAX_DESCRIPTION_CHARS;
        let r = builtin_registry().unwrap();
        for reg in r.iter() {
            let n = reg.description.text().chars().count();
            assert!(
                n < MAX_DESCRIPTION_CHARS,
                "`{}`'s description is {n} chars and the cap is {MAX_DESCRIPTION_CHARS}, so it \
                 reached the model cut off mid-sentence: {:?}",
                reg.id,
                reg.description.text()
            );
        }
    }

    /// **Every builtin parameter states its own meaning, where the model reads it.**
    ///
    /// A `ParamSpec` with no `description` does not reach the model blank -- `param_description`
    /// generates a sentence from the type and the arity, and for a `Text` payload that sentence is
    /// **"Optional. text."** It looks like documentation, occupies the slot documentation would
    /// occupy, and says nothing. Fifteen of the eighteen builtin parameters were in that state.
    ///
    /// Observed live 2026-08-26, journal seq 4677-4679: the model called `read` with neither
    /// `path` nor `ref`. Both are optional, both described as "Optional. text.", and nothing the
    /// model could see said what either was for or that one of them was required. The executor
    /// refused it naming both and the model recovered -- a wasted call, a wasted turn, and the
    /// journal recorded only `{"tool":"read","summary":"read"}`.
    ///
    /// **This is a guard on the drafting, not on the mechanism**, and that is the point: the
    /// generated fallback means a missing description can never fail loudly on its own. So it has
    /// to fail here. A new parameter added without one fails the build by name.
    #[test]
    fn every_builtin_parameter_says_what_it_is_for() {
        let r = builtin_registry().unwrap();
        let mut bare = Vec::new();
        for reg in r.iter() {
            for p in reg.manifest.params() {
                match &p.description {
                    // Not merely present: a blank or whitespace description is the same silence
                    // with a field set, and would satisfy an `is_some()` check.
                    Some(d) if !d.trim().is_empty() => {}
                    _ => bare.push(format!("{}::{}", reg.id, p.name)),
                }
            }
        }
        assert!(
            bare.is_empty(),
            "these parameters reach the model as a sentence generated from their type and arity              -- \"Optional. text.\" -- which reads as documentation and is not: {bare:?}"
        );
    }

    /// **A wrapped string literal that lost its `\\` continuation, which Rust does not warn about.**
    ///
    /// A description written across several source lines keeps every leading space of the
    /// continuation lines unless the line ends with a backslash. Five of `run`'s parameters
    /// shipped with runs of **22 spaces** inside them for exactly that reason -- the model was
    /// reading `"drawn from the tools                      you hold"`. Harmless to parse, wasteful
    /// to send, and it is formatting the model may imitate.
    ///
    /// Three rather than two, so an ordinary double space after a full stop is not a failure.
    #[test]
    fn no_builtin_description_carries_a_wrapped_literals_indentation() {
        let r = builtin_registry().unwrap();
        let mut ragged = Vec::new();
        for reg in r.iter() {
            if reg.description.text().contains("   ") {
                ragged.push(reg.id.to_string());
            }
            for p in reg.manifest.params() {
                if p.description.as_deref().is_some_and(|d| d.contains("   ")) {
                    ragged.push(format!("{}::{}", reg.id, p.name));
                }
            }
        }
        assert!(
            ragged.is_empty(),
            "these carry a source literal's indentation into the text the model reads; end each \
             wrapped line with a backslash: {ragged:?}"
        );
    }

    #[test]
    fn every_builtin_parameter_has_a_role() {
        // Not a restatement of the load-time check: this asserts that the eleven shipped
        // manifests actually pass it, which is what "startup fails on an unannotated
        // manifest" means for the tools that are always present.
        let r = builtin_registry().unwrap();
        for reg in r.iter() {
            for p in reg.manifest.params() {
                assert!(
                    matches!(p.role, ArgumentRole::Target | ArgumentRole::Payload),
                    "{}::{} has no role",
                    reg.id,
                    p.name
                );
            }
        }
    }

    #[test]
    fn the_shell_command_is_a_target() {
        // The single most consequential role assignment in the file. If `command` were a
        // Payload, untrusted content could compose a shell line and the provenance check
        // would pass it, because Payload fields are unchecked by design.
        let r = builtin_registry().unwrap();
        let bash = r.manifest(&ToolId::new("bash")).unwrap();
        assert_eq!(bash.role_of("command"), Some(ArgumentRole::Target));
        assert_eq!(bash.consequence(), ConsequenceLevel::Irreversible);
    }

    #[test]
    fn web_is_inert_and_never_inlines() {
        let r = builtin_registry().unwrap();
        let web = r.get(&ToolId::new("web")).unwrap();
        assert_eq!(web.manifest.consequence(), ConsequenceLevel::Inert);
        assert_eq!(
            web.summary.inline_threshold_bytes, 0,
            "fetched bytes never reach attention; the loop gets a reference"
        );
    }

    /// **The declaration. `Engine::spawn` is where it is now ENFORCED** — ADR-057 §4, and the
    /// enforcement is a separate test in `spawn_and_budget.rs` on purpose. This one asserts what
    /// the manifest says; between M2 and ADR-057 it was green while `ModelStep::Spawn` bypassed
    /// the adjudicator entirely, which is the "declared control nothing reads" family. A role in a
    /// manifest is a claim about a check somewhere else, so both halves need their own test.
    #[test]
    fn the_child_capability_arguments_are_targets() {
        let r = builtin_registry().unwrap();
        let run = r.manifest(&ToolId::new("run")).unwrap();
        assert_eq!(run.role_of("exposed_tools"), Some(ArgumentRole::Target));
        assert_eq!(run.role_of("budget_tokens"), Some(ArgumentRole::Target));
        assert_eq!(run.role_of("orphan_policy"), Some(ArgumentRole::Target));
        assert_eq!(run.role_of("task"), Some(ArgumentRole::Payload));
        // The two the model may never name at all. ADR-057 §5: withheld structurally, so there is
        // nothing to type. A parameter added here later would make `share` and the quarantine
        // profile model-reachable without anyone deciding to.
        assert_eq!(run.role_of("share"), None);
        assert_eq!(run.role_of("reads_untrusted"), None);
        // M3 Session C. A role and a kind are Targets for `orphan_policy`'s reason, one field
        // over: a closed enum that chooses a child's lifetime is already a Target, and a value
        // that chooses the inference engine executing every decision the child makes is one a
        // fortiori.
        assert_eq!(run.role_of("role"), Some(ArgumentRole::Target));
        assert_eq!(run.role_of("kind"), Some(ArgumentRole::Target));
    }

    /// **The set, not one absence -- and it is written out here independently of the manifest.**
    ///
    /// `the_child_capability_arguments_are_targets` above asks about named parameters one at a
    /// time, so it can see a role CHANGE and cannot see the set GROW: adding a seventh Target to
    /// `run` leaves every one of its assertions green. That is instance #19's shape (a check
    /// whose input is the object it is checking) aimed at a manifest.
    ///
    /// The literal below is the second copy on purpose, and the doubling is the mechanism: it is
    /// asserted **against** the manifest rather than derived from it, so a parameter added to
    /// `run` fails here by name until somebody classifies it.
    ///
    /// **And classifying it is not the whole job.** `ModelStep::Spawn` never reaches
    /// `adjudicate`, so a `Target` declaration on `run` is enforced by
    /// `marlowe-loop/src/engine.rs`'s `composes_spawn_targets` -- a hand-written mirror of this
    /// list. A new Target here needs a disjunct there in the same commit, and
    /// `marlowe-loop`'s `spawn_request_fields` guard is the other half of that pair.
    #[test]
    fn run_declares_exactly_seven_parameters_and_exactly_five_are_targets() {
        let r = builtin_registry().unwrap();
        let run = r.manifest(&ToolId::new("run")).unwrap();

        let mut names: Vec<&str> = run.params().iter().map(|p| p.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            vec![
                "budget_tokens",
                "exposed_tools",
                "kind",
                "orphan_policy",
                "output_contract",
                "role",
                "task",
            ],
            "a parameter was added to or removed from `run`. Classify it: a Target needs a \
             `composes_spawn_targets` disjunct in `marlowe-loop/src/engine.rs` in the same \
             commit, or untrusted content can choose it"
        );

        let mut targets: Vec<&str> = run
            .params()
            .iter()
            .filter(|p| p.role == ArgumentRole::Target)
            .map(|p| p.name.as_str())
            .collect();
        targets.sort_unstable();
        assert_eq!(
            targets,
            vec!["budget_tokens", "exposed_tools", "kind", "orphan_policy", "role"],
            "the Target subset of `run` moved"
        );
    }
}
