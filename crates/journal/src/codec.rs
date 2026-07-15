//! JSONL codec over caller-supplied handles.
//!
//! The journal never opens a file. Writing takes `impl Write`, reading takes
//! `impl BufRead` (line splitting needs buffering), and a real file or socket
//! appears only behind the [`crate::JournalSink`] seam.

use std::io::{BufRead, Write};

use crate::error::JournalError;
use crate::record::Record;

// ========================================================================
// Functions
// ========================================================================

/// Write one record as a single JSON line.
///
/// **Serialized into a buffer, then one `write_all`.** Writing serde's output
/// directly into the handle issues a syscall per token -- dozens per record -- and
/// leaves the line's framing at the mercy of whatever the handle does between
/// them. One call per record means an unbuffered file gets one `write`, which for
/// a line under `PIPE_BUF` is what makes "a torn line is only ever the last one"
/// true rather than hopeful.
pub fn write_record<W>(writer: &mut W, record: &Record) -> Result<(), JournalError>
where
    W: Write,
{
    let mut line = serde_json::to_vec(record)?;
    line.push(b'\n');
    writer.write_all(&line)?;
    Ok(())
}

/// Write a sequence of records, one per line.
pub fn write_records<W>(writer: &mut W, records: &[Record]) -> Result<(), JournalError>
where
    W: Write,
{
    for record in records {
        write_record(writer, record)?;
    }
    Ok(())
}

/// Read every record from a buffered source, one JSON line at a time. Blank
/// lines are skipped; an unparseable line is a decode error naming its line
/// number — **unless it is an unterminated final line**, which is dropped.
///
/// The distinction is the difference between corruption and a crash. A
/// malformed line in the middle of the file has a well-formed line after it, so
/// something wrote past it: the file is not what it claims to be and folding it
/// forward would guess. A *final* line with no `\n` is the ordinary shape of a
/// process killed between the `write` and its completion, and treating that as
/// corruption made the whole journal unreadable in exactly the case resume
/// exists for. One lost record is the honest cost; a lost session is not.
pub fn read_records<R>(mut reader: R) -> Result<Vec<Record>, JournalError>
where
    R: BufRead,
{
    let mut records = Vec::new();
    let mut line = String::new();
    let mut index = 0usize;
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        index += 1;
        let terminated = line.ends_with('\n');
        let text = line.trim();
        if text.is_empty() {
            continue;
        }
        match serde_json::from_str(text) {
            Ok(record) => records.push(record),
            Err(source) if terminated => {
                return Err(JournalError::Decode {
                    line: index,
                    source,
                });
            }
            // A torn tail. `read_line` returned 0 next time round anyway, so
            // stopping here loses nothing beyond the partial line itself.
            Err(_) => break,
        }
    }
    Ok(records)
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::{RecordId, SessionId};
    use crate::record::{Record, RecordKind, SessionStarted};

    fn sample_record() -> Record {
        Record {
            at: std::time::SystemTime::UNIX_EPOCH,
            id: RecordId::new(),
            kind: RecordKind::SessionStarted(SessionStarted::new(
                0,
                "/tmp/repo".into(),
                "claude-opus-4-8".into(),
                "policy-digest".into(),
                "prompt-digest".into(),
                "tools-digest".into(),
            )),
            parent: None,
            session: SessionId::new(),
        }
    }

    #[test]
    fn round_trip_preserves_record() {
        let record = sample_record();
        let mut buffer = Vec::new();
        write_record(&mut buffer, &record).unwrap();
        let mut cursor = std::io::Cursor::new(buffer);
        let read = read_records(&mut cursor).unwrap();
        assert_eq!(read, vec![record]);
    }

    #[test]
    fn read_skips_blank_lines() {
        let record = sample_record();
        let mut buffer = Vec::new();
        write_record(&mut buffer, &record).unwrap();
        // Inject blank lines between records.
        let mut buffer_with_gaps = Vec::new();
        buffer_with_gaps.extend_from_slice(b"\n");
        buffer_with_gaps.extend_from_slice(&buffer);
        buffer_with_gaps.extend_from_slice(b"\n\n");
        let mut cursor = std::io::Cursor::new(buffer_with_gaps);
        let read = read_records(&mut cursor).unwrap();
        assert_eq!(read, vec![record]);
    }

    #[test]
    fn decode_error_names_line_number() {
        let bad = "not-json\n";
        let mut cursor = std::io::Cursor::new(bad);
        let err = read_records(&mut cursor).unwrap_err();
        match err {
            JournalError::Decode { line, .. } => assert_eq!(line, 1),
            other => panic!("expected Decode, got {other:?}"),
        }
    }

    #[test]
    fn a_torn_final_line_loses_one_record_not_the_journal() {
        // The crash shape: killed between the write and its completion, so the
        // last line is half an record with no newline. Failing the whole decode
        // here made resume impossible in precisely the case it exists for.
        let record = sample_record();
        let mut buffer = Vec::new();
        write_record(&mut buffer, &record).unwrap();
        buffer.extend_from_slice(br#"{"at":"2026-07-15T12:00:0"#);

        let read = read_records(std::io::Cursor::new(buffer)).expect("the torn tail is dropped");
        assert_eq!(read, vec![record], "everything before the tear survives");
    }

    #[test]
    fn a_malformed_line_in_the_middle_is_still_fatal() {
        // Not a crash: something wrote *past* this line, so the file is not what
        // it claims to be and folding it forward would be a guess.
        let record = sample_record();
        let mut buffer = Vec::new();
        write_record(&mut buffer, &record).unwrap();
        buffer.extend_from_slice(b"not-json\n");
        write_record(&mut buffer, &record).unwrap();

        let error = read_records(std::io::Cursor::new(buffer))
            .expect_err("a malformed line with data after it is corruption");
        assert!(matches!(error, JournalError::Decode { line: 2, .. }));
    }

    #[test]
    fn a_complete_final_line_without_a_newline_still_reads() {
        // The tolerance must not become a rule that the last record is always
        // dropped: a file whose final line is whole but unterminated is intact.
        let record = sample_record();
        let mut buffer = Vec::new();
        write_record(&mut buffer, &record).unwrap();
        buffer.pop();

        let read = read_records(std::io::Cursor::new(buffer)).expect("reads");
        assert_eq!(read, vec![record]);
    }
}
