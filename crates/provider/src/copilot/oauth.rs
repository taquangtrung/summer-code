//! GitHub's OAuth device-code flow and the Copilot session-token exchange.
//!
//! Three calls, in order: [`start_device_flow`] gets a code to show the user,
//! [`poll_device_flow`] is called on a timer until GitHub reports the user
//! approved it (returning the long-lived GitHub access token), and
//! [`exchange_session_token`] trades that token for the short-lived
//! (~25-30 minute) Copilot session token every chat request actually needs —
//! called again whenever the cached one is close to expiring, never
//! persisted.
//!
//! The client id below is GitHub Copilot Chat's own registered OAuth app id
//! for VS Code, not one this crate was issued. Authenticating as it is the
//! same approach every community Copilot CLI takes to reach the API outside
//! GitHub's own approved clients; the caller this flow is wired behind spells
//! that trade-off out to whoever enables it.

use std::time::Duration;

use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;

// ========================================================================
// Constants
// ========================================================================

/// GitHub Copilot Chat's VS Code extension client id. Public: it identifies
/// the *application* to GitHub's OAuth server, not a user or a secret.
const CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";

const EDITOR_VERSION: &str = "vscode/1.95.0";
const EDITOR_PLUGIN_VERSION: &str = "copilot-chat/0.22.0";
const USER_AGENT: &str = "GitHubCopilotChat/0.22.0";
const INTEGRATION_ID: &str = "vscode-chat";

/// Headers every authenticated Copilot call carries beyond its credential:
/// the API 401s a request that does not identify as an editor integration.
/// Shared with [`crate::profile`]'s registry entry, so a chat request and the
/// session-token exchange that feeds it never drift apart.
pub(crate) const CHAT_HEADERS: &[(&str, &str)] = &[
    ("editor-version", EDITOR_VERSION),
    ("editor-plugin-version", EDITOR_PLUGIN_VERSION),
    ("user-agent", USER_AGENT),
    ("copilot-integration-id", INTEGRATION_ID),
    ("openai-intent", "conversation-panel"),
];

pub const DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
pub const ACCESS_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
pub const SESSION_TOKEN_URL: &str = "https://api.github.com/copilot_internal/v2/token";

// ========================================================================
// Data Structures
// ========================================================================

/// The code and URL to show the user, and how often to poll for it.
#[derive(Debug)]
pub struct DeviceCode {
    pub device_code: String,
    pub interval: Duration,
    pub user_code: String,
    pub verification_uri: String,
}

/// One poll's result. GitHub's own device-flow status codes, not exceptions:
/// `Pending` and `SlowDown` are the expected steady state while the user is
/// still in their browser, not failures.
///
/// `Debug`-derivable because `SecretString`'s own `Debug` already redacts —
/// deriving here does not print `github_token`.
#[derive(Debug)]
pub enum PollOutcome {
    Authorized { github_token: SecretString },
    Denied,
    Expired,
    Pending,
    SlowDown { interval: Option<Duration> },
}

/// The short-lived credential a chat request actually uses, plus the base URL
/// parsed out of it: GitHub routes different accounts to different proxy
/// hosts, and the token is the only place that host is named.
#[derive(Debug)]
pub struct SessionToken {
    pub base_url: reqwest::Url,
    pub expires_in: Duration,
    pub token: SecretString,
}

// ========================================================================
// Functions
// ========================================================================

/// Ask GitHub for a device code. Shown to the user as "go to
/// `verification_uri` and enter `user_code`".
pub async fn start_device_flow(
    client: &reqwest::Client,
    device_code_url: &str,
) -> Result<DeviceCode, CopilotAuthError> {
    #[derive(Deserialize)]
    struct Response {
        device_code: String,
        expires_in: u64,
        interval: Option<u64>,
        user_code: String,
        verification_uri: String,
    }
    let response = client
        .post(device_code_url)
        .header("accept", "application/json")
        .header("user-agent", USER_AGENT)
        .form(&[("client_id", CLIENT_ID), ("scope", "read:user")])
        .send()
        .await
        .map_err(CopilotAuthError::transport)?;
    let body: Response = json(response).await?;
    let _ = body.expires_in; // Surfaced reactively: GitHub reports `expired_token` on a poll rather than the caller tracking a countdown itself.
    Ok(DeviceCode {
        device_code: body.device_code,
        interval: Duration::from_secs(body.interval.unwrap_or(5)),
        user_code: body.user_code,
        verification_uri: body.verification_uri,
    })
}

