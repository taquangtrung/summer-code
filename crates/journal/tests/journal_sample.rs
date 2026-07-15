//! `docs/journal-sample.jsonl` is the authoritative example of the wire format,
//! and these tests are what stop it drifting from the schema.
//!
//! The JSONL file is the contract: external consumers (telemetry, the monitor,
//! Agentum/Mongoose) parse it without linking anything of ours. So the bytes are
//! specified rather than left to serde's defaults, and a test has to actually
//! look at them. A round-trip through Rust structs proves nothing here: it
//! passes under any tagging, any timestamp encoding, and any field order.
//!
//! The sample is pulled in with `include_str!` rather than read at runtime,
//! which keeps this crate's "never opens a file" rule intact even in its tests.

use std::io::Cursor;
use std::net::{IpAddr, Ipv4Addr};
use std::str::FromStr;
use std::time::{Duration, SystemTime};

use serde_json::{Value, json};
use summer_journal::{
    Access, AccessConnect, AccessExec, AccessListen, Block, BlockThinking, BlockToolUse, CallId,
    Compacted, Decision, Delegated, Folded, ModelTurn, Outcome, Permission, ProviderFailed, Record,
    RecordId, RecordKind, SessionId, SessionStarted, StopReason, ToolCall, ToolResult, Usage,
    read_records, write_records,
};

// ========================================================================
// Constants
// ========================================================================

const SAMPLE: &str = include_str!("../../../docs/journal-sample.jsonl");

/// 2026-07-15T12:00:00.000Z, matching the plan's worked example.
const BASE_EPOCH_SECS: u64 = 1_784_116_800;

// ========================================================================
// Functions
// ========================================================================

fn record_id(suffix: &str) -> RecordId {
    RecordId::from_str(&format!("01JQZ8{}{suffix}", "0".repeat(18))).expect("valid ULID")
}

fn session_id() -> SessionId {
    SessionId::from_str("01JQZ8000000000000000000SS").expect("valid ULID")
}

fn at(offset_secs: u64, millis: u32) -> SystemTime {
    SystemTime::UNIX_EPOCH
        + Duration::from_secs(BASE_EPOCH_SECS + offset_secs)
        + Duration::from_millis(u64::from(millis))
}

fn record(
    offset_secs: u64,
    millis: u32,
    id: &str,
    parent: Option<&str>,
    kind: RecordKind,
) -> Record {
    Record {
        at: at(offset_secs, millis),
        id: record_id(id),
        kind,
        parent: parent.map(record_id),
        session: session_id(),
    }
}

/// The tag serde writes for a kind. The match is exhaustive on purpose: adding
/// an `RecordKind` variant breaks this function, which is what forces the new
/// variant into the sample rather than letting it ship undocumented.
fn tag_of(kind: &RecordKind) -> &'static str {
    match kind {
        RecordKind::Compacted(_) => "compacted",
        RecordKind::Delegated(_) => "delegated",
        RecordKind::Folded(_) => "folded",
        RecordKind::ModelTurn(_) => "model_turn",
        RecordKind::Permission(_) => "permission",
        RecordKind::ProviderFailed(_) => "provider_failed",
        RecordKind::SessionStarted(_) => "session_started",
        RecordKind::SessionTitled { .. } => "session_titled",
        RecordKind::ToolCall(_) => "tool_call",
        RecordKind::ToolResult(_) => "tool_result",
        RecordKind::UserMessage { .. } => "user_message",
        // Not a kind anything writes: it is what *reading* an unknown `kind`
        // produces, so it has no tag of its own and cannot appear in the sample.
        RecordKind::Unknown => "unknown",
    }
}

