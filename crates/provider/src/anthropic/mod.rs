//! The Anthropic Messages API client, streaming.
//!
//! Retry lives here, behind the [`Provider`] trait, so the loop never sees a
//! 429. That includes **mid-stream** failure: a stream that dies before
//! `message_stop` discards its partial turn and re-sends the request whole.
//! That is the honest option, because a partial turn cannot be resumed (there is
//! no wire mechanism to continue an interrupted assistant message) and half a
//! turn cannot be journaled without inventing a `ModelTurn` the model never
//! finished. The cost is re-billing the input tokens, which prompt caching makes
//! small.
//!
//! Because retry spans the stream, the returned stream owns the request and
//! re-issues it: [`turn`](Provider::turn) does not resolve to a "the response
//! arrived" moment after which failures are the caller's problem.

mod sse;
mod wire;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures_util::stream::{self, BoxStream, StreamExt};
use serde_json::Value;

use crate::config::{ANTHROPIC_STREAM_IDLE_TIMEOUT, Auth, CONNECT_TIMEOUT, Effort, ProviderConfig};
use crate::provider::{Attempt, Provider, ProviderError, StreamEvent, TurnRequest};
use crate::retry::{MAX_ATTEMPTS, backoff, classify, classify_transport};
use secrecy::ExposeSecret;
use sse::SseDecoder;

// ========================================================================
// Data Structures
// ========================================================================

/// A streaming Anthropic Messages client.
pub struct AnthropicProvider {
    attempts: Arc<Mutex<Vec<Attempt>>>,
    client: reqwest::Client,
    config: ProviderConfig,
}

/// What the turn stream needs to re-issue its request after a mid-stream break.
struct TurnState {
    /// Shared with the provider, so a failure the *stream* hits still reaches
    /// the journal. Retry spans the stream, so the retries that matter most
    /// happen after `turn()` has already returned.
    attempts: Arc<Mutex<Vec<Attempt>>>,
    attempt: u32,
    body: Value,
    bytes: Option<BoxStream<'static, reqwest::Result<bytes::Bytes>>>,
    decoder: SseDecoder,
    finished: bool,
    request: RequestParts,
}

/// The parts of a request the stream re-sends with. Cloned out of the provider
/// so the stream can be `'static`.
#[derive(Clone)]
struct RequestParts {
    api_key: Option<String>,
    bearer: Option<String>,
    client: reqwest::Client,
    extra_headers: &'static [(&'static str, &'static str)],
    url: reqwest::Url,
}

// ========================================================================
// AnthropicProvider Implementations
// ========================================================================

