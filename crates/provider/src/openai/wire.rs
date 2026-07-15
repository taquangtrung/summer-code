//! The OpenAI chat-completions wire shapes, and the conversion at the edge.
//!
//! Private to this module for the reason `anthropic/wire.rs` is: the journal's
//! schema and the engine's [`TurnRequest`] are the contract, and an API change
//! must stop here rather than become a schema migration.
//!
//! **The shapes differ from Anthropic's in three ways that matter.**
//!
//! 1. The system prompt is a message with `role: "system"`, not a top-level
//!    field.
//! 2. A tool result is a message with `role: "tool"` carrying the id it answers,
//!    rather than a content block inside a user message. One assistant turn
//!    asking for three tools therefore becomes three separate messages back.
//! 3. Tool call arguments stream as JSON string fragments under an *index*, and
//!    the id and name arrive only on the first fragment. Nothing can be parsed
//!    until the block closes, which is the same rule the engine already applies.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use journal::{StopReason, Usage};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::Effort;
use crate::provider::{Role, TurnBlock, TurnMessage, TurnRequest};

// ========================================================================
// Data Structures
// ========================================================================

/// The request body.
#[derive(Serialize)]
pub struct ChatRequest {
    pub max_completion_tokens: u32,
    pub messages: Vec<ChatMessage>,
    pub model: String,
    /// Routes requests that share a prefix to the same cache, on OpenAI's own
    /// API. `None` everywhere else: a server on this transport that does not
    /// know the field may refuse the request over it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,
    /// The reasoning effort, when one is set. `/effort` used to be accepted
    /// and then never sent on this transport.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<&'static str>,
    /// Asks for usage numbers on the final chunk. Without it a streamed
    /// response carries no token counts at all, and every turn journals zero
    /// spend.
    pub stream_options: StreamOptions,
    pub stream: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ChatTool>,
}

#[derive(Serialize)]
pub struct StreamOptions {
    pub include_usage: bool,
}

/// One message. The role decides which of the optional fields are meaningful,
/// which is the API's shape rather than a choice made here.
#[derive(Serialize)]
pub struct ChatMessage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    pub role: &'static str,
    /// Set only on a `tool` message: which call this answers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCallRequest>,
}

#[derive(Serialize)]
pub struct ToolCallRequest {
    pub function: FunctionCall,
    pub id: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
}

#[derive(Serialize)]
pub struct FunctionCall {
    /// The arguments, as a JSON *string*. The API takes a string here, not an
    /// object, on the way in as well as out.
    pub arguments: String,
    pub name: String,
}

#[derive(Serialize)]
pub struct ChatTool {
    pub function: FunctionSpec,
    #[serde(rename = "type")]
    pub kind: &'static str,
}

#[derive(Serialize)]
pub struct FunctionSpec {
    pub description: String,
    pub name: String,
    pub parameters: Value,
}

/// One `data:` frame of the response stream.
#[derive(Deserialize)]
pub struct ChatChunk {
    #[serde(default)]
    pub choices: Vec<ChatChoice>,
    #[serde(default)]
    pub usage: Option<ChatUsage>,
}

#[derive(Deserialize)]
pub struct ChatChoice {
    #[serde(default)]
    pub delta: ChatDelta,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Default, Deserialize)]
pub struct ChatDelta {
    #[serde(default)]
    pub content: Option<String>,
    /// Present on reasoning models that stream their thinking. Not standard
    /// across every OpenAI-compatible endpoint, so its absence is normal.
    #[serde(default)]
    pub reasoning_content: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCallDelta>,
}

#[derive(Deserialize)]
pub struct ToolCallDelta {
    #[serde(default)]
    pub function: Option<FunctionDelta>,
    #[serde(default)]
    pub id: Option<String>,
    /// Which call this fragment belongs to. The only thing tying fragments
    /// together: the id arrives once and every later fragment carries just this.
    pub index: usize,
}

