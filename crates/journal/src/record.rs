//! The journal record schema.
//!
//! Every type an record names is defined here. The journal sits at the bottom of
//! the dependency graph and can borrow nothing; crates above it convert inward.
//! `policy`'s `Request` / `Decision` are twinned here as [`Access`] /
//! [`Decision`] so the monitor can join `Permission` against `ToolResult`
//! without linking `policy`.
//!
//! **Every enum here is internally tagged on a fixed `kind` key**
//! (`#[serde(tag = "kind", rename_all = "snake_case")]`). The JSONL file is the
//! contract: a consumer streaming lines routes on a discriminant at a known
//! key, which serde's default external tagging (`{"write": {...}}`) does not
//! give, because it puts the discriminant in a position that varies by variant.
//!
//! **That is why no variant wraps non-map content.** Internal tagging is not
//! available for a newtype variant wrapping a string: `Access::Read(PathBuf)`
//! fails at *runtime* with `cannot serialize tagged newtype variant containing
//! a string`, and so would `Block::Text(String)` and `Scope::Rule(String)`. The
//! fix is the variant shape, not the tagging: those three carry a named field
//! instead (`Read { path }`, `Text { text }`, `Rule { rule }`). A variant with
//! two or more fields wraps a struct named after it, which serializes
//! identically because serde flattens a newtype variant around a map.
//!
//! So `{"kind":"write","path":"/repo/src/foo.rs"}`, never
//! `{"Write":"/repo/src/foo.rs"}`. `docs/journal-sample.jsonl` is the
//! authoritative example and is round-tripped byte-for-byte by a test in
//! [`crate::codec`], so it cannot drift from this file.

use std::net::IpAddr;
use std::path::PathBuf;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::id::{CallId, RecordId, SessionId};

// ========================================================================
// Data Structures
// ========================================================================

/// One journaled record. `parent` is `None` for the session root; otherwise it is
/// the preceding record, making the journal a tree (one parent per record, unique
/// walk from any head).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    /// When the record was recorded. RFC 3339, milliseconds, UTC; see
    /// [`crate::timestamp`].
    #[serde(with = "crate::timestamp")]
    pub at: SystemTime,
    /// The record's own id.
    pub id: RecordId,
    /// What happened. The `kind` tag is flattened into the record object.
    #[serde(flatten)]
    pub kind: RecordKind,
    /// The preceding record, or `None` for the session root. This field is the tree.
    pub parent: Option<RecordId>,
    /// Which session this record belongs to.
    pub session: SessionId,
}

/// What kind of thing an [`Record`] records. Internally tagged so the
/// discriminant lands at a fixed `"kind"` key on every journal line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RecordKind {
    /// A mid-history range was summarized and dropped; see [`Compacted`].
    Compacted(Compacted),
    /// Tokens burned by a session this one delegated to; see [`Delegated`].
    Delegated(Delegated),
    /// Another session's history was folded into this one; see [`Folded`].
    Folded(Folded),
    /// One completed model turn: the blocks it produced and the tokens it burned.
    ModelTurn(ModelTurn),
    /// A policy decision reached for one access within a tool call.
    Permission(Permission),
    /// A model-transport failure, recorded for retry analysis and resume.
    ProviderFailed(ProviderFailed),
    /// The first record of a session, pinning the schema version and digests.
    SessionStarted(SessionStarted),
    /// The session's title, generated once from its first exchange. A resume
    /// picker reads this in place of its first-user-message fallback.
    SessionTitled { title: String },
    /// The model requested a tool call.
    ToolCall(ToolCall),
    /// A tool call completed with this outcome.
    ToolResult(ToolResult),
    /// A message from the user.
    UserMessage { text: String },
    /// A `kind` this build does not know.
    ///
    /// **[`crate::SCHEMA_VERSION`]'s doc says adding a variant is not breaking,
    /// and without this that was false.** An internally tagged enum rejects an
    /// unknown tag with a hard error, so a build that added a variant without
    /// bumping the version -- exactly what the doc sanctions -- wrote journals
    /// that an older build, or the external monitor, could not read *at all*:
    /// one unrecognized line failed the whole decode, and with it resume.
    ///
    /// Reading it as `Unknown` loses that record and keeps the session. The
    /// record's `parent` still comes from the outer [`Record`], so the chain stays
    /// walkable through it and only the payload is lost.
    #[serde(other)]
    Unknown,
}

