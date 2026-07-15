//! A content-addressed store of what files looked like before the agent changed
//! them, and the restore that undoes a session back to a chosen point.
//!
//! **This is what the event-sourced journal was always for.** The journal
//! already replays a session's *decisions* exactly; what it could not replay was
//! the workspace, because the bytes a file held before an edit were nowhere. A
//! blob per pre-write state plus an append-only manifest closes that, and
//! "rewind to turn 4 and take a different path" becomes a supported operation
//! rather than `git checkout` and a guess about which changes were the agent's.
//!
//! # Why a sibling file rather than journal records
//!
//! Snapshots are captured by the broker, which runs inside a spawned tool task.
//! The engine is the journal's only writer and takes `&mut self` to do it, so a
//! tool task cannot append there, and routing every capture through a channel
//! and a drain would make recording history contend with making it. The manifest
//! is therefore its own append-only JSONL beside the journal, with the same
//! discipline: one line per capture, written under a lock, never rewritten.
//!
//! The join between them is the ULID clock. Manifest entries and journal records
//! both carry timestamps from the same monotonic source, so "the workspace as of
//! this record" is a prefix of the manifest, which is exactly what
//! [`Snapshots::restore_before`] takes.
//!
//! # Deduplication
//!
//! Blobs are named by the SHA-256 of their contents, so a file written ten times
//! in a session with the same content costs one blob. The manifest still records
//! ten entries, because *when* a state was current is the thing being replayed
//! and is not recoverable from the blob set.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use sandbox::SnapshotSink;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::warn;

// ========================================================================
// Constants
// ========================================================================

/// Where the manifest lives, inside the store directory the caller opens.
const MANIFEST: &str = "manifest.jsonl";

/// The largest file that is snapshotted.
///
/// A cap, because the store is on the critical path of every write. Above it the
/// capture is skipped and recorded as skipped: a rewind that silently restored
/// nothing would be worse than one that says which files it could not cover.
const MAX_BLOB_BYTES: u64 = 8 * 1024 * 1024;

// ========================================================================
// Data Structures
// ========================================================================

/// The store: blobs on disk plus an append-only manifest.
pub struct Snapshots {
    /// The manifest handle, held open for the session. Behind a `Mutex` because
    /// [`SnapshotSink::capture`] takes `&self` and several tool tasks can be
    /// writing at once now that dispatch batches.
    manifest: Mutex<Option<fs::File>>,
    root: PathBuf,
    store: PathBuf,
}

/// One manifest line: a file's state at the moment before a write.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    /// Written RFC 3339 millis UTC, matching the journal beside it rather than
    /// serde's `{secs_since_epoch, ...}` default. Two time formats in one
    /// session's records is one too many.
    #[serde(with = "journal::timestamp")]
    pub at: SystemTime,
    /// The path, relative to the workspace root.
    pub path: PathBuf,
    pub state: State,
}

/// What a file held before the write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum State {
    /// The file did not exist. Restoring this state deletes it.
    ///
    /// The variant that makes "undo a file the agent created" work at all: with
    /// only content states recorded, a created file has no prior state and a
    /// rewind leaves it behind.
    Absent,
    /// The file held the blob named by this digest.
    Blob { digest: String },
    /// The file was too large to capture, or could not be read.
    ///
    /// Recorded rather than omitted so a restore can say what it did not cover
    /// instead of quietly leaving a modified file in place.
    Skipped { reason: String },
}

/// What a restore did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Restored {
    /// Files deleted because they did not exist at the target point.
    pub deleted: Vec<PathBuf>,
    /// Files whose prior contents could not be restored, and why.
    pub failed: Vec<(PathBuf, String)>,
    /// Files rewritten with their prior contents.
    pub written: Vec<PathBuf>,
}

// ========================================================================
// Snapshots Implementations
// ========================================================================

impl Snapshots {
    /// Open (or create) the store at `store`, scoping restores to `root`.
    ///
    /// The two paths are separate now that stores live outside the workspace
    /// (Claude Code's `~/.summer/projects/<encoded-root>` layout): `root` is
    /// the workspace every restored path is re-anchored under, `store` is
    /// wherever this session's blobs and manifest live — central for an
    /// unconfined session, in-project under `--sandbox`, where a
    /// workspace-rooted Landlock domain cannot reach `~`.
    ///
    /// A store that cannot be created is not fatal: the session runs without
    /// snapshots, and `capture` becomes a warning-per-write rather than an
    /// error the user cannot act on mid-turn.
    pub fn open(root: &Path, store: &Path) -> Self {
        let store = store.to_path_buf();
        let manifest = fs::create_dir_all(&store)
            .and_then(|()| {
                fs::OpenOptions::new()
                    .append(true)
                    .create(true)
                    .open(store.join(MANIFEST))
            })
            .inspect_err(|error| {
                warn!(store = %store.display(), %error, "snapshots are unavailable this session");
            })
            .ok();
        Self {
            manifest: Mutex::new(manifest),
            root: root.to_path_buf(),
            store,
        }
    }