/// One record per `RecordKind` variant, in the order the sample file lists them.
fn sample_records() -> Vec<Record> {
    let call = CallId::new("toolu_016x".into());
    vec![
        record(
            0,
            0,
            "A1",
            None,
            RecordKind::SessionStarted(SessionStarted::new(
                0,
                "/home/x/repo".into(),
                "claude-opus-4-8".into(),
                "sha256:9f2b".into(),
                "sha256:a1b2".into(),
                "sha256:c3d4".into(),
            )),
        ),
        record(
            1,
            0,
            "A2",
            Some("A1"),
            RecordKind::UserMessage {
                text: "add a test for foo".into(),
            },
        ),
        record(
            2,
            500,
            "A3",
            Some("A2"),
            RecordKind::ModelTurn(ModelTurn::new(
                vec![
                    Block::Text {
                        text: "I'll read the file first.".into(),
                    },
                    Block::Thinking(BlockThinking::new(
                        "EqoBCkgIAxABGAIiQ".into(),
                        String::new(),
                    )),
                    Block::ToolUse(BlockToolUse::new(
                        call.clone(),
                        "read".into(),
                        json!({"path": "src/foo.rs"}),
                    )),
                ],
                StopReason::ToolUse,
                Usage::new(1_200, 95, 8_400, 0),
            )),
        ),
        record(
            3,
            0,
            "A4",
            Some("A3"),
            RecordKind::ToolCall(ToolCall::new(
                call.clone(),
                "read".into(),
                json!({"path": "src/foo.rs"}),
            )),
        ),
        record(
            4,
            120,
            "A5",
            Some("A4"),
            RecordKind::Permission(Permission::new(
                call.clone(),
                Access::Write {
                    path: "/home/x/repo/src/foo.rs".into(),
                },
                Decision::AskedAllowed,
                summer_journal::Scope::Rule {
                    rule: "write:src/**".into(),
                },
            )),
        ),
        record(
            5,
            0,
            "A6",
            Some("A5"),
            RecordKind::ToolResult(ToolResult::new(
                call,
                Outcome::Ok {
                    content: "pub fn foo() {}".into(),
                },
            )),
        ),
        record(
            6,
            0,
            "A7",
            Some("A6"),
            RecordKind::ProviderFailed(ProviderFailed::new("rate_limited".into(), 1)),
        ),
        record(
            7,
            0,
            "A8",
            Some("A7"),
            RecordKind::Compacted(Compacted::new(
                vec![record_id("A2"), record_id("A3")],
                Usage::new(1200, 340, 0, 0),
                "The user asked for a test for foo; the agent read src/foo.rs.".into(),
            )),
        ),
        record(
            8,
            0,
            "A9",
            Some("A8"),
            RecordKind::Delegated(Delegated::new(
                CallId::new("toolu_017y".into()),
                "/home/x/repo/.summer/sessions/01JQZ8000000000000000000TT-subagent.jsonl".into(),
                Usage::new(900, 210, 0, 0),
            )),
        ),
        record(
            9,
            0,
            "AA",
            Some("A9"),
            RecordKind::Folded(Folded::new(
                42,
                record_id("B7"),
                SessionId::from_str("01JQZ8000000000000000000TT").expect("valid ULID"),
            )),
        ),
        record(
            10,
            0,
            "AB",
            Some("AA"),
            RecordKind::SessionTitled {
                title: "Add a regression test for foo".into(),
            },
        ),
    ]
}

fn render(records: &[Record]) -> String {
    let mut buffer = Vec::new();
    write_records(&mut buffer, records).expect("write");
    String::from_utf8(buffer).expect("utf-8")
}

