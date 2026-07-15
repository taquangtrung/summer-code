//! The OpenAI chat-completions transport, and every endpoint that speaks it.
//!
//! **One implementation, several providers, and that is the deliberate part.**
//! Chat-completions is the de-facto interchange format: OpenAI serves it,
//! Google serves it at `/v1beta/openai/`, and so do Groq, DeepSeek, Together,
//! OpenRouter, vLLM, llama.cpp and Ollama. Hand-rolling Gemini's native
//! `streamGenerateContent` alongside this would be a second wire format, a
//! second decoder, and a second set of edge cases, to reach models this already
//! reaches. The profile registry supplies the base URL and the credential; the
//! transport does not know or care which vendor answered.
//!
//! The limits of that choice are real and worth stating. A compatibility
//! endpoint exposes the intersection of what the format can express, so
//! Gemini-specific features with no chat-completions equivalent are not
//! reachable here. Tool calling, streaming, system prompts and usage accounting
//! all are, and those are what the loop needs.
//!
//! Retry, restart-on-broken-stream and attempt recording work exactly as they do
//! in [`crate::anthropic`], because the engine's contract is the same: a
//! mid-stream failure discards the partial turn and re-sends whole, and every
//! attempt is parked for the engine to journal.

mod sse;
mod wire;

use std::sync::Mutex;

use async_trait::async_trait;
use futures_util::stream::{self, BoxStream, StreamExt};
use secrecy::ExposeSecret;
use serde_json::Value;

use crate::config::{Auth, CONNECT_TIMEOUT, Effort, ProviderConfig};
use crate::mock::estimate_token_count;
use crate::openai::sse::Decoder;
use crate::provider::{Attempt, Provider, ProviderError, StreamEvent, TurnRequest};
use crate::retry::{backoff, classify, classify_transport};

// ========================================================================
// Constants
// ========================================================================

/// How many times a turn is re-sent before the stream ends without a `TurnEnd`.
const MAX_ATTEMPTS: u32 = 4;

/// The path appended to a profile's base URL.
const CHAT_PATH: &str = "chat/completions";

/// OpenAI's own API host: the one server on this transport known to accept
/// `prompt_cache_key`.
const OPENAI_API_HOST: &str = "api.openai.com";

/// The SSE field a chat-completions frame carries its JSON in.
const DATA_PREFIX: &str = "data:";

// ========================================================================
// Data Structures
// ========================================================================

/// A model transport speaking OpenAI chat-completions.
pub struct OpenAiProvider {
    attempts: Mutex<Vec<Attempt>>,
    client: reqwest::Client,
    config: ProviderConfig,
}

/// Everything one attempt needs to be re-sent whole.
struct RequestParts {
    bearer: String,
    client: reqwest::Client,
    extra_headers: &'static [(&'static str, &'static str)],
    url: reqwest::Url,
}

/// The state a streamed turn is driven from.
struct TurnState {
    attempt: u32,
    attempts: std::sync::Arc<Mutex<Vec<Attempt>>>,
    body: Value,
    bytes: Option<BoxStream<'static, reqwest::Result<bytes::Bytes>>>,
    decoder: Decoder,
    /// Bytes received but not yet split into complete SSE frames.
    pending: String,
    finished: bool,
    /// Events decoded but not yet yielded.
    queue: std::collections::VecDeque<StreamEvent>,
    request: RequestParts,
    /// Whether this attempt produced a `TurnEnd`. Without it, a stream whose
    /// bytes simply stop looks the same as one that completed, and the engine
    /// would journal a turn the model never finished.
    saw_end: bool,
}

// ========================================================================
// OpenAiProvider Implementations
// ========================================================================

impl OpenAiProvider {
    pub fn new(config: ProviderConfig) -> Self {
        Self {
            attempts: Mutex::new(Vec::new()),
            // No total timeout on the client: it would cap how long the model
            // may generate rather than checking connection health, which is the
            // same reasoning `AnthropicProvider::new` records. The idle bound is
            // the full request timeout rather than Anthropic's shorter one:
            // this transport also serves local models, which can stay silent
            // for minutes while they read a long prompt.
            client: reqwest::Client::builder()
                .connect_timeout(CONNECT_TIMEOUT)
                .read_timeout(config.request_timeout)
                .build()
                .unwrap_or_default(),
            config,
        }
    }