/// One poll for whether the user has approved the device code yet.
pub async fn poll_device_flow(
    client: &reqwest::Client,
    access_token_url: &str,
    device_code: &str,
) -> Result<PollOutcome, CopilotAuthError> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Response {
        Success {
            access_token: String,
        },
        Error {
            error: String,
            interval: Option<u64>,
        },
    }
    let response = client
        .post(access_token_url)
        .header("accept", "application/json")
        .header("user-agent", USER_AGENT)
        .form(&[
            ("client_id", CLIENT_ID),
            ("device_code", device_code),
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
        ])
        .send()
        .await
        .map_err(CopilotAuthError::transport)?;
    let body: Response = json(response).await?;
    Ok(match body {
        Response::Success { access_token } => PollOutcome::Authorized {
            github_token: SecretString::new(access_token),
        },
        Response::Error { error, interval } => match error.as_str() {
            "authorization_pending" => PollOutcome::Pending,
            "slow_down" => PollOutcome::SlowDown {
                interval: interval.map(Duration::from_secs),
            },
            "expired_token" => PollOutcome::Expired,
            "access_denied" => PollOutcome::Denied,
            other => return Err(CopilotAuthError::DeviceFlow(other.to_string())),
        },
    })
}

/// Exchange the long-lived GitHub access token for a short-lived Copilot
/// session token, carrying the account's assigned proxy host.
pub async fn exchange_session_token(
    client: &reqwest::Client,
    session_token_url: &str,
    github_token: &SecretString,
) -> Result<SessionToken, CopilotAuthError> {
    #[derive(Deserialize)]
    struct Response {
        expires_at: u64,
        token: String,
    }
    let mut request = client
        .get(session_token_url)
        .header("accept", "application/json")
        .header(
            "authorization",
            format!("token {}", github_token.expose_secret()),
        );
    for (name, value) in CHAT_HEADERS {
        request = request.header(*name, *value);
    }
    let response = request.send().await.map_err(CopilotAuthError::transport)?;
    let body: Response = json(response).await?;
    let base_url = base_url_from_token(&body.token).ok_or(CopilotAuthError::MalformedToken)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    Ok(SessionToken {
        base_url,
        expires_in: Duration::from_secs(body.expires_at.saturating_sub(now)),
        token: SecretString::new(body.token),
    })
}

/// One model a GitHub Copilot subscription can reach, as reported by
/// `/models` after the entries Copilot's own picker would hide are filtered
/// out.
///
use serde::Serialize;

/// Carries enough to both label a row in `/model`'s picker and order rows by
/// cost: Copilot bills a subscription's premium-request quota at a per-model
/// `multiplier` rather than dollars per token, so the multiplier — not a
/// fabricated price — is what sorts the list cheapest-first. See the registry
/// comment in `profile` for why a Copilot model's `Price` is zero.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CopilotModel {
    /// The id passed back to Copilot's chat endpoint, e.g. `gpt-4o`.
    pub id: String,
    /// The human-friendly name the API returns, e.g. `GPT-4o`. `None` only
    /// when the API omits it; [`Self::display_name`] falls back to the id.
    pub name: Option<String>,
    /// Premium-request billing multiplier (1.0 = one request against the
    /// quota). `None` when the API omits `billing.multiplier`; such models
    /// sort after every model that does name one rather than being assigned a
    /// fabricated value.
    pub multiplier: Option<f64>,
    /// Maximum input context, in tokens, when the API reports it.
    pub max_context_window_tokens: Option<u64>,
    /// The reasoning-effort levels this model accepts, as
    /// `capabilities.supports.reasoning_effort` lists them (e.g.
    /// `["low","medium","high","max"]`). `None` means the model does not support
    /// reasoning effort at all -- sending one is a 400 -- so the picker offers
    /// no effort choice and none is sent.
    pub reasoning_effort: Option<Vec<String>>,
}

impl CopilotModel {
    /// The label a picker shows: the API's friendly name, or the id when the
    /// API did not send one — the same `name ?? id` fallback the editor
    /// integration uses.
    pub fn display_name(&self) -> &str {
        match &self.name {
            Some(name) => name,
            None => &self.id,
        }
    }
}

#[derive(Deserialize)]
struct CopilotModelsResponse {
    data: Vec<CopilotModelEntry>,
}

/// The raw `/models` entry, before filtering. Every field but `id` is
/// optional because Copilot's own responses omit them freely.
#[derive(Deserialize)]
struct CopilotModelEntry {
    id: String,
    name: Option<String>,
    policy: Option<CopilotPolicy>,
    billing: Option<CopilotBilling>,
    capabilities: Option<CopilotCapabilities>,
}

#[derive(Deserialize)]
struct CopilotPolicy {
    /// `"disabled"` revokes the model for this account; anything else (or
    /// absent) leaves it selectable.
    state: Option<String>,
}

#[derive(Deserialize)]
struct CopilotBilling {
    multiplier: Option<f64>,
}

