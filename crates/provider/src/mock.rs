//! A scripted provider for tests and offline loop demos. Returns its turns in
//! order; the Nth call hands back the Nth scripted turn.

use std::collections::VecDeque;
use std::sync::Mutex;

use async_trait::async_trait;
use futures_util::StreamExt;
use futures_util::stream::{self, BoxStream};

use crate::provider::{Provider, ProviderError, StreamEvent, TurnRequest};

// ========================================================================
// Data Structures
// ========================================================================

/// One scripted turn: the events the mock will stream back, in order.
///
/// A turn whose events omit [`StreamEvent::TurnEnd`] models a stream that died
/// before `message_stop`. That is the only way to exercise the loop's
/// broken-stream path offline, so it is a supported script, not a malformed one.
pub struct ScriptedTurn {
    pub events: Vec<StreamEvent>,
}

/// A provider that replays scripted turns. Panics if asked for more turns than
/// it was given: that is a test-authoring bug, not a runtime condition.
pub struct MockProvider {
    turns: Mutex<VecDeque<ScriptedTurn>>,
}

// ========================================================================
// MockProvider Implementations
// ========================================================================

impl MockProvider {
    pub fn new(turns: Vec<ScriptedTurn>) -> Self {
        Self {
            turns: Mutex::new(turns.into()),
        }
    }
}

#[async_trait]
impl Provider for MockProvider {
    /// Not a model at all, and the journal says so rather than naming one this
    /// session never spoke to.
    fn model(&self) -> &str {
        "mock"
    }

    async fn turn(
        &self,
        _req: TurnRequest,
    ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
        let turn = self
            .turns
            .lock()
            .expect("mock turns lock poisoned")
            .pop_front()
            .expect("MockProvider ran out of scripted turns");
        Ok(stream::iter(turn.events).boxed())
    }

    /// The local estimate, which keeps golden tests offline and is honest about
    /// what they test: the loop, not the arithmetic.
    async fn count_tokens(&self, req: &TurnRequest) -> Result<u32, ProviderError> {
        Ok(estimate_token_count(req))
    }

    fn with_model(&self, _model: &str) -> Option<Box<dyn Provider>> {
        None
    }
}

// ========================================================================
// Functions
// ========================================================================

/// The cheap local estimate the plan allows for packing a window. The real
/// count comes from the API; a test that wanted the arithmetic would be testing
/// the wrong thing. Shared by every offline provider so they agree.
pub(crate) const BYTES_PER_TOKEN: usize = 4;

/// A bytes-per-token estimate of a request's token count. Offline providers
/// ([`MockProvider`], [`crate::Recorded`]) use it for `count_tokens` so golden
/// tests stay offline and honest about what they test: the loop, not the
/// arithmetic.
pub(crate) fn estimate_token_count(req: &TurnRequest) -> u32 {
    let mut bytes = req.system.len();
    for tool in &req.tools {
        bytes += tool.name.len() + tool.description.len() + tool.input_schema.to_string().len();
    }
    for message in &req.messages {
        for block in &message.content {
            bytes += block_len(block);
        }
    }
    (bytes / BYTES_PER_TOKEN) as u32
}

fn block_len(block: &crate::provider::TurnBlock) -> usize {
    use crate::provider::TurnBlock;
    match block {
        TurnBlock::Text(text) => text.len(),
        TurnBlock::Thinking(thinking) => thinking.text.len() + thinking.signature.len(),
        TurnBlock::ToolResult(result) => result.content.len(),
        TurnBlock::ToolUse(use_) => use_.name.len() + use_.args.to_string().len(),
    }
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use journal::{StopReason, Usage};

    use super::*;
    use crate::provider::{TurnEnd, TurnMessage};

    fn turn_end() -> StreamEvent {
        StreamEvent::TurnEnd(TurnEnd {
            stop: StopReason::EndTurn,
            usage: Usage::zero(),
        })
    }

    fn empty_req() -> TurnRequest {
        TurnRequest {
            messages: vec![],
            system: String::new(),
            tools: vec![],
        }
    }

    async fn collect(mut events: BoxStream<'_, StreamEvent>) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        while let Some(event) = events.next().await {
            out.push(event);
        }
        out
    }

    fn text_of(events: &[StreamEvent]) -> String {
        let mut text = String::new();
        for event in events {
            if let StreamEvent::TextDelta(delta) = event {
                text.push_str(delta);
            }
        }
        text
    }

    #[tokio::test]
    async fn replays_scripted_turns_in_order() {
        let provider = MockProvider::new(vec![
            ScriptedTurn {
                events: vec![StreamEvent::TextDelta("hello".into()), turn_end()],
            },
            ScriptedTurn {
                events: vec![turn_end()],
            },
        ]);

        let first = collect(provider.turn(empty_req()).await.unwrap()).await;
        assert_eq!(text_of(&first), "hello");
        let second = collect(provider.turn(empty_req()).await.unwrap()).await;
        assert_eq!(text_of(&second), "");
    }

    #[tokio::test]
    async fn a_turn_without_turn_end_models_a_broken_stream() {
        let provider = MockProvider::new(vec![ScriptedTurn {
            events: vec![StreamEvent::TextDelta("half a th".into())],
        }]);
        let events = collect(provider.turn(empty_req()).await.unwrap()).await;
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, StreamEvent::TurnEnd(_)))
        );
    }

    #[tokio::test]
    async fn count_tokens_estimates_from_the_whole_request() {
        let provider = MockProvider::new(vec![]);
        let req = TurnRequest {
            messages: vec![TurnMessage {
                content: vec![crate::provider::TurnBlock::Text("a".repeat(400))],
                role: crate::provider::Role::User,
            }],
            system: "s".repeat(400),
            tools: vec![],
        };
        assert_eq!(provider.count_tokens(&req).await.unwrap(), 200);
    }
}