/// The `access` object of the sample's `permission` line.
fn sample_access() -> Value {
    let line = SAMPLE
        .lines()
        .find(|line| line.contains(r#""kind":"permission""#))
        .expect("sample has a permission line");
    let parsed: Value = serde_json::from_str(line).expect("valid JSON");
    parsed["access"].clone()
}

// ========================================================================
// Tests
// ========================================================================

#[test]
fn sample_is_exactly_what_the_codec_writes() {
    // Byte-for-byte, so a tagging or timestamp change fails here rather than
    // silently rewriting the contract.
    assert_eq!(render(&sample_records()), SAMPLE);
}

#[test]
fn sample_round_trips_through_the_codec_unchanged() {
    let parsed = read_records(Cursor::new(SAMPLE)).expect("read");
    assert_eq!(parsed, sample_records());
    assert_eq!(render(&parsed), SAMPLE);
}

#[test]
fn sample_carries_one_line_per_record_kind() {
    let records = sample_records();
    let mut tags: Vec<&str> = records.iter().map(|record| tag_of(&record.kind)).collect();
    tags.sort_unstable();
    tags.dedup();
    assert_eq!(
        tags.len(),
        records.len(),
        "sample must carry each RecordKind exactly once"
    );

    for tag in tags {
        let needle = format!(r#""kind":"{tag}""#);
        assert!(
            SAMPLE.contains(&needle),
            "sample is missing a line for kind `{tag}`"
        );
    }
}

#[test]
fn every_line_is_newline_terminated_and_compact() {
    assert!(SAMPLE.ends_with('\n'), "last line must be \\n-terminated");
    assert!(!SAMPLE.contains('\r'), "no \\r\\n");
    for line in SAMPLE.lines() {
        assert!(!line.is_empty(), "no blank lines");
        assert!(
            !line.contains("\n  "),
            "no pretty-printing: one record per line"
        );
    }
}

#[test]
fn nested_enums_are_internally_tagged_not_externally() {
    // The failure this catches is the whole reason the sample exists. An
    // externally tagged `Access::Write(PathBuf)` renders
    // `{"Write":"/home/x/repo/src/foo.rs"}`, which routes on a key that moves
    // with the variant. The contract is a fixed `kind` key.
    let access = sample_access();
    assert_eq!(access["kind"], json!("write"));
    assert_eq!(access["path"], json!("/home/x/repo/src/foo.rs"));
    assert!(
        access.get("Write").is_none(),
        "external tagging came back: {access}"
    );
}

#[test]
fn newtype_variants_wrapping_a_string_cannot_sneak_back_in() {
    // `Access::Read(PathBuf)`, `Block::Text(String)` and `Scope::Rule(String)`
    // fail at RUNTIME under internal tagging, not at compile time, with
    // `cannot serialize tagged newtype variant containing a string`. Serializing
    // each is the only way to assert they were not reintroduced.
    let access = Access::Read {
        path: "/home/x/repo/src/foo.rs".into(),
    };
    assert_eq!(
        serde_json::to_value(&access).expect("Access::Read must serialize"),
        json!({"kind": "read", "path": "/home/x/repo/src/foo.rs"})
    );

    let block = Block::Text {
        text: "hello".into(),
    };
    assert_eq!(
        serde_json::to_value(&block).expect("Block::Text must serialize"),
        json!({"kind": "text", "text": "hello"})
    );

    let scope = summer_journal::Scope::Rule {
        rule: "write:src/**".into(),
    };
    assert_eq!(
        serde_json::to_value(&scope).expect("Scope::Rule must serialize"),
        json!({"kind": "rule", "rule": "write:src/**"})
    );
}

#[test]
fn thinking_blocks_round_trip_their_signature_byte_identical() {
    // `signature` is opaque and the API rejects a modified block, so a journal
    // that mangled it would rebuild a request the API refuses on the next turn.
    let signature = "EqoBCkgIAxABGAIiQ/+r4Zz9==";
    let block = Block::Thinking(BlockThinking::new(signature.into(), String::new()));
    let encoded = serde_json::to_string(&block).expect("serialize");
    let decoded: Block = serde_json::from_str(&encoded).expect("deserialize");
    assert_eq!(decoded, block);

    let Block::Thinking(thinking) = decoded else {
        panic!("expected a thinking block");
    };
    assert_eq!(thinking.signature, signature);
}

#[test]
fn listen_records_the_address_the_broker_bound() {
    // `Access::Listen` carries the bound address, not just the port, so the
    // audit trail can tell loopback from a wildcard bind.
    let access = Access::Listen(AccessListen::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 5432));
    assert_eq!(
        serde_json::to_value(&access).expect("serialize"),
        json!({"kind": "listen", "addr": "127.0.0.1", "port": 5432})
    );
}

#[test]
fn connect_records_the_addresses_the_host_resolved_to() {
    // The policy allows hosts by name, so the name alone cannot say where the
    // bytes went: an auditor reading `connect: docs.example.com` needs to see
    // that it answered `127.0.0.1` before the record means anything.
    let connect = Access::Connect(AccessConnect::new(
        vec![IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34))],
        "example.com".into(),
        443,
    ));
    assert_eq!(
        serde_json::to_value(&connect).expect("serialize"),
        json!({"kind": "connect", "addrs": ["93.184.216.34"], "host": "example.com", "port": 443})
    );
}

#[test]
fn a_journal_written_before_addrs_still_reads() {
    // `#[serde(default)]` is the whole forward-compatibility story for this
    // field: a v0 record carries no `addrs` key, and refusing to parse it would
    // break `/resume` on every session recorded before today.
    let record: Access =
        serde_json::from_value(json!({"kind": "connect", "host": "example.com", "port": 443}))
            .expect("an older connect record still parses");
    assert_eq!(
        record,
        Access::Connect(AccessConnect::new(Vec::new(), "example.com".into(), 443))
    );
}

#[test]
fn a_folded_record_written_as_events_still_reads() {
    // The only wire-visible casualty of naming the unit a `Record`: this count
    // was `events`. A journal that folded a subagent before the rename has to
    // stay resumable, so the old spelling is an accepted alias.
    let old = json!({
        "kind": "folded",
        "events": 42,
        "source_head": "01JQZ8000000000000000000B7",
        "source_session": "01JQZ8000000000000000000TT",
    });
    let folded: Folded = serde_json::from_value(old).expect("the pre-rename spelling still parses");
    assert_eq!(folded.records, 42);
}

#[test]
fn remaining_access_variants_carry_the_kind_key() {
    let exec = Access::Exec(AccessExec::new("cargo".into(), vec!["test".into()]));
    assert_eq!(
        serde_json::to_value(&exec).expect("serialize"),
        json!({"kind": "exec", "program": "cargo", "args": ["test"]})
    );
}