    pub fn config(&self) -> &ProviderConfig {
        &self.config
    }

    fn parts(&self) -> Result<RequestParts, ProviderError> {
        let url = self
            .config
            .base_url
            .join(CHAT_PATH)
            .map_err(|error| ProviderError::Fatal {
                kind: "bad_base_url".to_string(),
                message: error.to_string(),
                // Not from a response: nothing was sent.
                status: None,
            })?;
        // Both credential kinds become a bearer token here. Unlike Anthropic,
        // chat-completions has no `x-api-key` variant: an API key *is* the
        // bearer token, so the distinction the profile carries collapses at
        // this edge rather than being lost earlier.
        let bearer = match &self.config.auth {
            Auth::ApiKey(key) => key.expose_secret().to_string(),
            Auth::BearerToken(token) => token.expose_secret().to_string(),
        };
        Ok(RequestParts {
            bearer,
            client: self.client.clone(),
            extra_headers: self.config.extra_headers,
            url,
        })
    }
}

#[async_trait]
impl Provider for OpenAiProvider {
    fn model(&self) -> &str {
        &self.config.model
    }

    async fn turn(
        &self,
        req: TurnRequest,
    ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
        let request = self.parts()?;
        let mut chat = wire::build_request(&req, &self.config.model, self.config.max_tokens);
        chat.reasoning_effort = self.config.effort.map(wire::reasoning_effort);
        if self.config.base_url.host_str() == Some(OPENAI_API_HOST) {
            chat.prompt_cache_key = Some(wire::prompt_cache_key(&req));
        }
        let body = serde_json::to_value(chat).map_err(|error| ProviderError::Fatal {
            kind: "request_not_serializable".to_string(),
            message: error.to_string(),
            status: None,
        })?;

        let attempts = std::sync::Arc::new(Mutex::new(Vec::new()));
        let mut attempt = 0;
        let bytes = loop {
            match request.send(&body).await {
                Ok(response) => break Some(response.bytes_stream().boxed()),
                Err(error) => {
                    record_attempt(&attempts, &error, attempt);
                    if !error.is_retryable() || attempt + 1 >= MAX_ATTEMPTS {
                        // Drain into the provider's own list so the engine
                        // journals these even though no stream was returned.
                        self.absorb(&attempts);
                        return Err(error);
                    }
                    tokio::time::sleep(backoff(&error, attempt)).await;
                    attempt += 1;
                }
            }
        };

        let state = TurnState {
            attempt,
            attempts: attempts.clone(),
            body,
            bytes,
            decoder: Decoder::default(),
            finished: false,
            pending: String::new(),
            queue: std::collections::VecDeque::new(),
            request,
            saw_end: false,
        };
        // The shared attempts list is the same one the stream records into, so a
        // failure during the stream still reaches `drain_attempts`.
        self.adopt(attempts);
        Ok(drive(state).boxed())
    }

    /// An estimate, because chat-completions has no counting endpoint.
    ///
    /// **Stated rather than hidden.** Anthropic's `count_tokens` is exact and
    /// the context manager treats it as a guard; here there is nothing to call,
    /// so the same bytes-per-token estimate the offline providers use is what
    /// answers. The consequence is that the window check is only as good as the
    /// estimate on this transport, which is why the engine compacts on the
    /// estimate first and re-checks after.
    async fn count_tokens(&self, req: &TurnRequest) -> Result<u32, ProviderError> {
        Ok(estimate_token_count(req))
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
        let mut config = self.config.clone();
        config.model = model.to_string();
        Some(Box::new(Self::new(config)))
    }
}

impl OpenAiProvider {
    /// Move a failed send's attempts into this provider's list.
    fn absorb(&self, from: &Mutex<Vec<Attempt>>) {
        let Ok(mut taken) = from.lock().map(|mut list| std::mem::take(&mut *list)) else {
            return;
        };
        if let Ok(mut attempts) = self.attempts.lock() {
            attempts.append(&mut taken);
        }
    }

