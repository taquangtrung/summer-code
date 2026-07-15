//! Assembling an engine, and resuming one from a journal.
//!
//! Every seam is injected: the provider, the dispatcher, the broker, the journal
//! sink, the context manager, the hooks. Nothing here reaches for a default that
//! touches the outside world, which is why a test builds the same `Engine` the
//! CLI does and differs only in what it passes.
//!
//! **The opt-in flags are opt-in for one shared reason.** `title_sessions` and
//! `summarize_compactions` each add a `Provider::turn` the loop did not ask for,
//! and [`provider::Recorded`] answers the Nth call with the Nth recorded turn
//! while ignoring the request. One extra call therefore shifts every later turn
//! by one, silently, because the loop cannot tell `Recorded` from a live model.
//! Interactive and headless sessions turn them on; the golden suite and
//! `bench --replay` leave them off.

use std::io::{BufReader, Read};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};

use journal::{Journal, JournalSink, RecordId, RecordKind, SessionId, read_records};
use provider::Provider;
use sandbox::{Broker, PermissionRecord};
use tools::{Dispatcher, Mechanisms};

use crate::context::ContextManager;
use crate::digest::{policy_digest, prompt_digest, tools_digest};
use crate::engine::{DelegatedRun, Engine, EngineError};
use crate::hook::HookSet;
use crate::limits::{Limits, Spend};
use crate::recorder::Recorder;
use crate::session::{self, HistoryEntry, ResumeError};
use crate::skill_gate::SkillGate;
use crate::ui_event::Observer;

// ========================================================================
// Data Structures
// ========================================================================

/// Builds an engine with every seam injected.
pub struct EngineBuilder {
    broker: Arc<Broker>,
    /// A context manager a caller chose, or `None` to size one to the model.
    context: Option<ContextManager>,
    cwd: PathBuf,
    delegated: Receiver<DelegatedRun>,
    dispatcher: Dispatcher,
    hooks: HookSet,
    limits: Limits,
    mechanisms: Mechanisms,
    observer: Option<Observer>,
    permissions: Receiver<PermissionRecord>,
    provider: Box<dyn Provider>,
    sink: Box<dyn JournalSink>,
    skills: SkillGate,
    /// Whether compaction asks the model to summarize what it drops. Off by
    /// default for the reason `title_sessions` is, and for one more: `Recorded`
    /// replay is *positional* over `Provider::turn`, so an extra call here
    /// would consume a recorded turn and desynchronize every turn after it.
    /// See [`EngineBuilder::summarize_compactions`].
    summarize_compactions: bool,
    system_prompt: String,
    /// Whether a session's first turn also titles it. Off by default: every
    /// test that scripts a [`provider::MockProvider`] by the exact number of
    /// round trips its turn makes would otherwise need one more script for a
    /// call it never asked to test. See [`EngineBuilder::title_sessions`].
    title_sessions: bool,
    /// Whether this engine's broker reports its decisions to another engine's
    /// journal, which decides whether the runtime properties can mean anything
    /// here. See [`EngineBuilder::audited_elsewhere`].
    audited_elsewhere: bool,
}
// ========================================================================
// EngineBuilder Implementations
// ========================================================================

impl EngineBuilder {
    /// `permissions` is the receiving half of the channel the broker was given
    /// the sender for. The two halves are separate parameters rather than
    /// something the builder makes, because the broker has to exist first: it
    /// is what the dispatcher's tools were built against.
    pub fn new(
        cwd: PathBuf,
        system_prompt: String,
        provider: Box<dyn Provider>,
        dispatcher: Dispatcher,
        broker: Arc<Broker>,
        sink: Box<dyn JournalSink>,
        permissions: Receiver<PermissionRecord>,
    ) -> Self {
        // A dropped sender by default: a frontend that wires no delegation has
        // an engine that drains an empty channel, which costs one `try_recv`
        // per batch and changes nothing.
        let (_, delegated) = channel();
        Self {
            broker,
            context: None,
            cwd,
            delegated,
            dispatcher,
            hooks: HookSet::default(),
            limits: Limits::defaults(),
            mechanisms: Mechanisms::default(),
            observer: None,
            permissions,
            provider,
            sink,
            skills: SkillGate::default(),
            summarize_compactions: false,
            system_prompt,
            title_sessions: false,
            audited_elsewhere: false,
        }
    }