/// First record of a session. The digests make a prompt or tool-description
/// change mechanically detectable: a golden journal recorded against one digest
/// and replayed under another diverges, and the failure can name why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionStarted {
    /// Journal schema this session was written against.
    pub schema_version: u32,
    /// The session's working directory.
    pub cwd: PathBuf,
    /// The exact model id this session ran against, e.g. `claude-opus-4-8`.
    ///
    /// Beside the digests for the same reason they are here: provenance a reader
    /// cannot reconstruct. A golden journal that fails after a model swap
    /// diverges with no clue why, and Phase 9 telemetry cannot attribute a
    /// session to a model at all. `#[serde(default)]` so a journal written
    /// before this field existed still reads, which is what makes adding it a
    /// non-breaking change rather than a `schema_version` bump.
    #[serde(default)]
    pub model: String,
    /// Hash of the effective policy.
    pub policy_digest: String,
    /// Hash of the system prompt as sent.
    pub prompt_digest: String,
    /// Hash of the sorted tool specs as sent.
    pub tools_digest: String,
}

impl SessionStarted {
    pub fn new(
        schema_version: u32,
        cwd: PathBuf,
        model: String,
        policy_digest: String,
        prompt_digest: String,
        tools_digest: String,
    ) -> Self {
        Self {
            schema_version,
            cwd,
            model,
            policy_digest,
            prompt_digest,
            tools_digest,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelTurn {
    /// The content blocks the model produced this turn.
    pub blocks: Vec<Block>,
    /// Why the turn ended. Not inferable from `blocks`: see [`StopReason`].
    pub stop: StopReason,
    /// Token accounting for this turn.
    pub usage: Usage,
}

impl ModelTurn {
    pub fn new(blocks: Vec<Block>, stop: StopReason, usage: Usage) -> Self {
        Self {
            blocks,
            stop,
            usage,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub call: CallId,
    pub name: String,
    pub args: Value,
}

impl ToolCall {
    pub fn new(call: CallId, name: String, args: Value) -> Self {
        Self { call, name, args }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub call: CallId,
    pub outcome: Outcome,
}

impl ToolResult {
    pub fn new(call: CallId, outcome: Outcome) -> Self {
        Self { call, outcome }
    }
}

/// A policy decision for one access within a tool call. Per access, not per
/// call: one `grep` reads hundreds of files, so keying by `call` alone would
/// leave a specific write unmatched against its grant. `call` is a correlation
/// field, not a key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Permission {
    pub call: CallId,
    pub access: Access,
    pub decision: Decision,
    /// What the user agreed to, which is not always what was asked. Without it,
    /// resume cannot tell "allow this write" from "allow all writes under `src/`".
    pub scope: Scope,
}

impl Permission {
    pub fn new(call: CallId, access: Access, decision: Decision, scope: Scope) -> Self {
        Self {
            call,
            access,
            decision,
            scope,
        }
    }
}

/// One model-transport failure, recorded before the retry sleeps rather than
/// after the outcome is known: a retry storm that ends in success is invisible
/// in a journal that only records failures nobody recovered from.
///
/// The classification field is `failure`, not `kind`, and that is forced rather
/// than chosen. `RecordKind` is tagged on `kind` and this struct's fields are
/// flattened alongside that tag, so a field named `kind` would emit the key
/// twice: serde writes both happily and then refuses to read the result back
/// (`duplicate field `kind``). The tag key is the contract external consumers
/// route on, so the payload field is the one that moves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderFailed {
    pub attempt: u32,
    pub failure: String,
}

impl ProviderFailed {
    pub fn new(failure: String, attempt: u32) -> Self {
        Self { attempt, failure }
    }
}

/// What a delegated sub-session cost the session that spawned it.
///
/// **A session's own `ModelTurn` records cannot answer this.** A subagent runs on
/// an engine of its own and journals its turns to a file of its own, so from the
/// parent's journal those tokens simply never existed: the runaway guard did not
/// see them, and a session that delegated three times reported a fraction of
/// what it spent. This record is where they re-enter the parent's accounting, and
/// being an record rather than a counter is what makes them survive a resume.
///
/// It carries no per-turn detail on purpose. The sub-session's own journal has
/// every turn it took, and `journal` names that file, so the summary here is the
/// total and the pointer rather than a second copy that could disagree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delegated {
    /// The tool call that spawned the sub-session.
    pub call: CallId,
    /// The sub-session's journal file, so its turns can be read in full.
    pub journal: String,
    /// Everything the sub-session spent, totalled across its turns.
    pub usage: Usage,
}

impl Delegated {
    pub fn new(call: CallId, journal: String, usage: Usage) -> Self {
        Self {
            call,
            journal,
            usage,
        }
    }
}

/// Records that a range of older records was summarized away. `dropped` is
/// load-bearing, not provenance: those records remain physically on the walk
/// (append-only means their children's parents cannot be rewritten) and must be
/// subtracted logically. See [`crate::Walk`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Compacted {
    pub dropped: Vec<RecordId>,
    /// Everything the session had spent, at the moment of the compaction.
    ///
    /// **Spend has to survive here or it does not survive at all.** The walk
    /// subtracts a compaction's dropped records before the fold, so the
    /// `ModelTurn` usage inside the dropped range vanishes with them: a resumed
    /// session restarted its `max_tokens` counter near zero,
    /// on exactly the long sessions compaction exists for. Carrying the running
    /// total forward makes the guard cumulative across compaction, and the fold
    /// adds the turns *after* this record to it.
    ///
    /// `#[serde(default)]` so a journal written before this field reads as zero,
    /// which is what the old behaviour already was.
    #[serde(default)]
    pub spent: Usage,
    pub summary: String,
}

impl Compacted {
    pub fn new(dropped: Vec<RecordId>, spent: Usage, summary: String) -> Self {
        Self {
            dropped,
            spent,
            summary,
        }
    }
}

/// Records that another session's history was folded into this one.
///
/// **A marker, not a re-parenting, and that distinction is the whole point.**
/// `/resume` used to adopt the foreign journal's head as the running session's
/// `head`, so every record appended afterwards carried a parent id that does not
/// exist in its own file. On the next restart the walk hit that dangling parent
/// and silently truncated: everything after the `/resume` was unrecoverable, and
/// resume of the running session failed with `NoSession`. This record keeps the
/// running chain intact and records what was folded, so `/status` and the
/// monitor can both say where the context came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Folded {
    /// How many records were folded in.
    ///
    /// **The one field the `Event` to `Record` rename touched on the wire.** It
    /// was `events`, and a journal written before the rename still says so, so
    /// the old spelling is accepted on read: dropping it would make a session
    /// that had ever folded a subagent unresumable, which is a steep price for a
    /// word. Written as `records`.
    #[serde(alias = "events")]
    pub records: usize,
    /// The head of the journal that was folded.
    pub source_head: RecordId,
    /// The session that journal belonged to.
    pub source_session: SessionId,
}

impl Folded {
    pub fn new(records: usize, source_head: RecordId, source_session: SessionId) -> Self {
        Self {
            records,
            source_head,
            source_session,
        }
    }
}

/// One block of model output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Block {
    Text { text: String },
    Thinking(BlockThinking),
    ToolUse(BlockToolUse),
}