    /// Every manifest entry, oldest first.
    pub fn entries(&self) -> Vec<Entry> {
        let Ok(text) = fs::read_to_string(self.store.join(MANIFEST)) else {
            return Vec::new();
        };
        text.lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    /// Restore the workspace to how it looked before `at`.
    ///
    /// **The earliest entry per path wins, and that is the whole algorithm.** To
    /// undo everything from `at` onward, each file must go back to the state it
    /// held *before the first* write at or after `at`, not the state before the
    /// last one. Taking the last would restore a file to an intermediate version
    /// the agent itself produced.
    pub fn restore_before(&self, at: SystemTime) -> Restored {
        // Compared at the manifest's own resolution. Entries are written RFC
        // 3339 millis, so a capture reads back truncated down to the
        // millisecond; comparing that against a full-precision boundary taken
        // in the same millisecond puts the capture *before* the boundary and
        // silently drops it from the rewind. Truncating the boundary too makes
        // a tie inclusive, which errs toward restoring -- the safe direction for
        // an undo.
        let at = truncate_to_millis(at);
        let mut targets: BTreeMap<PathBuf, State> = BTreeMap::new();
        for entry in self.entries().into_iter().filter(|entry| entry.at >= at) {
            targets.entry(entry.path).or_insert(entry.state);
        }

        let mut restored = Restored::default();
        for (path, state) in targets {
            let full = self.root.join(&path);
            match state {
                State::Absent => match remove_if_present(&full) {
                    Ok(()) => restored.deleted.push(path),
                    Err(error) => restored.failed.push((path, error)),
                },
                State::Blob { digest } => match self.write_blob_to(&digest, &full) {
                    Ok(()) => restored.written.push(path),
                    Err(error) => restored.failed.push((path, error)),
                },
                State::Skipped { reason } => restored.failed.push((path, reason)),
            }
        }
        restored
    }

    /// Copy the blob named `digest` over `target`.
    fn write_blob_to(&self, digest: &str, target: &Path) -> Result<(), String> {
        let blob = self.store.join(digest);
        let contents = fs::read(&blob).map_err(|error| format!("{digest}: {error}"))?;
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
        }
        fs::write(target, contents).map_err(|error| format!("{}: {error}", target.display()))
    }

    /// Append one entry to the manifest.
    fn append(&self, entry: &Entry) {
        let Ok(line) = serde_json::to_string(entry) else {
            return;
        };
        let Ok(mut guard) = self.manifest.lock() else {
            return;
        };
        let Some(file) = guard.as_mut() else {
            return;
        };
        // Best-effort, like the rest of this module: a manifest that cannot be
        // appended to must not fail the write it is describing.
        if let Err(error) = writeln!(file, "{line}") {
            warn!(%error, "a snapshot entry could not be recorded");
        }
    }

    /// Write `contents` as a blob, returning its digest.
    fn store_blob(&self, contents: &[u8]) -> Result<String, String> {
        let digest = format!("{:x}", Sha256::digest(contents));
        let blob = self.store.join(&digest);
        // Content-addressed, so an existing blob with this name already holds
        // exactly these bytes and rewriting it would be pure I/O.
        if blob.exists() {
            return Ok(digest);
        }
        fs::write(&blob, contents).map_err(|error| format!("{}: {error}", blob.display()))?;
        Ok(digest)
    }

    /// The state `path` is currently in, ready to be recorded.
    fn current_state(&self, path: &Path) -> State {
        let metadata = match fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return State::Absent,
            Err(error) => {
                return State::Skipped {
                    reason: format!("could not be read: {error}"),
                };
            }
        };
        if metadata.len() > MAX_BLOB_BYTES {
            return State::Skipped {
                reason: format!("{} bytes exceeds the snapshot cap", metadata.len()),
            };
        }
        match fs::read(path).map_err(|error| error.to_string()) {
            Ok(contents) => match self.store_blob(&contents) {
                Ok(digest) => State::Blob { digest },
                Err(reason) => State::Skipped { reason },
            },
            Err(reason) => State::Skipped { reason },
        }
    }
}

impl SnapshotSink for Snapshots {
    fn capture(&self, path: &Path) {
        // The store lives under the root, and the agent may not write there
        // (`deny_write("**/.summer/**")`), so this is defence against a future
        // policy change rather than a live path: snapshotting the snapshots
        // would recurse.
        if path.starts_with(&self.store) {
            return;
        }
        let relative = path.strip_prefix(&self.root).unwrap_or(path).to_path_buf();
        self.append(&Entry {
            at: SystemTime::now(),
            path: relative,
            state: self.current_state(path),
        });
    }
}

// ========================================================================
// Functions
// ========================================================================

/// `time` with its sub-millisecond part dropped, matching what the manifest
/// stores.
fn truncate_to_millis(time: SystemTime) -> SystemTime {
    let Ok(since_epoch) = time.duration_since(std::time::UNIX_EPOCH) else {
        // A time before the epoch is not something this store can produce; it
        // is returned unchanged rather than being an error nobody can act on.
        return time;
    };
    std::time::UNIX_EPOCH + std::time::Duration::from_millis(since_epoch.as_millis() as u64)
}

