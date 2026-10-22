//! Running a sub-task in a session of its own.
//!
//! This is the half of the `subagent` tool that `tools` cannot hold: building an
//! engine needs a provider, a dispatcher, a broker and a journal, and `engine`
//! depends on `tools`, so the tool takes a [`SubagentRunner`] and the frontend
//! supplies one. See `tools::subagent` for why the seam is shaped that way.
//!
//! **What the sub-session shares, and what it does not.** It shares the
//! workspace, the broker, and therefore every permission the user has already
//! granted: a subagent is the same user doing the same work, not a lower trust
//! level, and giving it a second policy would mean two answers to "may this
//! session write here". It does not share the conversation, the journal, or the
//! tool set: it starts from an empty context, writes its own session file, and
//! is not given a `subagent` tool of its own.
//!
//! **That last exclusion is the recursion guard.** A subagent that could
//! delegate could delegate to something that delegates, and nothing in the
//! prompt reliably stops it. The tool is simply absent from the sub-session's
//! dispatcher, so the depth is one by construction rather than by instruction.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Sender, channel};

use async_trait::async_trait;
use engine::{Broker, DelegatedRun, Engine, EngineBuilder, Limits, SYSTEM_PROMPT, Stop};
use journal::{JournalSink, SessionId, WriterSink};
use provider::{ProviderConfig, ProviderId, transport};
use tools::{Builtin, Dispatcher, ShellSpec, SubagentRunner, ToolSource};

// ========================================================================
// Constants
// ========================================================================

/// How many rounds a sub-session gets.
///
/// Lower than a session's own ceiling on purpose. A subagent answers one
/// question; one that has taken this many rounds has misunderstood it, and the
/// caller is blocked waiting the whole time.
const MAX_SUB_TURNS: u32 = 24;

/// What the sub-session is told about itself, with its role substituted.
///
/// It repeats what the caller cannot: the sub-session has no view of the
/// conversation that spawned it, so "you were asked this by someone who cannot
/// see your work" has to be said, or the answer comes back as a fragment that
/// only makes sense next to context the caller never sent.
const ROLE_PROMPT: &str = "\n\nYou are running as a subagent in the role: {role}.\n\
     You were delegated one self-contained task by another agent that cannot see \
     anything you do. Only your final message is returned to it, so end with the \
     complete answer: what you found, where, and what it means for the task. Do not \
     ask questions; there is no one to answer them.";

/// The placeholder [`ROLE_PROMPT`] substitutes.
const ROLE_PLACEHOLDER: &str = "{role}";

// ========================================================================
// Data Structures
// ========================================================================

/// Builds and runs one sub-session per call, against the same workspace and
/// broker as the session that asked.
pub struct Nested {
    broker: Arc<Broker>,
    cwd: PathBuf,
    /// Where a finished sub-session's spend is reported, so the parent's
    /// runaway guard and statusline count tokens it caused to be burned.
    spent: Sender<DelegatedRun>,
    /// Cloned per sub-session: a `Provider` is not reusable across engines, and
    /// the config is what a fresh one is built from.
    provider_config: ProviderConfig,
    provider_id: ProviderId,
    shell: ShellSpec,
}

/// A built sub-session and where it is recording, so a failure can name the
/// file to read.
struct SubSession {
    engine: Engine,
    journal: PathBuf,
}

// ========================================================================
// Nested Implementations
// ========================================================================

impl Nested {
    pub fn new(
        broker: Arc<Broker>,
        cwd: PathBuf,
        provider_config: ProviderConfig,
        provider_id: ProviderId,
        shell: ShellSpec,
        spent: Sender<DelegatedRun>,
    ) -> Self {
        Self {
            broker,
            cwd,
            provider_config,
            provider_id,
            shell,
            spent,
        }
    }

    /// One sub-session, run to its answer.
    ///
    /// Every failure here is the harness failing to run the sub-session, which
    /// is why they are all `Err` and all carry their cause: the tool reports it
    /// as "did not run", and a caller that saw an empty `Ok` instead would read
    /// a broken journal file as "found nothing".
    fn build(&self, role: &str) -> Result<SubSession, Box<dyn Error>> {
        let (decisions, permissions) = channel();
        // The sub-session's own broker records, dropped rather than journaled
        // twice: the parent's receiver is already attached to the parent engine,
        // and a second receiver on the same broker would split the record stream
        // between two journals unpredictably. What the sub-session did is in its
        // own journal; what it was allowed to do is in the parent's audit trail,
        // because the grants are the parent's.
        drop(decisions);

        let system = format!(
            "{SYSTEM_PROMPT}{}",
            ROLE_PROMPT.replace(ROLE_PLACEHOLDER, role)
        );
        // No `with_subagent`: the recursion guard, by construction.
        let dispatcher = Dispatcher::new(Builtin::with_shell(self.shell.clone()).tools())?;
        let (sink, path) = journal_for(&self.cwd)?;

        let engine = EngineBuilder::new(
            self.cwd.clone(),
            system,
            transport(self.provider_id, self.provider_config.clone()),
            dispatcher,
            Arc::clone(&self.broker),
            sink,
            permissions,
        )
        .limits(Limits {
            max_turns: MAX_SUB_TURNS,
            ..Limits::defaults()
        })
        // The broker's decisions go to the parent's journal, so the properties
        // that read them cannot mean anything here; without this, every call the
        // sub-session made was reported as an unaudited success.
        .audited_elsewhere()
        .build()?;
        Ok(SubSession {
            engine,
            journal: path,
        })
    }
}

#[async_trait]
impl SubagentRunner for Nested {
    async fn run(&self, prompt: &str, role: &str) -> Result<String, String> {
        let mut session = self.build(role).map_err(|error| error.to_string())?;
        let outcome = session.engine.turn(prompt.to_string()).await;

        // Reported on both paths, before the error is: a sub-session that failed
        // partway still burned what it burned, and dropping that on the failure
        // path would make a retried delegation cost less on paper than it did in
        // fact. A closed channel means the parent is already gone.
        let _ = self.spent.send(DelegatedRun {
            journal: session.journal.display().to_string(),
            usage: session.engine.spent(),
        });

        let summary = outcome
            .map_err(|error| format!("{error} (session file: {})", session.journal.display()))?;
        // The stop reason travels with the text. A subagent that hit its turn
        // ceiling has a partial answer, and the caller deciding what to do with
        // it needs to know that rather than reading it as complete.
        Ok(match summary.stop {
            Stop::Done => summary.text,
            other => format!(
                "[the subagent stopped early: {other:?}; what it had is below]\n\n{}",
                summary.text
            ),
        })
    }
}

// ========================================================================
// Functions
// ========================================================================

/// A sub-session's journal, in the same directory as the parent's.
///
/// Its own file, because the point of the tool is that the two contexts are
/// separate: interleaving them would make "what did the subagent do" a filtering
/// question over someone else's turns.
fn journal_for(cwd: &Path) -> Result<(Box<dyn JournalSink>, PathBuf), Box<dyn Error>> {
    let dir = cwd.join(".summer").join("sessions");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}-subagent.jsonl", SessionId::new()));
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    Ok((Box::new(WriterSink::new(file)), path))
}