    pub fn limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Which quality mechanisms the loop runs. All of them by default, which is
    /// the shipped loop; the ablation harness is what constructs one with a
    /// mechanism off, so a mechanism that does not move a metric can be deleted
    /// rather than argued about.
    ///
    /// The dispatcher's tool set is built from the same value by the caller, so
    /// one [`Mechanisms`] names the whole experiment: this method owns the
    /// engine's gates, and the caller passes the value on to the tools whose
    /// affordances are also mechanisms.
    pub fn mechanisms(mut self, mechanisms: Mechanisms) -> Self {
        self.mechanisms = mechanisms;
        self
    }

    /// Override the context manager (token budget, compaction policy). Tests
    /// set a small budget to force compaction. Without it the budget is sized
    /// to the provider's model, and re-sized when the provider is swapped; a
    /// context set here stays put.
    pub fn context(mut self, context: ContextManager) -> Self {
        self.context = Some(context);
        self
    }

    /// Attach the session's hooks. Empty by default, which is the loop with no
    /// hook machinery in its path at all.
    pub fn hooks(mut self, hooks: HookSet) -> Self {
        self.hooks = hooks;
        self
    }

    /// Attach the channel sub-sessions report their spend on.
    ///
    /// The frontend owns the sending half, because it is what builds the
    /// sub-sessions. Without this the tokens a delegated run burned are invisible
    /// here: they are journaled in the sub-session's own file, and the parent's
    /// runaway guard, statusline and usage dashboard all read the parent's.
    pub fn delegated(mut self, delegated: Receiver<DelegatedRun>) -> Self {
        self.delegated = delegated;
        self
    }

    /// Say that this engine's broker reports its decisions elsewhere, so the
    /// runtime properties are not run here.
    ///
    /// For a sub-session sharing a parent's broker: a broker has one decisions
    /// channel, the parent owns it, and properties that read `Permission` records
    /// from a journal that receives none would flag every call this session
    /// made. The accesses are audited in the parent's journal, under the call
    /// that spawned this one.
    pub fn audited_elsewhere(mut self) -> Self {
        self.audited_elsewhere = true;
        self
    }

    /// Attach the session's skill gate. Empty by default, which screens no
    /// edit: a user who configured no rules gets the loop exactly as it was.
    pub fn skill_gate(mut self, skills: SkillGate) -> Self {
        self.skills = skills;
        self
    }

    /// Attach a UI observer. Absent by default, so the plain REPL and every
    /// test build an engine that emits nothing and behaves exactly as before.
    pub fn observer(mut self, observer: Observer) -> Self {
        self.observer = Some(observer);
        self
    }

    /// Title this session from its first exchange, right after that turn
    /// completes. Off by default: a frontend with a `/resume` picker to feed
    /// wants this, a one-shot script or a test scripting a provider by its
    /// exact round-trip count does not.
    pub fn title_sessions(mut self) -> Self {
        self.title_sessions = true;
        self
    }

    /// Ask the model to summarize each range compaction drops, instead of
    /// recording the mechanical count of what went.
    ///
    /// **Off by default, and the default is not timidity.** `Recorded` replays a
    /// journal *positionally* over `Provider::turn`: it answers the Nth call
    /// with the Nth recorded turn and ignores the request. An extra call here
    /// would therefore consume a recorded turn, and every turn after the first
    /// compaction would replay the wrong one -- silently, since the loop cannot
    /// tell `Recorded` from a live model. `bench --replay` and the golden suite
    /// leave this off and keep the mechanical summary, which is deterministic
    /// and needs no provider; an interactive frontend turns it on.
    ///
    /// A failure is not fatal: see `Engine::apply_compaction`, which falls
    /// back to the mechanical summary rather than failing the turn.
    pub fn summarize_compactions(mut self) -> Self {
        self.summarize_compactions = true;
        self
    }

