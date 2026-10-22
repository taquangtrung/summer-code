//! Listing, previewing, and resuming past sessions.
//!
//! A session is a journal file, so every one of these is a fold over records: the
//! picker's rows, the preview line, and the resumed history all come from
//! reading the same JSONL rather than from a separate index that could disagree
//! with it. Nothing here writes.

use std::error::Error;

use engine::Engine;

use crate::tui::state::{
    Item, PickerDisplay, PickerStage, SessionRow, TuiState, format_age, rebuild_transcript,
    truncate,
};
use crate::tui::{DriverCx, PickerCx};

// ========================================================================
// Constants
// ========================================================================

/// The name of a session nothing has been said in yet.
const UNTITLED: &str = "untitled session";

// ========================================================================
// Functions
// ========================================================================

/// `/sessions`: the session journals in this workspace's store (wherever
/// bootstrap put it — central or in-project), newest first. `/resume` will
/// pick from this list.
pub(super) fn sessions_text(state: &TuiState) -> String {
    let dir = std::path::Path::new(&state.sessions_dir);
    let rows = list_sessions(dir);
    if rows.is_empty() {
        return format!("sessions: none at {}", dir.display());
    }
    let mut out = String::from("sessions (newest first):");
    for row in &rows {
        out.push_str(&format!(
            "\n  {:>10}  {}  ({})",
            row.age, row.preview, row.id
        ));
    }
    out.push_str("\n  (/resume picks from this list; /resume <session-id> names one)");
    out
}

/// `/resume` with no id: open the session picker over this project's sessions,
/// newest first. The prompt doubles as the filter box, exactly as `/model`'s
/// picker does.
pub(super) fn open_session_picker(cx: &mut DriverCx<'_>) {
    let rows = list_sessions(std::path::Path::new(&cx.state.sessions_dir));
    if rows.is_empty() {
        cx.state
            .notify(format!("no sessions at {} yet", cx.state.sessions_dir));
        return;
    }
    cx.input.clear();
    cx.state.pending_picker = Some(PickerDisplay {
        selected: 0,
        stage: PickerStage::Sessions { rows },
    });
}

/// Esc Esc: list the messages that opened this session's turns, to rewind to.
pub(super) fn open_rewind_picker(cx: &mut DriverCx<'_>) {
    let rows = cx
        .engine_slot
        .as_ref()
        .map(Engine::rewindable_turns)
        .unwrap_or_default();
    if rows.is_empty() {
        cx.state
            .notify("nothing to rewind to yet: no turn has run in this session");
        return;
    }
    cx.state.notify(
        "rewind to before which message? It comes back to edit; files stay as they are (/rewind restores them)",
    );
    cx.state.pending_picker = Some(PickerDisplay {
        selected: 0,
        stage: PickerStage::Rewind { rows },
    });
}

/// Rewind the conversation to just before the `turns`-th last turn, whose
/// opening message was `text`, and hand that message back to edit.
///
/// The transcript loses the same turns the engine does, counted from the end:
/// the engine's list covers only the turns this process ran, and those are
/// always the newest user rows on screen, however much was resumed before them.
pub(super) fn rewind_conversation(cx: &mut PickerCx<'_>, turns: usize, text: &str) {
    let Some(engine) = cx.engine_slot.as_mut() else {
        return;
    };
    match engine.rewind_turns(turns) {
        Ok(true) => {
            let start = cx
                .state
                .transcript
                .iter()
                .enumerate()
                .filter(|(_, item)| matches!(item, Item::User(_)))
                .map(|(index, _)| index)
                .rev()
                .nth(turns - 1);
            if let Some(start) = start {
                cx.state.transcript.truncate(start);
            }
            cx.state.plan = None;
            cx.input.insert_str(text);
            cx.state.notify(format!(
                "rewound {turns} turn{}; the message is back to edit",
                if turns == 1 { "" } else { "s" }
            ));
        }
        Ok(false) => cx
            .state
            .notify("that turn was already compacted away and cannot be rewound to"),
        Err(error) => cx.state.notify_error(format!("could not rewind: {error}")),
    }
}