/// Delete `path` if it is there, treating "already gone" as success.
fn remove_if_present(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn store(root: &Path) -> Snapshots {
        Snapshots::open(root, &root.join("snapshots"))
    }

    #[test]
    fn a_prior_version_is_restored_over_the_agents_edit() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("src.rs");
        fs::write(&file, "original").unwrap();

        let snapshots = store(dir.path());
        let before = SystemTime::now();
        snapshots.capture(&file);
        fs::write(&file, "the agent's version").unwrap();

        let restored = snapshots.restore_before(before);
        assert_eq!(fs::read_to_string(&file).unwrap(), "original");
        assert_eq!(restored.written.len(), 1);
        assert!(restored.failed.is_empty(), "{restored:?}");
    }

    #[test]
    fn a_file_the_agent_created_is_deleted_by_a_rewind() {
        // Without `State::Absent` a created file has no prior state and survives
        // the rewind, which is the most visible way an undo can be wrong.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("new.rs");

        let snapshots = store(dir.path());
        let before = SystemTime::now();
        snapshots.capture(&file);
        fs::write(&file, "brand new").unwrap();
        assert!(file.exists());

        let restored = snapshots.restore_before(before);
        assert!(!file.exists(), "a created file survived the rewind");
        assert_eq!(restored.deleted.len(), 1);
    }

    #[test]
    fn the_earliest_state_wins_when_a_file_was_written_twice() {
        // The bug this catches: taking the *last* state before the target
        // restores a file to an intermediate version the agent itself wrote, so
        // the rewind lands somewhere that never existed before the session.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("edited.rs");
        fs::write(&file, "v0").unwrap();

        let snapshots = store(dir.path());
        let before = SystemTime::now();
        snapshots.capture(&file);
        fs::write(&file, "v1").unwrap();
        // The clock has millisecond resolution in the manifest; a sleep keeps
        // the two entries distinguishable rather than relying on ordering alone.
        std::thread::sleep(Duration::from_millis(5));
        snapshots.capture(&file);
        fs::write(&file, "v2").unwrap();

        snapshots.restore_before(before);
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            "v0",
            "the rewind should reach the state before the first capture"
        );
    }

    #[test]
    fn a_capture_after_the_target_point_is_untouched() {
        // A rewind must not undo work the user did before the point they chose.
        let dir = tempfile::tempdir().unwrap();
        let early = dir.path().join("early.rs");
        let late = dir.path().join("late.rs");
        fs::write(&early, "early original").unwrap();
        fs::write(&late, "late original").unwrap();

        let snapshots = store(dir.path());
        snapshots.capture(&early);
        fs::write(&early, "early edited").unwrap();
        std::thread::sleep(Duration::from_millis(5));

        let cut = SystemTime::now();
        snapshots.capture(&late);
        fs::write(&late, "late edited").unwrap();

        snapshots.restore_before(cut);
        assert_eq!(fs::read_to_string(&late).unwrap(), "late original");
        assert_eq!(
            fs::read_to_string(&early).unwrap(),
            "early edited",
            "a rewind reached back past the point it was given"
        );
    }

    #[test]
    fn identical_contents_share_one_blob() {
        let dir = tempfile::tempdir().unwrap();
        let one = dir.path().join("a.rs");
        let two = dir.path().join("b.rs");
        fs::write(&one, "same").unwrap();
        fs::write(&two, "same").unwrap();

        let snapshots = store(dir.path());
        snapshots.capture(&one);
        snapshots.capture(&two);

        let blobs = fs::read_dir(dir.path().join("snapshots"))
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name() != MANIFEST)
            .count();
        assert_eq!(blobs, 1, "identical contents stored twice");
        assert_eq!(snapshots.entries().len(), 2, "both captures still recorded");
    }

    #[test]
    fn an_oversized_file_is_recorded_as_skipped_not_omitted() {
        // Omitting it makes a rewind silently leave a modified file in place,
        // and nothing anywhere says so.
        let dir = tempfile::tempdir().unwrap();
        let big = dir.path().join("big.bin");
        fs::write(&big, vec![0u8; (MAX_BLOB_BYTES + 1) as usize]).unwrap();

        let snapshots = store(dir.path());
        let before = SystemTime::now();
        snapshots.capture(&big);

        let entries = snapshots.entries();
        assert_eq!(entries.len(), 1);
        assert!(
            matches!(entries[0].state, State::Skipped { .. }),
            "{entries:?}"
        );

        let restored = snapshots.restore_before(before);
        assert_eq!(restored.failed.len(), 1, "the restore must report the gap");
    }

    #[test]
    fn the_manifest_survives_being_reopened() {
        // Resume and `/rewind` both read a manifest a previous process wrote.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("x.rs");
        fs::write(&file, "before").unwrap();

        let before = SystemTime::now();
        store(dir.path()).capture(&file);
        fs::write(&file, "after").unwrap();

        let reopened = store(dir.path());
        assert_eq!(reopened.entries().len(), 1);
        reopened.restore_before(before);
        assert_eq!(fs::read_to_string(&file).unwrap(), "before");
    }
}