/// A thinking block, which is neither optional nor cosmetic. Opus 4.8 under
/// adaptive thinking emits these, and they must be echoed back
/// **byte-identical** on the next turn of the same conversation: `signature` is
/// opaque and the API rejects a modified block. A journal that dropped them
/// could not replay a turn, and resume would rebuild a request the API refuses.
///
/// `text` is empty unless the provider asked for `display: "summarized"`. Store
/// what arrived either way.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockThinking {
    pub signature: String,
    pub text: String,
}

impl BlockThinking {
    pub fn new(signature: String, text: String) -> Self {
        Self { signature, text }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BlockToolUse {
    pub call: CallId,
    pub name: String,
    pub args: Value,
}

impl BlockToolUse {
    pub fn new(call: CallId, name: String, args: Value) -> Self {
        Self { call, name, args }
    }
}

/// Why a turn ended. Mirrors the wire field of the same name. `Refusal` and
/// `MaxTokens` are the two that change what the loop does next, and neither is
/// inferable from the blocks a turn produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    /// Output truncated: raise `max_tokens` or drop effort.
    MaxTokens,
    /// A server-side tool paused; re-send to resume. Not a failure, and easy to
    /// mistake for a stall.
    PauseTurn,
    /// The API returns HTTP 200 with empty or partial content here. A refusal is
    /// an answer, not an error, and re-sending the same prompt is not a strategy.
    Refusal {
        category: Option<String>,
    },
    StopSequence,
    ToolUse,
}

/// Token accounting for one turn. `input` is new, uncached input only; cached
/// tokens are counted in `cache_read` / `cache_write`. Cost budgets exclude
/// `cache_read` (a cache hit is nearly free); context-window budgets include it
/// (a cached token still occupies the window). See the two budgets in
/// engine.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input: u32,
    pub output: u32,
    pub cache_read: u32,
    pub cache_write: u32,
}