#[derive(Deserialize)]
pub struct FunctionDelta {
    #[serde(default)]
    pub arguments: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Deserialize)]
pub struct ChatUsage {
    #[serde(default)]
    pub completion_tokens: u32,
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub prompt_tokens_details: Option<PromptDetails>,
}

#[derive(Deserialize)]
pub struct PromptDetails {
    /// Prompt tokens served from the provider's cache. Counted separately from
    /// `prompt_tokens` for the same reason Anthropic's are: a cached token still
    /// occupies the window but is not new spend.
    #[serde(default)]
    pub cached_tokens: u32,
}

// ========================================================================
// Functions
// ========================================================================

/// Build a request body from the engine's provider-neutral turn.
pub fn build_request(req: &TurnRequest, model: &str, max_tokens: u32) -> ChatRequest {
    let mut messages = Vec::with_capacity(req.messages.len() + 1);
    if !req.system.is_empty() {
        messages.push(ChatMessage {
            content: Some(req.system.clone()),
            role: "system",
            tool_call_id: None,
            tool_calls: Vec::new(),
        });
    }
    for message in &req.messages {
        push_message(&mut messages, message);
    }
    ChatRequest {
        max_completion_tokens: max_tokens,
        messages,
        model: model.to_string(),
        prompt_cache_key: None,
        reasoning_effort: None,
        stream: true,
        stream_options: StreamOptions {
            include_usage: true,
        },
        tools: req
            .tools
            .iter()
            .map(|tool| ChatTool {
                function: FunctionSpec {
                    description: tool.description.clone(),
                    name: tool.name.clone(),
                    parameters: tool.input_schema.clone(),
                },
                kind: "function",
            })
            .collect(),
    }
}

/// The `reasoning_effort` value for `effort`. The chat-completions scale tops
/// out at `high` on the servers this transport speaks to, so the two levels
/// above it ask for the most there is rather than a value that would be
/// refused.
pub fn reasoning_effort(effort: Effort) -> &'static str {
    match effort {
        Effort::Low => "low",
        Effort::Medium => "medium",
        Effort::High | Effort::Max | Effort::Xhigh => "high",
    }
}

/// A cache-routing key for `req`: a hash of its system prompt and tool names,
/// which hold still for a whole session, so every turn of it routes alike.
pub fn prompt_cache_key(req: &TurnRequest) -> String {
    let mut hasher = DefaultHasher::new();
    req.system.hash(&mut hasher);
    for tool in &req.tools {
        tool.name.hash(&mut hasher);
    }
    format!("summer-{:016x}", hasher.finish())
}

/// Flatten one neutral message into the one or more chat messages it becomes.
///
/// **A user message carrying tool results becomes several messages, and the
/// results must come first.** The API requires every `tool` message to directly
/// follow the assistant message that asked for it, so a user turn holding three
/// tool results plus a text note emits three `tool` messages and then, if there
/// was any text, one `user` message.
fn push_message(out: &mut Vec<ChatMessage>, message: &TurnMessage) {
    let role = match message.role {
        Role::Assistant => "assistant",
        Role::User => "user",
    };

    let mut text = String::new();
    let mut tool_calls = Vec::new();
    for block in &message.content {
        match block {
            TurnBlock::Text(chunk) => text.push_str(chunk),
            // Thinking is not echoed back: the API has no field for an opaque
            // signed reasoning block on the way in, and inventing one would be
            // a 400. Anthropic's requirement to replay them byte-identically
            // simply does not apply here.
            TurnBlock::Thinking(_) => {}
            TurnBlock::ToolResult(result) => out.push(ChatMessage {
                content: Some(result.content.clone()),
                role: "tool",
                tool_call_id: Some(result.call.to_string()),
                tool_calls: Vec::new(),
            }),
            TurnBlock::ToolUse(use_) => tool_calls.push(ToolCallRequest {
                function: FunctionCall {
                    arguments: use_.args.to_string(),
                    name: use_.name.clone(),
                },
                id: use_.call.to_string(),
                kind: "function",
            }),
        }
    }

    if !text.is_empty() || !tool_calls.is_empty() {
        out.push(ChatMessage {
            content: (!text.is_empty()).then_some(text),
            role,
            tool_call_id: None,
            tool_calls,
        });
    }
}

