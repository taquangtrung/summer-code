//! GitHub Copilot as a [`Provider`]: the same Anthropic-Messages transport
//! every other profile uses, wrapped with the token lifecycle Copilot alone
//! needs. See [`oauth`] for the device flow and the session-token exchange
//! this wraps around.

mod oauth;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures_util::stream::BoxStream;
use secrecy::SecretString;
use tokio::sync::Mutex as AsyncMutex;

pub(crate) use oauth::CHAT_HEADERS;
pub use oauth::{
    ACCESS_TOKEN_URL, CopilotAuthError, CopilotModel, DEVICE_CODE_URL, DeviceCode, PollOutcome,
    SESSION_TOKEN_URL, SessionToken, exchange_session_token, fetch_models, poll_device_flow,
    start_device_flow,
};

use crate::anthropic::AnthropicProvider;
use crate::config::{Auth, Effort, ProviderConfig};
use crate::profile::ProviderId;
use crate::provider::{Attempt, Provider, ProviderError, StreamEvent, TurnRequest};

// ========================================================================
// Constants
// ========================================================================

/// Refresh this much before the session token's real expiry: a request
/// started right at the edge would otherwise race the token's own
/// server-side expiry rather than this cache's.
const REFRESH_MARGIN: Duration = Duration::from_secs(60);

// ========================================================================
// Data Structures
// ========================================================================

/// Reaches GitHub Copilot's Anthropic-compatible endpoint through a
/// short-lived session token this refreshes before it expires. Wraps a plain
/// [`AnthropicProvider`] rather than reimplementing streaming and retry: only
/// the credential and base URL are Copilot-specific, and both are resolved
/// into a fresh [`ProviderConfig`] each time the cache turns over.
pub struct CopilotProvider {
    /// Attempts drained from each cache generation's inner provider right
    /// after it is used, so a refresh mid-session does not lose the ones
    /// recorded against the provider it replaces.
    attempts: Mutex<Vec<Attempt>>,
    cache: AsyncMutex<Option<Cached>>,
    client: reqwest::Client,
    github_token: SecretString,
    model: String,
    /// The reasoning effort sent on every cached provider built from a refreshed
    /// session token (`None` = do not send -- some models 400 on it). A
    /// `/effort` change survives the ~25-minute token refresh because it is read
    /// here each rebuild.
    effort: Option<Effort>,
}

/// One cache generation: the provider built from a session token, and when
/// that token needs replacing.
struct Cached {
    expires_at: Instant,
    inner: Arc<AnthropicProvider>,
}

// ========================================================================
// CopilotProvider Implementations
// ========================================================================

impl CopilotProvider {
    /// `model` overrides the profile's default the same way
    /// [`ProviderConfig::for_profile`] does for every other provider.
    /// `github_token` is the long-lived token from `/login`'s device flow (or
    /// a hand-set env var) — not yet exchanged for anything Copilot's API
    /// will accept, which only happens lazily on first use.
    pub fn new(github_token: SecretString, model: Option<&str>) -> Self {
        let profile = ProviderId::GithubCopilot.profile();
        Self {
            attempts: Mutex::new(Vec::new()),
            cache: AsyncMutex::new(None),
            client: reqwest::Client::new(),
            github_token,
            model: model.unwrap_or(profile.default_model).to_string(),
            effort: None,
        }
    }

    /// The provider for the current session token, refreshing first if the
    /// cached one is missing or close to expiry.
    ///
    /// Holds the lock across the refresh's network round-trip on purpose:
    /// Copilot's token exchange is rare (every ~25-30 minutes) and this
    /// engine drives one turn at a time, so serializing a concurrent caller
    /// behind an in-flight refresh is cheaper and simpler than the
    /// coordination needed to avoid it.
    async fn provider(&self) -> Result<Arc<AnthropicProvider>, ProviderError> {
        let mut guard = self.cache.lock().await;
        if let Some(cached) = guard.as_ref()
            && cached.expires_at > Instant::now()
        {
            return Ok(cached.inner.clone());
        }

        let session = oauth::exchange_session_token(
            &self.client,
            oauth::SESSION_TOKEN_URL,
            &self.github_token,
        )
        .await
        .map_err(map_auth_error)?;
        let config = ProviderConfig::new(Auth::BearerToken(session.token))
            .with_base_url(session.base_url)
            .with_extra_headers(CHAT_HEADERS)
            .with_model(self.model.clone());
        let config = match self.effort {
            Some(effort) => config.with_effort(effort),
            None => config,
        };
        let inner = Arc::new(AnthropicProvider::new(config));
        let expires_at = Instant::now() + session.expires_in.saturating_sub(REFRESH_MARGIN);
        *guard = Some(Cached {
            expires_at,
            inner: inner.clone(),
        });
        Ok(inner)
    }

