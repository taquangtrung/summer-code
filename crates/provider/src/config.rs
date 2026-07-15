//! Provider configuration.
//!
//! **The values here are API facts, not preferences, and three of them look like
//! mistakes to anyone working from a pre-2026 prior:**
//!
//! - The model id is `claude-opus-5`, with no date suffix. Date-suffixing it
//!   is a 404.
//! - `thinking` is set to adaptive **explicitly**. Omitting the field runs
//!   *without* thinking, which is the quiet wrong answer for an agent, and the
//!   older `{type: "enabled", budget_tokens: N}` shape is a 400 on this model.
//! - Effort is `output_config: {effort: ...}`, not a top-level field. `xhigh`
//!   sits between `high` and `max` and is the right level for coding.
//!
//! **`temperature`, `top_p` and `top_k` are absent on purpose.** All three are a
//! 400 on this model, so a config struct offering a temperature knob would be
//! offering an error. Steering is prompt-only.

use std::time::Duration;

use reqwest::Url;
use secrecy::SecretString;

use crate::profile::{BadBaseUrl, ProviderProfile};

// ========================================================================
// Constants
// ========================================================================

/// The exact model id. No date suffix: a dated snapshot of this name 404s.
///
/// **Thinking is on by default on this model**, unlike `claude-opus-4-8` where
/// omitting the field meant no thinking. The explicit adaptive setting below is
/// therefore belt-and-braces rather than load-bearing, and is kept because the
/// same [`ProviderConfig`] is sent to older ids when a user pins one.
pub const DEFAULT_MODEL: &str = "claude-opus-5";

/// The loop always streams, and at `xhigh` the model needs room to think and act
/// across a turn. Too low surfaces as `StopReason::MaxTokens` and a truncated
/// answer.
pub const DEFAULT_MAX_TOKENS: u32 = 64_000;

pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

/// Streaming makes this a backstop rather than a turn budget.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(600);

/// How long a TCP and TLS connect may take. A host that has not answered in
/// this long is down or unreachable, and the retry loop is the place to find
/// that out, not a request that sits for minutes first.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The longest silence an Anthropic stream may keep before it is treated as
/// dead. The API sends `ping` events through long thinking, so two minutes
/// with no byte at all is a stalled connection, which waiting the full
/// [`DEFAULT_REQUEST_TIMEOUT`] used to leave hanging for ten minutes before
/// the retry began.
pub const ANTHROPIC_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(120);

/// Every request carries this, whichever credential is in use.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Required alongside a bearer token. An OAuth token sent as `x-api-key` is a 401.
pub const OAUTH_BETA: &str = "oauth-2025-04-20";

// ========================================================================
// Data Structures
// ========================================================================

/// Everything the transport needs for a turn.
#[derive(Clone)]
pub struct ProviderConfig {
    pub auth: Auth,
    pub base_url: Url,
    pub effort: Option<Effort>,
    /// Static headers a profile's compatibility layer requires beyond the
    /// credential itself — GitHub Copilot's Anthropic-compatible endpoint
    /// only accepts requests that identify as an editor integration
    /// (`Editor-Version`, `Copilot-Integration-Id`, …); Anthropic and Z.ai
    /// need none, so this is empty for them.
    pub extra_headers: &'static [(&'static str, &'static str)],
    pub max_tokens: u32,
    pub model: String,
    pub request_timeout: Duration,
    pub thinking: Thinking,
}

/// The two credential kinds carry different headers, and the wrong pairing is a
/// 401:
///
/// - `ApiKey` sends `x-api-key: <key>`.
/// - `BearerToken` sends `Authorization: Bearer <token>` plus
///   `anthropic-beta: oauth-2025-04-20`.
///
/// Both are [`SecretString`], so a `Debug` on [`ProviderConfig`] cannot print the
/// credential into a journal or a tracing span, which is how API keys usually
/// leak.
#[derive(Clone)]
pub enum Auth {
    ApiKey(SecretString),
    BearerToken(SecretString),
}

/// Reasoning effort, sent as `output_config: {effort: ...}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effort {
    High,
    Low,
    Max,
    Medium,
    /// Between `high` and `max`, and the recommended level for coding and
    /// agentic work.
    Xhigh,
}