impl AnthropicProvider {
    /// **`read_timeout`, not `timeout`, and the difference is a whole turn.**
    ///
    /// reqwest's `timeout` is a budget on the *entire* request including the
    /// body, and a streaming turn's body is not finished until the model stops
    /// generating. At 64k `max_tokens` with xhigh adaptive thinking, a legitimate
    /// turn passes 600 seconds and the timeout fires mid-stream -- on a turn that
    /// was working. The retry loop then restarts it from zero, up to five times,
    /// each attempt failing at the same place: roughly five times the cost for a
    /// guaranteed failure, and the longer the turn the likelier it is.
    ///
    /// `read_timeout` bounds the gap *between* chunks instead, which is the
    /// thing that actually indicates a dead connection. A generating model sends
    /// something well inside the window; a hung one sends nothing and is cut off
    /// as intended.
    ///
    /// The total budget stays where it makes sense: `count_tokens` is a single
    /// small non-streaming response, so it is bounded per-request in
    /// [`AnthropicProvider::count_tokens`].
    pub fn new(config: ProviderConfig) -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(config.request_timeout.min(ANTHROPIC_STREAM_IDLE_TIMEOUT))
            .build()
            .unwrap_or_default();
        Self {
            attempts: Arc::new(Mutex::new(Vec::new())),
            client,
            config,
        }
    }

    /// Construct from the environment. `None` when no credential is set; the CLI
    /// treats that as "live model unavailable".
    pub fn from_env() -> Option<Self> {
        ProviderConfig::from_env().ok().map(Self::new)
    }

    pub fn config(&self) -> &ProviderConfig {
        &self.config
    }

    fn record(&self, error: &ProviderError, attempt: u32) {
        record_attempt(&self.attempts, error, attempt);
    }

    fn parts(&self, url: reqwest::Url) -> RequestParts {
        let (api_key, bearer) = match &self.config.auth {
            Auth::ApiKey(key) => (Some(key.expose_secret().to_string()), None),
            Auth::BearerToken(token) => (None, Some(token.expose_secret().to_string())),
        };
        RequestParts {
            api_key,
            bearer,
            client: self.client.clone(),
            extra_headers: self.config.extra_headers,
            url,
        }
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn model(&self) -> &str {
        &self.config.model
    }

    async fn turn(
        &self,
        req: TurnRequest,
    ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
        let body = wire::body(&self.config, req, true);
        let parts = self.parts(self.config.messages_url());

        // Open the stream here so a fatal request error (a 400 naming the
        // parameter that is wrong) surfaces from `turn` rather than as an empty
        // stream the loop has to interpret.
        let mut attempt = 0u32;
        let bytes = loop {
            attempt += 1;
            // No total budget: the body is the model generating, and
            // `read_timeout` is what detects a dead connection.
            match parts.send(&body, None).await {
                Ok(response) => break response.bytes_stream().boxed(),
                Err(error) => {
                    self.record(&error, attempt);
                    if !error.is_retryable() || attempt >= MAX_ATTEMPTS {
                        return Err(error);
                    }
                    tokio::time::sleep(backoff(&error, attempt)).await;
                }
            }
        };

        let state = TurnState {
            attempt,
            attempts: self.attempts.clone(),
            body,
            bytes: Some(bytes),
            decoder: SseDecoder::new(),
            finished: false,
            request: parts,
        };
        Ok(drive(state).boxed())
    }

    async fn count_tokens(&self, req: &TurnRequest) -> Result<u32, ProviderError> {
        // Exact and effectively free. Never a tokenizer crate: `tiktoken` is
        // OpenAI's and undercounts Claude by 15-20% on prose, more on code.
        let body = wire::count_tokens_body(&self.config, req);
        let parts = self.parts(self.config.count_tokens_url());

        let mut attempt = 0u32;
        let response = loop {
            attempt += 1;
            // One small non-streaming response, so a total budget is
            // exactly the right shape here.
            match parts.send(&body, Some(self.config.request_timeout)).await {
                Ok(response) => break response,
                Err(error) => {
                    self.record(&error, attempt);
                    if !error.is_retryable() || attempt >= MAX_ATTEMPTS {
                        return Err(error);
                    }
                    tokio::time::sleep(backoff(&error, attempt)).await;
                }
            }
        };

        let value: Value = response
            .json()
            .await
            .map_err(|error| classify_transport(&error))?;
        value
            .get("input_tokens")
            .and_then(Value::as_u64)
            .map(|count| count as u32)
            .ok_or_else(|| ProviderError::Fatal {
                kind: "malformed_response".into(),
                message: "count_tokens response had no input_tokens".into(),
                // Not a response status: the request succeeded and the body
                // was wrong.
                status: None,
            })
    }

    fn drain_attempts(&self) -> Vec<Attempt> {
        self.attempts
            .lock()
            .map(|mut attempts| std::mem::take(&mut *attempts))
            .unwrap_or_default()
    }

    fn set_effort(&mut self, effort: Option<Effort>) {
        self.config.effort = effort;
    }

    fn max_output_tokens(&self) -> Option<u32> {
        Some(self.config.max_tokens)
    }

    fn with_model(&self, model: &str) -> Option<Box<dyn Provider>> {
        let mut cloned_config = self.config.clone();
        cloned_config.model = model.to_string();
        Some(Box::new(Self::new(cloned_config)))
    }
}

// ========================================================================
// RequestParts Implementations
// ========================================================================

impl RequestParts {
    /// One POST. The header pairing is credential-specific and the wrong one is
    /// a 401: an API key goes in `x-api-key`, an OAuth token goes in
    /// `Authorization: Bearer` alongside the beta header.
    /// Send `body`, optionally bounding the *whole* exchange.
    ///
    /// `total` is `Some` only for non-streaming requests. On a streaming one it
    /// would be a cap on how long the model may generate, which is not a
    /// connection health check: see [`AnthropicProvider::new`].
    async fn send(
        &self,
        body: &Value,
        total: Option<std::time::Duration>,
    ) -> Result<reqwest::Response, ProviderError> {
        let mut request = self
            .client
            .post(self.url.clone())
            .header("anthropic-version", crate::config::ANTHROPIC_VERSION);
        if let Some(total) = total {
            request = request.timeout(total);
        }

        if let Some(key) = &self.api_key {
            request = request.header("x-api-key", key);
        }
        if let Some(token) = &self.bearer {
            request = request
                .header("authorization", format!("Bearer {token}"))
                .header("anthropic-beta", crate::config::OAUTH_BETA);
        }
        for (name, value) in self.extra_headers {
            request = request.header(*name, *value);
        }

        let response = request
            .json(body)
            .send()
            .await
            .map_err(|error| classify_transport(&error))?;

        if response.status().is_success() {
            Ok(response)
        } else {
            Err(classify(response).await)
        }
    }
}

