//! Request body construction. Anthropic wire shapes live here and nowhere else,
//! so an API change is not a journal schema migration.

use serde_json::{Value, json};

use crate::config::{Display, ProviderConfig, Thinking};
use crate::provider::{Role, TurnBlock, TurnRequest};

// ========================================================================
// Functions
// ========================================================================

/// Build the Messages request body.
///
/// **Render order is `tools`, then `system`, then `messages`**, and the cache
/// breakpoints follow from it. A breakpoint on the last system block caches the
/// tool definitions too, which is what we want: the tool list is large, stable,
/// and identical on every turn of a session. A second breakpoint on the last
/// content block of the last message makes each turn read the whole prior
/// conversation from cache.
///
/// **Two breakpoints, not four.** Four is the per-request limit, not a target.
///
/// The cache minimum on this model is 4096 tokens. A shorter prefix silently
/// does not cache: no error, `cache_creation_input_tokens: 0`. The system prompt
/// plus the built-in tool schemas clears it comfortably.
pub fn body(config: &ProviderConfig, req: TurnRequest, stream: bool) -> Value {
    let mut body = json!({
        "model": config.model,
        "max_tokens": config.max_tokens,
    });
    // Effort is opt-in: some models 400 on `output_config.effort`, so it is
    // sent only when a provider set one.
    if let Some(effort) = config.effort {
        body["output_config"] = json!({ "effort": effort.as_str() });
    }

    if stream {
        body["stream"] = json!(true);
    }

    // Thinking is set explicitly, always. Omitting the field runs *without*
    // thinking on this model, and `{type: "enabled", budget_tokens: N}` is a 400.
    body["thinking"] = thinking(config.thinking);

    if !req.tools.is_empty() {
        body["tools"] = tools(&req);
    }
    if !req.system.is_empty() {
        body["system"] = system(&req.system);
    }
    body["messages"] = messages(req);
    body
}

/// The tool list. Cached along with the system prompt by the breakpoint below it.
fn tools(req: &TurnRequest) -> Value {
    let rendered: Vec<Value> = req
        .tools
        .iter()
        .map(|tool| {
            json!({
                "name": tool.name,
                "description": tool.description,
                "input_schema": tool.input_schema,
            })
        })
        .collect();
    json!(rendered)
}

/// The system prompt as a single text block, carrying the first breakpoint.
fn system(prompt: &str) -> Value {
    json!([{
        "type": "text",
        "text": prompt,
        "cache_control": { "type": "ephemeral" },
    }])
}

/// The window. The last content block of the last message carries the second
/// breakpoint.
fn messages(req: TurnRequest) -> Value {
    let mut rendered: Vec<Value> = req
        .messages
        .into_iter()
        .map(|message| {
            let role = match message.role {
                Role::Assistant => "assistant",
                Role::User => "user",
            };
            let content: Vec<Value> = message.content.into_iter().map(block).collect();
            json!({ "role": role, "content": content })
        })
        .collect();

    if let Some(last) = rendered.last_mut()
        && let Some(blocks) = last["content"].as_array_mut()
        && let Some(block) = blocks.last_mut()
    {
        block["cache_control"] = json!({ "type": "ephemeral" });
    }
    json!(rendered)
}

fn thinking(thinking: Thinking) -> Value {
    match thinking {
        Thinking::Adaptive { display } => match display {
            Display::Omitted => json!({ "type": "adaptive" }),
            Display::Summarized => json!({ "type": "adaptive", "display": display.as_str() }),
        },
        Thinking::Disabled => json!({ "type": "disabled" }),
    }
}

fn block(block: TurnBlock) -> Value {
    match block {
        TurnBlock::Text(text) => json!({ "type": "text", "text": text }),
        // Echoed back byte-identical, signature included: the API rejects a
        // modified thinking block.
        TurnBlock::Thinking(thinking) => json!({
            "type": "thinking",
            "thinking": thinking.text,
            "signature": thinking.signature,
        }),
        TurnBlock::ToolUse(use_) => json!({
            "type": "tool_use",
            "id": use_.call.as_str(),
            "name": use_.name,
            "input": use_.args,
        }),
        TurnBlock::ToolResult(result) => json!({
            "type": "tool_result",
            "tool_use_id": result.call.as_str(),
            "content": result.content,
            "is_error": result.is_error,
        }),
    }
}