/// `/resume <id>`: fold that session's journal in and keep going. `/resume`
/// with no id opens the picker instead (see [`open_session_picker`]).
pub(super) fn resume_session(
    engine_slot: &mut Option<Engine>,
    state: &mut TuiState,
    id: String,
) -> Result<(), Box<dyn Error>> {
    let path = std::path::Path::new(&state.sessions_dir).join(format!("{id}.jsonl"));
    let Some(mut engine) = engine_slot.take() else {
        return Ok(());
    };
    let name = session_name(&path);
    let mut result = Ok(0);
    if let Err(error) = fold_journal_into(&mut engine, &path, state) {
        result = Err(error);
    }
    *engine_slot = Some(engine);
    match result {
        Ok(count) => {
            state.notify(format!(
                "resumed \"{name}\" ({count} records). The model has the prior context; \
                 new turns continue in this session."
            ));
        }
        Err(error) => state.notify_error(format!("resume: {id}: {error}")),
    }
    Ok(())
}

/// Pick up the running session's generated title once the engine has recorded
/// it, for the surfaces that name the session while it runs (the window title,
/// the picker's active row). The engine records it a turn or so after the
/// first, so this rereads the journal after each turn until it finds one, and
/// never again after.
pub(super) fn learn_session_title(state: &mut TuiState) {
    if state.session_title.is_some() || state.session_id.is_empty() {
        return;
    }
    let path =
        std::path::Path::new(&state.sessions_dir).join(format!("{}.jsonl", state.session_id));
    let Ok(file) = std::fs::File::open(path) else {
        return;
    };
    let Ok(records) = journal::read_records(std::io::BufReader::new(file)) else {
        return;
    };
    state.session_title = records.iter().find_map(|record| match &record.kind {
        journal::RecordKind::SessionTitled { title } => Some(title.clone()),
        _ => None,
    });
}

/// This project's sessions, newest first, each with a one-line preview (see
/// [`session_name`]) so a picker row says what the session was about. Shared by
/// `/resume`'s picker and `/sessions`' listing, so the two lists agree.
fn list_sessions(dir: &std::path::Path) -> Vec<SessionRow> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut rows: Vec<(std::time::SystemTime, SessionRow)> = entries
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "jsonl"))
        .filter_map(|entry| {
            let id = entry.path().file_stem()?.to_string_lossy().to_string();
            let modified = entry.metadata().ok()?.modified().ok()?;
            let preview = session_name(&entry.path());
            let age = format_age(modified);
            Some((modified, SessionRow { age, id, preview }))
        })
        .collect();
    rows.sort_by_key(|row| std::cmp::Reverse(row.0));
    rows.into_iter().map(|(_, row)| row).collect()
}

/// What a session is called wherever it is shown: the journal's generated title
/// if it has one, else its first user message's first line, truncated either
/// way. Its id is a ULID, unique but unreadable, so it is shown only where it
/// has to be typed (`/sessions`, for `/resume <id>`). A title is recorded once,
/// after the session's first turn, so a session still on its first turn (or
/// one from before this existed) has none yet and falls back. A session with
/// no message yet is untitled.
pub(super) fn session_name(path: &std::path::Path) -> String {
    // Not written yet is the newest session there is, not a broken one.
    let Ok(file) = std::fs::File::open(path) else {
        return UNTITLED.into();
    };
    let Ok(records) = journal::read_records(std::io::BufReader::new(file)) else {
        return "(unreadable)".into();
    };
    for record in &records {
        if let journal::RecordKind::SessionTitled { title } = &record.kind {
            return truncate(title, 70);
        }
    }
    for record in &records {
        if let journal::RecordKind::UserMessage { text } = &record.kind {
            let line = text.lines().next().unwrap_or("");
            return truncate(line, 70);
        }
    }
    UNTITLED.into()
}