impl Usage {
    pub fn new(input: u32, output: u32, cache_read: u32, cache_write: u32) -> Self {
        Self {
            input,
            output,
            cache_read,
            cache_write,
        }
    }

    /// Zero usage, for turns that produced nothing yet.
    pub fn zero() -> Self {
        Self {
            input: 0,
            output: 0,
            cache_read: 0,
            cache_write: 0,
        }
    }
}

/// How a tool call ended. `TimedOut` is distinct from `Err`: a tool that ran out
/// of time is a context-management signal, not a failure the model should retry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Outcome {
    Denied { reason: String },
    Err { message: String },
    Ok { content: String },
    TimedOut,
}

/// A policy decision, in the journal's own vocabulary. The runtime cache
/// distinguishes a fresh prompt's answer (`Asked*`) from a remembered one
/// (`Remembered*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Decision {
    Allowed,
    AskedAllowed,
    AskedDenied,
    /// No rule spoke, and the kernel's confinement of this process answered
    /// instead. Distinct from `Allowed` because the guarantee behind it is
    /// different: `Allowed` rests on a rule, this rests on the sandbox having
    /// actually been enforcing at the moment of the call.
    ConfinedAllow,
    Denied,
    RememberedAllow,
    RememberedDeny,
}

/// The reach of a user's answer. `Access` records the request; `Scope` records
/// the answer's reach. Carried as written (a string) so a `policy` rule
/// syntax change is never a journal schema migration.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Scope {
    Once,
    Rule { rule: String },
}

/// What was asked for. A structural twin of `policy::Request`; the engine
/// converts at the seam, for the same reason [`Decision`] is duplicated: a
/// schema that names another crate's types is a coupling, not a contract.
///
/// `Read` and `Write` carry a named `path` field rather than wrapping a
/// `PathBuf` directly. That is the serialization constraint, not a style
/// preference: a newtype variant around a string cannot be internally tagged and
/// fails at runtime.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Access {
    Connect(AccessConnect),
    Exec(AccessExec),
    Listen(AccessListen),
    Read { path: PathBuf },
    Write { path: PathBuf },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AccessConnect {
    /// Every address the host resolved to, as the broker resolved it.
    ///
    /// **The host alone cannot answer the question an auditor asks.** A policy
    /// allows hosts by name, so a name that resolves inward -- to loopback, or
    /// to the cloud metadata address -- is a reach into services the network
    /// rules never described, and a journal holding only the name records the
    /// access without recording where it went.
    ///
    /// Empty means no address was recorded, and the `decision` beside it says
    /// which case that is: a denial never reached a lookup, while an allow with
    /// no addresses is a lookup that failed. An record predating this field also
    /// reads as empty, which is what `#[serde(default)]` buys: an older journal
    /// parses rather than failing, at the cost of looking like a failed lookup.
    #[serde(default)]
    pub addrs: Vec<IpAddr>,
    pub host: String,
    pub port: u16,
}

impl AccessConnect {
    pub fn new(addrs: Vec<IpAddr>, host: String, port: u16) -> Self {
        Self { addrs, host, port }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AccessExec {
    pub program: String,
    pub args: Vec<String>,
}

impl AccessExec {
    pub fn new(program: String, args: Vec<String>) -> Self {
        Self { program, args }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AccessListen {
    /// The address the broker actually bound, not the one asked for. The broker
    /// chooses the address from policy, so this records the safe or dangerous
    /// case in the audit trail.
    pub addr: IpAddr,
    pub port: u16,
}

impl AccessListen {
    pub fn new(addr: IpAddr, port: u16) -> Self {
        Self { addr, port }
    }
}