/// Thinking configuration. `Adaptive` must be set explicitly; there is no
/// "default on".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Thinking {
    Adaptive { display: Display },
    Disabled,
}

/// Whether thinking blocks arrive with their text populated. `signature` arrives
/// either way and must be echoed back byte-identical.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Display {
    Omitted,
    Summarized,
}

// ========================================================================
// ProviderConfig Implementations
// ========================================================================

impl ProviderConfig {
    /// The defaults the plan argues for: `claude-opus-4-8`, 64k output, `xhigh`
    /// effort, adaptive thinking with the text omitted.
    pub fn new(auth: Auth) -> Self {
        Self {
            auth,
            base_url: Url::parse(DEFAULT_BASE_URL).expect("DEFAULT_BASE_URL parses"),
            effort: None,
            extra_headers: &[],
            max_tokens: DEFAULT_MAX_TOKENS,
            model: DEFAULT_MODEL.to_string(),
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            thinking: Thinking::Adaptive {
                display: Display::Omitted,
            },
        }
    }

    /// Read the credential from the environment: `ANTHROPIC_API_KEY` first, then
    /// `ANTHROPIC_AUTH_TOKEN`. The two take different headers, so which variable
    /// answered decides the [`Auth`] variant.
    pub fn from_env() -> Result<Self, AuthError> {
        Ok(Self::new(Auth::from_env()?))
    }

    /// Build a config from a registry profile and an already-resolved
    /// credential. `model` overrides the profile's default; `auth` is
    /// whatever source (a stored credential file, or the profile's own env
    /// vars) already produced — this function does no credential lookup of
    /// its own, so it stays usable for a profile with no env-var story at all.
    ///
    /// Fails only when the profile's base-URL override is set and does not
    /// parse: a typo there is the user's to fix, not a reason to panic.
    pub fn for_profile(
        profile: &ProviderProfile,
        model: Option<&str>,
        auth: Auth,
    ) -> Result<Self, BadBaseUrl> {
        Ok(Self::new(auth)
            .with_base_url(profile.resolved_base_url()?)
            .with_extra_headers(profile.extra_headers)
            .with_model(model.unwrap_or(profile.default_model)))
    }

    /// Render thinking blocks' text. The CLI flips this on for `--show-thinking`.
    pub fn with_display(mut self, display: Display) -> Self {
        if let Thinking::Adaptive { .. } = self.thinking {
            self.thinking = Thinking::Adaptive { display };
        }
        self
    }

    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    /// Point at a proxy or a test server.
    pub fn with_base_url(mut self, base_url: Url) -> Self {
        self.base_url = base_url;
        self
    }

    /// Set the profile-specific static headers every request carries in
    /// addition to the credential.
    pub fn with_extra_headers(mut self, headers: &'static [(&'static str, &'static str)]) -> Self {
        self.extra_headers = headers;
        self
    }

    /// Override the model id: a provider profile's default, or a `/model`
    /// selection.
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    /// Enable reasoning effort, sent as `output_config.effort`. Effort is opt-in
    /// (the default is off -- some models 400 on it), so a provider that wants
    /// it calls this; [`crate::provider::Provider::set_effort`] toggles it live.
    pub fn with_effort(mut self, effort: Effort) -> Self {
        self.effort = Some(effort);
        self
    }

    /// `{base_url}/v1/messages`.
    pub fn messages_url(&self) -> Url {
        self.join("v1/messages")
    }

    /// `{base_url}/v1/messages/count_tokens`.
    pub fn count_tokens_url(&self) -> Url {
        self.join("v1/messages/count_tokens")
    }

    fn join(&self, path: &str) -> Url {
        let mut url = self.base_url.clone();
        {
            let Ok(mut segments) = url.path_segments_mut() else {
                return self.base_url.clone();
            };
            segments.pop_if_empty();
            for segment in path.split('/') {
                segments.push(segment);
            }
        }
        url
    }
}

/// Deliberately hand-written, and the one impl in this crate that must not be
/// derived: a derived `Debug` would print the credential.
impl std::fmt::Debug for ProviderConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderConfig")
            .field("auth", &self.auth)
            .field("base_url", &self.base_url)
            .field("effort", &self.effort)
            .field("extra_headers", &self.extra_headers)
            .field("max_tokens", &self.max_tokens)
            .field("model", &self.model)
            .field("request_timeout", &self.request_timeout)
            .field("thinking", &self.thinking)
            .finish()
    }
}