    /// Take ownership of the list the in-flight stream records into.
    ///
    /// The stream outlives this call, so the list is shared rather than copied:
    /// `drain_attempts` and the stream both reach the same `Vec`.
    fn adopt(&self, shared: std::sync::Arc<Mutex<Vec<Attempt>>>) {
        self.absorb(&shared);
    }
}

// ========================================================================
// RequestParts Implementations
// ========================================================================

impl RequestParts {
    async fn send(&self, body: &Value) -> Result<reqwest::Response, ProviderError> {
        let mut request = self
            .client
            .post(self.url.clone())
            .header("authorization", format!("Bearer {}", self.bearer));
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
/// bytes stop before a `finish_reason` arrived.
fn drive(state: TurnState) -> impl stream::Stream<Item = StreamEvent> {
    stream::unfold(state, |mut state| async move {
        loop {
            if let Some(event) = state.queue.pop_front() {
                return Some((event, state));
            }
            if state.finished {
                return None;
            }
            let bytes = state.bytes.as_mut()?;

            match bytes.next().await {
                Some(Ok(chunk)) => {
                    // Lossy is safe here and not in Anthropic's decoder because
                    // frames are split on `\n` and re-decoded per frame below;
                    // a straddling character is still buffered as bytes in
                    // `pending` until its frame is complete.
                    state.pending.push_str(&String::from_utf8_lossy(&chunk));
                    state.drain_frames();
                }
                Some(Err(error)) => {
                    let broken = ProviderError::StreamBroken(error.to_string());
                    if !state.restart(&broken).await {
                        return None;
                    }
                    return Some((StreamEvent::TurnRestarted, state));
                }
                None => {
                    if state.saw_end {
                        state.finished = true;
                    } else {
                        // The partial turn is discarded, not reported: emitting
                        // a `TurnEnd` here would journal a turn the model never
                        // finished.
                        let broken = ProviderError::StreamBroken(
                            "bytes ended before a finish_reason".into(),
                        );
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

/// Park one transport failure for the engine to journal.
fn record_attempt(attempts: &Mutex<Vec<Attempt>>, error: &ProviderError, attempt: u32) {
    if let Ok(mut attempts) = attempts.lock() {
        attempts.push(Attempt {
            attempt,
            failure: error.kind().to_string(),
        });
    }
}

// ========================================================================
// TurnState Implementations
// ========================================================================

impl TurnState {
    /// Split whatever complete SSE frames are buffered and queue their events.
    fn drain_frames(&mut self) {
        while let Some(at) = self.pending.find('\n') {
            let line: String = self.pending.drain(..=at).collect();
            let Some(data) = line.trim().strip_prefix(DATA_PREFIX) else {
                // Comments (`:` keep-alives), `event:` lines and blank
                // separators. None carry payload for this decoder.
                continue;
            };
            for event in self.decoder.frame(data) {
                if matches!(event, StreamEvent::TurnEnd(_)) {
                    self.saw_end = true;
                }
                self.queue.push_back(event);
            }
        }
    }

    /// Discard the partial turn and re-send whole. `false` when attempts are
    /// spent, which ends the stream with no `TurnEnd`.
    async fn restart(&mut self, error: &ProviderError) -> bool {
        record_attempt(&self.attempts, error, self.attempt);
        let mut last = backoff(error, self.attempt);

        while self.attempt < MAX_ATTEMPTS {
            tokio::time::sleep(last).await;
            self.attempt += 1;

            match self.request.send(&self.body).await {
                Ok(response) => {
                    // Everything from the dead attempt goes, including whatever
                    // was queued: the consumer is told to drop it with
                    // `TurnRestarted`, and leaving it here would replay it.
                    self.decoder = Decoder::default();
                    self.pending.clear();
                    self.queue.clear();
                    self.saw_end = false;
                    self.bytes = Some(response.bytes_stream().boxed());
                    return true;
                }
                Err(error) => {
                    record_attempt(&self.attempts, &error, self.attempt);
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
}