    fn collect_attempts(&self, provider: &AnthropicProvider) {
        let mut drained = provider.drain_attempts();
        if let Ok(mut attempts) = self.attempts.lock() {
            attempts.append(&mut drained);
        }
    }
}

#[async_trait]
impl Provider for CopilotProvider {
    fn model(&self) -> &str {
        &self.model
    }

    async fn turn(
        &self,
        req: TurnRequest,
    ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
        let provider = self.provider().await?;
        let result = provider.turn(req).await;
        self.collect_attempts(&provider);
        result
    }

    async fn count_tokens(&self, req: &TurnRequest) -> Result<u32, ProviderError> {
        let provider = self.provider().await?;
        let result = provider.count_tokens(req).await;
        self.collect_attempts(&provider);
        result
    }

    fn drain_attempts(&self) -> Vec<Attempt> {
        self.attempts
            .lock()
            .map(|mut attempts| std::mem::take(&mut *attempts))
            .unwrap_or_default()
    }

    fn set_effort(&mut self, effort: Option<Effort>) {
        // Remember the choice for the next session-token refresh, and drop the
        // cached provider so the next turn rebuilds it with the new effort.
        // `try_lock` (not `blocking_lock`) because this runs on the runtime's
        // thread, which the latter refuses to block; between turns the cache is
        // uncontended, and mid-turn the rebuild-on-refresh applies it anyway.
        self.effort = effort;
        if let Ok(mut guard) = self.cache.try_lock() {
            *guard = None;
        }
    }

    fn with_model(&self, model: &str) -> Option<Box<dyn Provider>> {
        Some(Box::new(Self {
            attempts: Mutex::new(Vec::new()),
            cache: AsyncMutex::new(None),
            client: self.client.clone(),
            github_token: self.github_token.clone(),
            model: model.to_string(),
            effort: self.effort,
        }))
    }
}

// ========================================================================
// Functions
// ========================================================================

/// A 5xx from the token exchange is the same "try again" shape as one from
/// the chat endpoint itself; anything else (a revoked token, a malformed
/// response) is not something a retry fixes.
fn map_auth_error(error: CopilotAuthError) -> ProviderError {
    match error {
        CopilotAuthError::Transport(message) => ProviderError::Transport(message),
        CopilotAuthError::Http { status, body: _ } if (500..600).contains(&status) => {
            ProviderError::Server { status }
        }
        CopilotAuthError::Http { status, body } => ProviderError::Fatal {
            kind: "copilot_auth".into(),
            message: body,
            status: Some(status),
        },
        error @ (CopilotAuthError::DeviceFlow(_)
        | CopilotAuthError::MalformedResponse(_)
        | CopilotAuthError::MalformedToken) => ProviderError::Fatal {
            kind: "copilot_auth".into(),
            message: error.to_string(),
            status: None,
        },
    }
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_5xx_from_the_token_exchange_is_retryable() {
        // The bug this catches: a transient GitHub outage during the token
        // exchange must not permanently fail the turn the same way a 401
        // (revoked token) should -- retry.is_retryable() is what the caller
        // uses to decide, so the mapping has to land on `Server`, not `Fatal`.
        let error = map_auth_error(CopilotAuthError::Http {
            body: "oops".into(),
            status: 503,
        });
        assert!(matches!(error, ProviderError::Server { status: 503 }));
        assert!(error.is_retryable());
    }

    #[test]
    fn a_4xx_from_the_token_exchange_is_fatal_not_retried() {
        // A revoked or expired GitHub token will 401 on every retry
        // identically; retrying it is a wasted round-trip that also delays
        // surfacing the actionable "log in again" error.
        let error = map_auth_error(CopilotAuthError::Http {
            body: "bad credentials".into(),
            status: 401,
        });
        assert!(!error.is_retryable());
    }

    #[test]
    fn a_transport_failure_is_retryable() {
        let error = map_auth_error(CopilotAuthError::Transport("connection reset".into()));
        assert!(error.is_retryable());
    }
}