// ========================================================================
// Auth Implementations
// ========================================================================

impl Auth {
    /// `ANTHROPIC_API_KEY`, then `ANTHROPIC_AUTH_TOKEN`. Failure names both.
    pub fn from_env() -> Result<Self, AuthError> {
        if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
            return Ok(Self::ApiKey(SecretString::new(key)));
        }
        if let Ok(token) = std::env::var("ANTHROPIC_AUTH_TOKEN") {
            return Ok(Self::BearerToken(SecretString::new(token)));
        }
        Err(AuthError::Missing)
    }
}

/// Names the variant and never the secret.
impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let variant = match self {
            Self::ApiKey(_) => "ApiKey",
            Self::BearerToken(_) => "BearerToken",
        };
        write!(f, "{variant}(<redacted>)")
    }
}

// ========================================================================
// Effort Implementations
// ========================================================================

impl Effort {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Low => "low",
            Self::Max => "max",
            Self::Medium => "medium",
            Self::Xhigh => "xhigh",
        }
    }

    /// Parse a level name (`low`/`medium`/`high`/`xhigh`/`max`), for `/effort`.
    /// Case-insensitive, so a typed `XHigh` or `MAX` reads the same as the
    /// canonical lowercase the API uses.
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text.trim().to_ascii_lowercase().as_str() {
            "low" => Self::Low,
            "medium" => Self::Medium,
            "high" => Self::High,
            "xhigh" => Self::Xhigh,
            "max" => Self::Max,
            _ => return None,
        })
    }
}

// ========================================================================
// Display Implementations
// ========================================================================

impl Display {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Omitted => "omitted",
            Self::Summarized => "summarized",
        }
    }
}

// ========================================================================
// Errors
// ========================================================================

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("no credential: set ANTHROPIC_API_KEY or ANTHROPIC_AUTH_TOKEN")]
    Missing,
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use secrecy::SecretString;

    use super::*;

    fn config() -> ProviderConfig {
        ProviderConfig::new(Auth::ApiKey(SecretString::new("sk-ant-supersecret".into())))
    }

    #[test]
    fn debug_never_prints_the_credential() {
        // The failure this catches is a leak, so assert on the secret itself
        // rather than on the shape of the redaction.
        let rendered = format!("{:?}", config());
        assert!(
            !rendered.contains("supersecret"),
            "credential leaked into Debug: {rendered}"
        );
        assert!(rendered.contains("<redacted>"));
    }

    #[test]
    fn defaults_are_the_api_facts() {
        let config = config();
        assert_eq!(config.model, DEFAULT_MODEL);
        assert_eq!(config.max_tokens, 64_000);
        assert_eq!(config.effort, None);
        assert_eq!(
            config.thinking,
            Thinking::Adaptive {
                display: Display::Omitted
            }
        );
    }

    #[test]
    fn urls_hang_off_the_base() {
        let config = config();
        assert_eq!(
            config.messages_url().as_str(),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            config.count_tokens_url().as_str(),
            "https://api.anthropic.com/v1/messages/count_tokens"
        );
    }

    #[test]
    fn a_proxy_base_url_keeps_its_path_prefix() {
        let config =
            config().with_base_url(Url::parse("https://proxy.internal/anthropic").unwrap());
        assert_eq!(
            config.messages_url().as_str(),
            "https://proxy.internal/anthropic/v1/messages"
        );
    }

    #[test]
    fn effort_parse_reads_each_level_case_insensitively() {
        // `/effort` accepts the canonical lowercase the API uses and the forms a
        // user is likely to type (XHigh, MAX), and rejects anything else so a
        // typo surfaces instead of silently landing on the default.
        assert_eq!(Effort::parse("low"), Some(Effort::Low));
        assert_eq!(Effort::parse("Medium"), Some(Effort::Medium));
        assert_eq!(Effort::parse("HIGH"), Some(Effort::High));
        assert_eq!(Effort::parse("xhigh"), Some(Effort::Xhigh));
        assert_eq!(Effort::parse(" max "), Some(Effort::Max));
        assert_eq!(Effort::parse("turbo"), None);
    }
}