/// The neutral stop reason a `finish_reason` means.
pub fn stop_reason(finish: &str) -> StopReason {
    match finish {
        "length" => StopReason::MaxTokens,
        "tool_calls" | "function_call" => StopReason::ToolUse,
        // The category the API gave, so the loop's `Stop::Refused` names why
        // rather than reporting a bare refusal.
        "content_filter" => StopReason::Refusal {
            category: Some("content_filter".to_string()),
        },
        // "stop" and anything an OpenAI-compatible endpoint invents: the turn
        // ended. `&str` is an open universe, so a wildcard is right here.
        _ => StopReason::EndTurn,
    }
}

/// Usage as the journal records it.
pub fn usage(usage: &ChatUsage) -> Usage {
    let cached = usage
        .prompt_tokens_details
        .as_ref()
        .map_or(0, |details| details.cached_tokens);
    Usage::new(
        // `prompt_tokens` includes the cached ones, and the journal counts them
        // separately. Subtracting keeps the total honest instead of billing the
        // cache twice.
        usage.prompt_tokens.saturating_sub(cached),
        usage.completion_tokens,
        cached,
        // The API reports no cache-write count; claiming one would be inventing
        // a number.
        0,
    )
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use journal::CallId;
    use serde_json::json;

    use crate::provider::{TurnToolResult, TurnToolUse};

    use super::*;

    fn user(blocks: Vec<TurnBlock>) -> TurnMessage {
        TurnMessage {
            content: blocks,
            role: Role::User,
        }
    }

    #[test]
    fn effort_maps_onto_the_scale_the_api_accepts() {
        // `xhigh` and `max` are Anthropic's; sent as-is they would be a 400.
        assert_eq!(reasoning_effort(Effort::Low), "low");
        assert_eq!(reasoning_effort(Effort::Xhigh), "high");
        assert_eq!(reasoning_effort(Effort::Max), "high");
    }

    #[test]
    fn the_cache_key_holds_across_turns_and_is_sent_only_when_set() {
        let mut req = TurnRequest {
            messages: Vec::new(),
            system: "You are summer-code.".into(),
            tools: Vec::new(),
        };
        let key = prompt_cache_key(&req);
        req.messages.push(TurnMessage {
            content: vec![TurnBlock::Text("hi".into())],
            role: Role::User,
        });
        assert_eq!(prompt_cache_key(&req), key, "a new message keeps the key");

        let body = serde_json::to_value(build_request(&req, "gpt-5", 100)).unwrap();
        assert!(body.get("prompt_cache_key").is_none() && body.get("reasoning_effort").is_none());
    }

    #[test]
    fn the_system_prompt_becomes_a_message_not_a_field() {
        let req = TurnRequest {
            messages: vec![user(vec![TurnBlock::Text("hi".into())])],
            system: "be helpful".into(),
            tools: Vec::new(),
        };
        let body = build_request(&req, "gpt-5", 100);
        assert_eq!(body.messages[0].role, "system");
        assert_eq!(body.messages[0].content.as_deref(), Some("be helpful"));
        assert_eq!(body.messages[1].role, "user");
    }

    #[test]
    fn each_tool_result_becomes_its_own_tool_message_carrying_the_call_id() {
        // The API rejects a user message with tool results inside it, and it
        // matches results to calls by id. Getting either wrong is a 400 on every
        // turn that used a tool, which is nearly all of them.
        let req = TurnRequest {
            messages: vec![user(vec![
                TurnBlock::ToolResult(TurnToolResult {
                    call: CallId::new("call_a".into()),
                    content: "first".into(),
                    is_error: false,
                }),
                TurnBlock::ToolResult(TurnToolResult {
                    call: CallId::new("call_b".into()),
                    content: "second".into(),
                    is_error: false,
                }),
            ])],
            system: String::new(),
            tools: Vec::new(),
        };
        let body = build_request(&req, "gpt-5", 100);
        assert_eq!(body.messages.len(), 2, "one message per result");
        assert_eq!(body.messages[0].role, "tool");
        assert_eq!(body.messages[0].tool_call_id.as_deref(), Some("call_a"));
        assert_eq!(body.messages[1].tool_call_id.as_deref(), Some("call_b"));
    }

    #[test]
    fn a_user_turn_with_results_and_text_puts_the_results_first() {
        // The `tool` messages have to follow the assistant message that asked;
        // emitting the user's text between them breaks that adjacency.
        let req = TurnRequest {
            messages: vec![user(vec![
                TurnBlock::ToolResult(TurnToolResult {
                    call: CallId::new("call_a".into()),
                    content: "output".into(),
                    is_error: false,
                }),
                TurnBlock::Text("now do the next thing".into()),
            ])],
            system: String::new(),
            tools: Vec::new(),
        };
        let body = build_request(&req, "gpt-5", 100);
        assert_eq!(body.messages[0].role, "tool");
        assert_eq!(body.messages[1].role, "user");
    }

    #[test]
    fn an_assistant_tool_call_serializes_its_arguments_as_a_string() {
        // The one field where the API takes a JSON string rather than an object.
        // Sending an object is a 400 the error message does not explain well.
        let req = TurnRequest {
            messages: vec![TurnMessage {
                content: vec![TurnBlock::ToolUse(TurnToolUse {
                    args: json!({"path": "src/lib.rs"}),
                    call: CallId::new("call_a".into()),
                    name: "read".into(),
                })],
                role: Role::Assistant,
            }],
            system: String::new(),
            tools: Vec::new(),
        };
        let body = build_request(&req, "gpt-5", 100);
        let rendered = serde_json::to_value(&body.messages[0]).unwrap();
        assert_eq!(
            rendered["tool_calls"][0]["function"]["arguments"],
            json!(r#"{"path":"src/lib.rs"}"#)
        );
    }

    #[test]
    fn an_empty_message_is_dropped_rather_than_sent_blank() {
        // A user turn that was nothing but tool results must not also emit an
        // empty `user` message; several endpoints reject one.
        let req = TurnRequest {
            messages: vec![user(vec![TurnBlock::ToolResult(TurnToolResult {
                call: CallId::new("call_a".into()),
                content: "output".into(),
                is_error: false,
            })])],
            system: String::new(),
            tools: Vec::new(),
        };
        let body = build_request(&req, "gpt-5", 100);
        assert_eq!(body.messages.len(), 1);
    }

    #[test]
    fn cached_prompt_tokens_are_not_counted_twice() {
        // `prompt_tokens` is inclusive of the cached ones. Recording both whole
        // makes a well-cached session report roughly double its real input.
        let counted = usage(&ChatUsage {
            completion_tokens: 10,
            prompt_tokens: 1_000,
            prompt_tokens_details: Some(PromptDetails { cached_tokens: 900 }),
        });
        assert_eq!(counted.input, 100);
        assert_eq!(counted.cache_read, 900);
        assert_eq!(counted.output, 10);
    }

    #[test]
    fn finish_reasons_map_to_the_journals_vocabulary() {
        assert_eq!(stop_reason("tool_calls"), StopReason::ToolUse);
        assert_eq!(stop_reason("length"), StopReason::MaxTokens);
        assert!(matches!(
            stop_reason("content_filter"),
            StopReason::Refusal { .. }
        ));
        assert_eq!(stop_reason("stop"), StopReason::EndTurn);
    }
}