    /// Assemble the engine and record its `SessionStarted` root record.
    pub fn build(self) -> Result<Engine, EngineError> {
        let prompt_digest = prompt_digest(&self.system_prompt);
        let tools_digest = tools_digest(self.dispatcher.specs());
        let policy_digest = policy_digest(self.broker.policy());

        let session = SessionId::new();
        let mut engine = self.assemble(session, None, Vec::new(), Spend::default());
        let model = engine.provider.model().to_string();
        engine.record(RecordKind::SessionStarted(journal::SessionStarted::new(
            journal::SCHEMA_VERSION,
            engine.cwd.to_path_buf(),
            model,
            policy_digest,
            prompt_digest,
            tools_digest,
        )))?;
        Ok(engine)
    }

    /// Resume a session from its journal, folding state from the walk at `head`.
    ///
    /// Every seam a fresh engine needs is needed here too: a resumed session
    /// still calls the model and dispatches tools. The difference from
    /// [`build`](Self::build) is only that the session starts folded rather than
    /// empty.
    ///
    /// The fold seeds the broker's grant cache (so a resumed session does not
    /// re-ask what the user already approved) and surfaces `open_calls` — tools
    /// in flight when the process died — which resume answers with a synthetic
    /// outcome rather than re-dispatching a half-applied write.
    pub fn resume(self, journal: impl Read, head: RecordId) -> Result<Engine, ResumeError> {
        let records = read_records(BufReader::new(journal))?;
        let indexed = Journal::from_records(records)?;
        let walk = indexed.walk(head).ok_or(ResumeError::UnknownHead(head))?;
        let state = session::fold(walk)?;

        // Seed the grant cache before the first turn: a resumed session must not
        // re-ask what the user already answered. The cache is derived, not a
        // second store, so it rebuilds from the `Permission` scopes here.
        self.broker.restore(state.granted);

        // Seed the dedup ledger for the same reason, from the `ToolCall` records.
        // Both are derived caches, and a ledger that reset on resume made this
        // module's own claim (a call known to have run is not run again, so a
        // half-applied write is not re-attempted) false precisely across the
        // restart it exists to survive.
        let mut builder = self;
        builder.dispatcher.restore(state.dispatched);

        let mut engine = builder.assemble(state.id, Some(head), state.history, state.spend);
        engine.complete_open_calls(&state.open_calls)?;
        Ok(engine)
    }

    /// Assemble an [`Engine`] from the builder's seams and the session's initial
    /// state. Shared by [`build`](Self::build) (empty state) and
    /// [`resume`](Self::resume) (folded state) so the field list has one home.
    fn assemble(
        self,
        session: SessionId,
        head: Option<RecordId>,
        history: Vec<HistoryEntry>,
        spend: Spend,
    ) -> Engine {
        let recorder = if self.audited_elsewhere {
            Recorder::audited_elsewhere(self.sink)
        } else {
            Recorder::new(self.sink, self.broker.policy())
        };
        let context_pinned = self.context.is_some();
        let context = self
            .context
            .unwrap_or_else(|| ContextManager::for_provider(self.provider.as_ref()));
        Engine {
            broker: self.broker,
            context,
            context_pinned,
            cwd: Arc::from(self.cwd.as_path()),
            delegated: self.delegated,
            dispatcher: self.dispatcher,
            head,
            hooks: self.hooks,
            history,
            limits: self.limits,
            mechanisms: self.mechanisms,
            observer: self.observer,
            permissions: self.permissions,
            provider: self.provider,
            recorder,
            session,
            skills: self.skills,
            spend,
            summarize_compactions: self.summarize_compactions,
            system_prompt: self.system_prompt,
            title_sessions: self.title_sessions,
            turn_cancel: None,
            edited: Vec::new(),
            active_plan: None,
            plan_open: false,
            plan_nudged: false,
            pending_title: None,
            turn_openers: Vec::new(),
            verify_retries: 0,
        }
    }
}
