//! Record builders shared by the property tests.
//!
//! Every property needs the same two or three records with one field varied, and
//! hand-rolling an `Record` per test buries the varied field in boilerplate.

use std::time::SystemTime;

use journal::{
    Access, CallId, Decision, Outcome, Permission, Record, RecordId, RecordKind, Scope, SessionId,
    ToolCall, ToolResult,
};

// ========================================================================
// Functions
// ========================================================================

pub fn call_id(id: &str) -> CallId {
    CallId::new(id.to_string())
}

pub fn permission_record(call: CallId, access: Access, decision: Decision, scope: Scope) -> Record {
    record(RecordKind::Permission(Permission::new(
        call, access, decision, scope,
    )))
}

pub fn tool_call_record(call: CallId, name: &str) -> Record {
    record(RecordKind::ToolCall(ToolCall::new(
        call,
        name.to_string(),
        serde_json::Value::Null,
    )))
}

pub fn tool_result_record(call: CallId, outcome: Outcome) -> Record {
    record(RecordKind::ToolResult(ToolResult::new(call, outcome)))
}

fn record(kind: RecordKind) -> Record {
    Record {
        at: SystemTime::UNIX_EPOCH,
        id: RecordId::new(),
        kind,
        parent: None,
        session: SessionId::new(),
    }
}