#[derive(Deserialize)]
struct CopilotCapabilities {
    /// `"chat"` for completion models, `"embeddings"` for models that cannot
    /// answer a turn. The exhaustive signal for whether a model belongs in the
    /// picker: every entry carries it, and unlike `model_picker_enabled` it
    /// does not hide the chat models Copilot's own UI keeps off its list.
    #[serde(rename = "type")]
    kind: Option<String>,
    limits: Option<CopilotLimits>,
    supports: Option<CopilotSupports>,
}

#[derive(Deserialize)]
struct CopilotSupports {
    /// The reasoning-effort levels the model accepts
    /// (`["low","medium","high","max"]`). Absent means the model rejects
    /// `output_config.effort` entirely.
    reasoning_effort: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct CopilotLimits {
    max_context_window_tokens: Option<u64>,
}

/// Fetch the models a Copilot subscription can actually reach, straight from
/// the `/models` endpoint every Copilot-aware editor calls.
///
/// Offers every chat model the subscription can reach: anything whose
/// `capabilities.type` is not `"chat"` (embeddings, mostly) is dropped, as is
/// any model whose account `policy.state` is `"disabled"` (selecting it would
/// 403). This is intentionally wider than Copilot's own picker, which also
/// hides `model_picker_enabled: false` chat models -- legacy `gpt-4o`/`gpt-4.1`
/// variants, the exec/search agents, `auto-model` -- that this account can
/// still drive. Survivors are sorted cheapest (lowest premium-request
/// `multiplier`) first; models the API does not attach a multiplier to sort
/// after every model that has one, rather than being silently assigned a
/// fabricated one.
pub async fn fetch_models(
    client: &reqwest::Client,
    session_token_url: &str,
    github_token: &SecretString,
) -> Result<Vec<CopilotModel>, CopilotAuthError> {
    let session = exchange_session_token(client, session_token_url, github_token).await?;
    let models_url = session
        .base_url
        .join("models")
        .map_err(|_| CopilotAuthError::MalformedToken)?;

    let mut request = client
        .get(models_url)
        .header("accept", "application/json")
        .header(
            "authorization",
            format!("Bearer {}", session.token.expose_secret()),
        );
    for (name, value) in CHAT_HEADERS {
        request = request.header(*name, *value);
    }
    let response = request.send().await.map_err(CopilotAuthError::transport)?;
    let body: CopilotModelsResponse = json(response).await?;
    Ok(select_picker_models(body.data))
}

/// The pure half of [`fetch_models`]: turn the raw `/models` entries into the
/// filtered, sorted list the picker shows. Split out so the filter/sort/map
/// logic is testable without a socket — request-shape bugs are covered by the
/// HTTP test, but "which models survive, and in what order" is logic that
/// lives entirely in this transform.
fn select_picker_models(entries: Vec<CopilotModelEntry>) -> Vec<CopilotModel> {
    let mut models: Vec<CopilotModel> = entries
        .into_iter()
        // Chat models only. `capabilities.type` is present on every entry and
        // is the exhaustive signal; `model_picker_enabled` is not, because it
        // hides ~26 chat models Copilot keeps off its own picker (legacy
        // gpt-4o/gpt-4.1/gpt-3.5, the exec/search agents, auto-model) that this
        // account can still reach.
        .filter(|entry| entry.capabilities.as_ref().and_then(|c| c.kind.as_deref()) == Some("chat"))
        // An account-level `disabled` policy revokes the model; selecting it
        // would 403, so it is dropped even though it is a chat model.
        .filter(|entry| entry.policy.as_ref().and_then(|p| p.state.as_deref()) != Some("disabled"))
        .map(|entry| CopilotModel {
            multiplier: entry.billing.as_ref().and_then(|b| b.multiplier),
            max_context_window_tokens: entry
                .capabilities
                .as_ref()
                .and_then(|c| c.limits.as_ref())
                .and_then(|l| l.max_context_window_tokens),
            reasoning_effort: entry
                .capabilities
                .as_ref()
                .and_then(|c| c.supports.as_ref())
                .and_then(|s| s.reasoning_effort.clone()),
            id: entry.id,
            name: entry.name,
        })
        .collect();
    models.sort_by(|a, b| {
        a.multiplier
            .unwrap_or(f64::INFINITY)
            .partial_cmp(&b.multiplier.unwrap_or(f64::INFINITY))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    models
}

/// Parse `proxy-ep=<host>` out of the token's semicolon-delimited claims and
/// turn it into `https://api.<host, "proxy." stripped>`: the token is the
/// only place a Copilot account's assigned host is named, and there is no
/// separate lookup for it.
fn base_url_from_token(token: &str) -> Option<reqwest::Url> {
    let proxy_ep = token
        .split(';')
        .find_map(|field| field.strip_prefix("proxy-ep="))?;
    let host = proxy_ep.strip_prefix("proxy.").unwrap_or(proxy_ep);
    reqwest::Url::parse(&format!("https://api.{host}")).ok()
}

/// Read a JSON body, classifying a non-2xx status or an unparseable body as
/// distinct error shapes rather than one generic failure — the device-flow
/// polling loop needs to tell "GitHub is unreachable" apart from "GitHub
/// answered and it was an HTTP error", since only the latter should stop
/// polling immediately.
async fn json<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
) -> Result<T, CopilotAuthError> {
    let status = response.status();
    let text = response.text().await.map_err(CopilotAuthError::transport)?;
    if !status.is_success() {
        return Err(CopilotAuthError::Http {
            body: text,
            status: status.as_u16(),
        });
    }
    serde_json::from_str(&text)
        .map_err(|error| CopilotAuthError::MalformedResponse(format!("{error}: {text}")))
}

// ========================================================================
// Errors
// ========================================================================

#[derive(Debug, thiserror::Error)]
pub enum CopilotAuthError {
    #[error("GitHub device flow: {0}")]
    DeviceFlow(String),
    #[error("GitHub returned HTTP {status}: {body}")]
    Http { body: String, status: u16 },
    #[error("malformed response: {0}")]
    MalformedResponse(String),
    #[error("Copilot session token had no usable proxy endpoint")]
    MalformedToken,
    #[error("network error talking to GitHub: {0}")]
    Transport(String),
}

impl CopilotAuthError {
    fn transport(error: reqwest::Error) -> Self {
        Self::Transport(error.to_string())
    }
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_from_token_strips_the_proxy_prefix() {
        let token = "tid=abc;exp=123;proxy-ep=proxy.individual.githubcopilot.com;foo=bar";
        let url = base_url_from_token(token).expect("proxy-ep is present");
        assert_eq!(url.as_str(), "https://api.individual.githubcopilot.com/");
    }

    #[test]
    fn base_url_from_token_is_none_without_a_proxy_ep_claim() {
        // The bug this catches: silently falling back to some default host
        // would send a valid-looking request to the wrong account's proxy,
        // which fails as an opaque 401/403 far from this parsing step.
        assert!(base_url_from_token("tid=abc;exp=123").is_none());
    }

    #[test]
    fn select_picker_models_keeps_only_chat_drops_disabled_and_sorts_cheapest_first() {
        // `capabilities.type` is the line: "chat" models are offered,
        // "embeddings" and anything untyped are not, and an account-disabled
        // chat model is still dropped because selecting it would 403. The
        // survivors sort cheapest-first; a model with no billing multiplier
        // sorts after every one that has a multiplier, not dropped or invented.
        let body: CopilotModelsResponse = serde_json::from_str(
            r#"{
                "data": [
                    {"id": "expensive", "capabilities": {"type": "chat"}, "billing": {"multiplier": 3.0}},
                    {"id": "cheap", "capabilities": {"type": "chat"}, "billing": {"multiplier": 0.25}},
                    {"id": "embedding", "capabilities": {"type": "embeddings"}},
                    {"id": "revoked", "capabilities": {"type": "chat"}, "policy": {"state": "disabled"}},
                    {"id": "untyped", "capabilities": {}},
                    {"id": "unknown-cost", "capabilities": {"type": "chat", "limits": {"max_context_window_tokens": 200000}}}
                ]
            }"#,
        )
        .expect("fixture parses");
        let models = select_picker_models(body.data);
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["cheap", "expensive", "unknown-cost"]);
        assert!(models[2].multiplier.is_none());
        assert_eq!(models[2].max_context_window_tokens, Some(200_000));
    }

    #[test]
    fn select_picker_models_keeps_a_chat_model_with_only_an_id() {
        // A chat model the API sent no name or billing for still appears,
        // falling back to its id for the label -- the API omits both freely.
        let body: CopilotModelsResponse =
            serde_json::from_str(r#"{"data":[{"id":"only-id","capabilities":{"type":"chat"}}]}"#)
                .expect("fixture parses");
        let models = select_picker_models(body.data);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "only-id");
        assert_eq!(models[0].display_name(), "only-id");
        assert!(models[0].multiplier.is_none());
    }

    #[test]
    fn copilot_model_display_name_falls_back_to_the_id() {
        let named = CopilotModel {
            id: "gpt-4o".into(),
            name: Some("GPT-4o".into()),
            multiplier: None,
            max_context_window_tokens: None,
            reasoning_effort: None,
        };
        assert_eq!(named.display_name(), "GPT-4o");
        let unnamed = CopilotModel {
            id: "gpt-4o".into(),
            name: None,
            multiplier: None,
            max_context_window_tokens: None,
            reasoning_effort: None,
        };
        assert_eq!(unnamed.display_name(), "gpt-4o");
    }
}