/// `/resume <id>`: fold a past session's journal into the running engine, so the
/// model continues with that context. The engine keeps its own broker, provider,
/// and journal — only its history is replaced — so the sandbox and prompt
/// channel are unchanged.
/// Fold a journal into the running engine and rebuild the on-screen transcript
/// from it — the one body behind `summer -c` (startup) and `/resume` (live), so
/// the two cannot drift in what "resumed" means: the model keeps the context,
/// the grants and dedup ledger carry over (engine side), and the user sees the
/// conversation they are continuing rather than an empty screen.
pub(super) fn fold_journal_into(
    engine: &mut Engine,
    path: &std::path::Path,
    state: &mut TuiState,
) -> Result<usize, Box<dyn Error>> {
    let file = std::fs::File::open(path)?;
    let count = engine.resume(file)?;
    let records = journal::read_records(std::io::BufReader::new(std::fs::File::open(path)?))?;
    state.transcript = rebuild_transcript(&records);
    Ok(count)
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {

    use journal::{Record, RecordId, RecordKind, SessionId, SessionStarted, write_records};

    use super::*;

    /// Write `records` as a journal file under a fresh temp dir and return its
    /// path, for a test that reads it back through [`session_name`].
    fn journal_fixture(records: &[Record]) -> std::path::PathBuf {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.keep().join("session.jsonl");
        let mut buffer = Vec::new();
        write_records(&mut buffer, records).unwrap();
        std::fs::write(&path, buffer).unwrap();
        path
    }

    #[test]
    fn session_name_prefers_the_generated_title_over_the_first_user_message() {
        // A session on its first turn (or one journaled before titling
        // existed) has no `SessionTitled` record yet, and must still fall back
        // to the first user message -- the picker's original behavior.
        let session = SessionId::new();
        let started = Record {
            at: std::time::SystemTime::now(),
            id: RecordId::new(),
            kind: RecordKind::SessionStarted(SessionStarted::new(
                0,
                "/repo".into(),
                "mock".into(),
                "sha256:policy".into(),
                "sha256:prompt".into(),
                "sha256:tools".into(),
            )),
            parent: None,
            session,
        };
        let user_message = Record {
            at: std::time::SystemTime::now(),
            id: RecordId::new(),
            kind: RecordKind::UserMessage {
                text: "please add a README badge\nand nothing else".into(),
            },
            parent: Some(started.id),
            session,
        };
        assert_eq!(
            session_name(&journal_fixture(&[started.clone(), user_message.clone()])),
            "please add a README badge",
            "no title yet: falls back to the first line of the first user message"
        );

        let titled = Record {
            at: std::time::SystemTime::now(),
            id: RecordId::new(),
            kind: RecordKind::SessionTitled {
                title: "Add a README badge".into(),
            },
            parent: Some(user_message.id),
            session,
        };
        assert_eq!(
            session_name(&journal_fixture(&[started, user_message, titled])),
            "Add a README badge",
            "a title, once recorded, replaces the first-user-message preview"
        );
    }

    #[test]
    fn the_running_session_learns_its_title_once_the_journal_has_one() {
        let session = SessionId::new();
        let titled = Record {
            at: std::time::SystemTime::now(),
            id: RecordId::new(),
            kind: RecordKind::SessionTitled {
                title: "Fix the flaky login test".into(),
            },
            parent: None,
            session,
        };
        let path = journal_fixture(&[titled]);
        let mut state = crate::tui::testing::state_with(0, &[]);
        state.sessions_dir = path.parent().unwrap().display().to_string();
        state.session_id = "missing".into();
        learn_session_title(&mut state);
        assert_eq!(state.session_title, None, "no journal yet, no title");

        state.session_id = "session".into();
        learn_session_title(&mut state);
        assert_eq!(
            state.session_title.as_deref(),
            Some("Fix the flaky login test")
        );
    }
}