// ========================================================================
// Functions
// ========================================================================

/// Drive the byte stream through the decoder, re-sending the whole turn if the
/// bytes stop before `message_stop`.
fn drive(state: TurnState) -> impl stream::Stream<Item = StreamEvent> {
    stream::unfold(state, |mut state| async move {
        loop {
            if let Some(event) = state.decoder.next_event() {
                return Some((event, state));
            }
            if state.finished {
                return None;
            }

            // No byte stream left: the attempts are spent and the turn ends
            // with no TurnEnd, which the loop reads as a broken stream.
            let bytes = state.bytes.as_mut()?;

            match bytes.next().await {
                Some(Ok(chunk)) => {
                    // Bytes, not lossy-decoded text: a character straddling a
                    // chunk boundary must not become U+FFFD.
                    state.decoder.push_bytes(&chunk);
                    // An in-stream `error` event ends this attempt with the
                    // server's own words, so the reason reaches the journal and
                    // the user rather than being flattened into "the bytes
                    // stopped".
                    if let Some(reason) = state.decoder.take_error() {
                        let broken = ProviderError::StreamBroken(reason);
                        if !state.restart(&broken).await {
                            return None;
                        }
                        return Some((StreamEvent::TurnRestarted, state));
                    }
                }
                Some(Err(error)) => {
                    let broken = ProviderError::StreamBroken(error.to_string());
                    if !state.restart(&broken).await {
                        return None;
                    }
                    return Some((StreamEvent::TurnRestarted, state));
                }
                None => {
                    if state.decoder.saw_message_stop() {
                        // The turn completed. Any TurnEnd is already queued.
                        state.finished = true;
                    } else {
                        // Bytes stopped without message_stop. The partial turn is
                        // discarded, not reported: emitting a TurnEnd here would
                        // journal a ModelTurn the model never finished.
                        let broken =
                            ProviderError::StreamBroken("bytes ended before message_stop".into());
                        if !state.restart(&broken).await {
                            return None;
                        }
                        return Some((StreamEvent::TurnRestarted, state));
                    }
                }
            }
        }
    })
}

// ========================================================================
// TurnState Implementations
// ========================================================================

impl TurnState {
    /// Discard the partial turn and re-send whole. Returns false when the
    /// attempts are spent, which ends the stream with no `TurnEnd`.
    ///
    /// Every failure on the way is recorded, including the ones this method
    /// hits itself. Retry spans the stream, so these are the retries a journal
    /// would otherwise be blindest to: a turn that restarted four times and then
    /// succeeded would look identical to one that went clean.
    async fn restart(&mut self, error: &ProviderError) -> bool {
        self.record(error);
        let mut last = backoff(error, self.attempt);

        while self.attempt < MAX_ATTEMPTS {
            tokio::time::sleep(last).await;
            self.attempt += 1;

            match self.request.send(&self.body, None).await {
                Ok(response) => {
                    self.decoder.reset();
                    self.bytes = Some(response.bytes_stream().boxed());
                    return true;
                }
                Err(error) => {
                    // Same rule as the initial send: only `Fatal` is terminal.
                    // Ending the stream on a 429 here would be an asymmetry the
                    // caller cannot see or act on.
                    self.record(&error);
                    if !error.is_retryable() {
                        break;
                    }
                    last = backoff(&error, self.attempt);
                }
            }
        }

        self.finished = true;
        self.bytes = None;
        false
    }

    fn record(&self, error: &ProviderError) {
        record_attempt(&self.attempts, error, self.attempt);
    }
}

// ========================================================================
// Functions
// ========================================================================

/// Park one transport failure for the engine to journal. The engine is the only
/// journal writer, so the provider records and the engine drains.
fn record_attempt(attempts: &Mutex<Vec<Attempt>>, error: &ProviderError, attempt: u32) {
    if let Ok(mut attempts) = attempts.lock() {
        attempts.push(Attempt {
            attempt,
            failure: error.kind().to_string(),
        });
    }
}