/// Everything in [`body`] except `max_tokens`, `stream` and `output_config`,
/// which `count_tokens` rejects.
pub fn count_tokens_body(config: &ProviderConfig, req: &TurnRequest) -> Value {
    let mut body = json!({ "model": config.model });
    if !req.tools.is_empty() {
        body["tools"] = tools(req);
    }
    if !req.system.is_empty() {
        body["system"] = system(&req.system);
    }
    let messages: Vec<Value> = req
        .messages
        .iter()
        .map(|message| {
            let role = match message.role {
                Role::Assistant => "assistant",
                Role::User => "user",
            };
            let content: Vec<Value> = message.content.iter().cloned().map(block).collect();
            json!({ "role": role, "content": content })
        })
        .collect();
    body["messages"] = json!(messages);
    body
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use journal::CallId;
    use secrecy::SecretString;
    use serde_json::json;

    use super::*;
    use crate::config::{Auth, DEFAULT_MODEL};
    use crate::provider::{TurnMessage, TurnThinking, TurnTool, TurnToolUse};

    fn config() -> ProviderConfig {
        ProviderConfig::new(Auth::ApiKey(SecretString::new("sk-test".into())))
    }

    fn request() -> TurnRequest {
        TurnRequest {
            messages: vec![TurnMessage {
                content: vec![TurnBlock::Text("hello".into())],
                role: Role::User,
            }],
            system: "You are summer-code.".into(),
            tools: vec![TurnTool {
                description: "Read a file".into(),
                input_schema: json!({"type": "object"}),
                name: "read".into(),
            }],
        }
    }

    #[test]
    fn thinking_is_adaptive_and_always_present() {
        // Omitting the field runs without thinking, which is the quiet wrong
        // answer for an agent, so its absence is the bug worth catching.
        let body = body(&config(), request(), true);
        assert_eq!(body["thinking"], json!({ "type": "adaptive" }));
    }

    #[test]
    fn summarized_thinking_asks_for_the_text() {
        let config = config().with_display(Display::Summarized);
        let body = body(&config, request(), true);
        assert_eq!(
            body["thinking"],
            json!({ "type": "adaptive", "display": "summarized" })
        );
    }

    #[test]
    fn effort_is_nested_under_output_config_when_set() {
        // Effort is opt-in: absent unless a provider sets one (some models 400
        // on it), and nested under `output_config` rather than top-level when
        // it is.
        let without = body(&config(), request(), true);
        assert!(
            without.get("output_config").is_none(),
            "no effort by default"
        );

        let with = body(
            &config().with_effort(crate::config::Effort::Xhigh),
            request(),
            true,
        );
        assert_eq!(with["output_config"], json!({ "effort": "xhigh" }));
        assert!(
            with.get("effort").is_none(),
            "effort is not a top-level field"
        );
    }

    #[test]
    fn sampling_parameters_are_never_sent() {
        // All three are a 400 on this model. Steering is prompt-only.
        let body = body(&config(), request(), true);
        for banned in ["temperature", "top_p", "top_k"] {
            assert!(body.get(banned).is_none(), "{banned} must not be sent");
        }
    }

    #[test]
    fn model_and_max_tokens_are_the_defaults() {
        // Against `DEFAULT_MODEL` rather than a literal: this asserts that the
        // configured model reaches the wire, not which id is current, and a
        // literal here turns every model bump into an unrelated test failure.
        let body = body(&config(), request(), true);
        assert_eq!(body["model"], json!(DEFAULT_MODEL));
        assert_eq!(body["max_tokens"], json!(64_000));
    }

    #[test]
    fn exactly_two_cache_breakpoints_on_the_system_and_last_message_block() {
        // A missing breakpoint is silent: no error, cache_creation_input_tokens
        // stays 0, and every turn pays full price.
        let body = body(&config(), request(), true);
        assert_eq!(
            body["system"][0]["cache_control"],
            json!({ "type": "ephemeral" })
        );
        let last = &body["messages"][0]["content"][0];
        assert_eq!(last["cache_control"], json!({ "type": "ephemeral" }));
        assert_eq!(count_breakpoints(&body), 2);
    }

    #[test]
    fn the_breakpoint_lands_on_the_last_block_of_the_last_message_only() {
        let mut req = request();
        req.messages.push(TurnMessage {
            content: vec![
                TurnBlock::Text("first".into()),
                TurnBlock::Text("second".into()),
            ],
            role: Role::Assistant,
        });
        let body = body(&config(), req, true);
        assert!(
            body["messages"][0]["content"][0]
                .get("cache_control")
                .is_none()
        );
        assert!(
            body["messages"][1]["content"][0]
                .get("cache_control")
                .is_none()
        );
        assert_eq!(
            body["messages"][1]["content"][1]["cache_control"],
            json!({ "type": "ephemeral" })
        );
        assert_eq!(count_breakpoints(&body), 2);
    }

    #[test]
    fn thinking_blocks_round_trip_their_signature_onto_the_wire() {
        let mut req = request();
        req.messages.push(TurnMessage {
            content: vec![TurnBlock::Thinking(TurnThinking {
                signature: "EqoBCkgIAx==".into(),
                text: "reasoning".into(),
            })],
            role: Role::Assistant,
        });
        let body = body(&config(), req, true);
        let block = &body["messages"][1]["content"][0];
        assert_eq!(block["type"], json!("thinking"));
        assert_eq!(block["signature"], json!("EqoBCkgIAx=="));
        assert_eq!(block["thinking"], json!("reasoning"));
    }

    #[test]
    fn tool_use_carries_the_provider_call_id() {
        let mut req = request();
        req.messages.push(TurnMessage {
            content: vec![TurnBlock::ToolUse(TurnToolUse {
                args: json!({"path": "a.rs"}),
                call: CallId::new("toolu_1".into()),
                name: "read".into(),
            })],
            role: Role::Assistant,
        });
        let body = body(&config(), req, true);
        let block = &body["messages"][1]["content"][0];
        assert_eq!(block["type"], json!("tool_use"));
        assert_eq!(block["id"], json!("toolu_1"));
        assert_eq!(block["input"], json!({"path": "a.rs"}));
    }

    #[test]
    fn count_tokens_body_drops_what_that_endpoint_rejects() {
        let body = count_tokens_body(&config(), &request());
        for absent in ["max_tokens", "stream", "output_config"] {
            assert!(body.get(absent).is_none(), "{absent} must not be sent");
        }
        assert_eq!(body["model"], json!(DEFAULT_MODEL));
        assert!(body["messages"].is_array());
    }

    fn count_breakpoints(body: &Value) -> usize {
        let mut count = 0;
        if let Some(blocks) = body["system"].as_array() {
            count += blocks
                .iter()
                .filter(|block| block.get("cache_control").is_some())
                .count();
        }
        if let Some(messages) = body["messages"].as_array() {
            for message in messages {
                if let Some(blocks) = message["content"].as_array() {
                    count += blocks
                        .iter()
                        .filter(|block| block.get("cache_control").is_some())
                        .count();
                }
            }
        }
        count
    }
}
