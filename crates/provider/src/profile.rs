//! Built-in provider profiles: which host and models a named provider maps
//! to, resolved into a [`ProviderConfig`](crate::config::ProviderConfig)
//! once a credential exists.
//!
//! A profile's env vars are distinct from Anthropic's own `ANTHROPIC_API_KEY`/
//! `ANTHROPIC_AUTH_TOKEN` wherever a provider's own compatibility layer reuses
//! those exact names (Z.ai's does, since it is meant as an Anthropic drop-in
//! for tools that only know those two variables): a registry holding both
//! providers active at once cannot tell which provider a shared name was meant
//! for, so each profile here gets names of its own.

use reqwest::Url;
use secrecy::SecretString;

use crate::anthropic::AnthropicProvider;
use crate::config::{Auth, DEFAULT_BASE_URL, DEFAULT_MODEL, ProviderConfig};
use crate::copilot::{CHAT_HEADERS, CopilotProvider};
use crate::openai::OpenAiProvider;
use crate::provider::Provider;

// ========================================================================
// Constants
// ========================================================================

const MODEL_GLM_4_5: &str = "glm-4.5";
const MODEL_GLM_4_5_AIR: &str = "glm-4.5-air";
const MODEL_GLM_4_6: &str = "glm-4.6";
const MODEL_GLM_4_7: &str = "glm-4.7";
const MODEL_GLM_5: &str = "glm-5";
const MODEL_GLM_5_TURBO: &str = "glm-5-turbo";
const MODEL_GLM_5_1: &str = "glm-5.1";
const MODEL_GLM_5_2: &str = "glm-5.2";
const MODEL_GLM_5_3: &str = "glm-5.3";

const ZAI_BASE_URL: &str = "https://api.z.ai/api/anthropic";

/// OpenAI's chat-completions root. The trailing slash matters: `Url::join`
/// replaces the last path segment without one, so `.../v1` would become
/// `.../chat/completions` and 404.
const OPENAI_BASE_URL: &str = "https://api.openai.com/v1/";

/// Google's OpenAI-compatibility endpoint, which is why Gemini needs no second
/// transport. See [`crate::openai`] for the reasoning and its limits.
const GEMINI_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta/openai/";

/// Where `llama-server` listens when started with no `--port`. vLLM's default
/// is 8000, which `SUMMER_LOCAL_BASE_URL` covers; one default had to win, and
/// llama.cpp is what a laptop runs.
const LOCAL_BASE_URL: &str = "http://127.0.0.1:8080/v1/";

/// Points the `local` profile at another OpenAI-compatible server.
///
/// An environment variable rather than a config key because the CLI, the TUI's
/// model picker and the bench all build their config through
/// [`ProviderConfig::for_profile`], and the bench reads no config file.
const LOCAL_BASE_URL_ENV: &str = "SUMMER_LOCAL_BASE_URL";

/// For a local server started with `--api-key`. Most are not.
const LOCAL_API_KEY_ENV: &str = "SUMMER_LOCAL_API_KEY";

/// What a local server is sent when no key is set. `llama-server` and vLLM
/// ignore the header unless they were started with a key of their own.
const LOCAL_NO_KEY: &str = "no-key";

/// The model id sent when none is chosen. A single-model server answers with
/// whatever it loaded, whatever this says.
const MODEL_LOCAL: &str = "local";

const MODEL_GPT_5: &str = "gpt-5";
const MODEL_GPT_5_MINI: &str = "gpt-5-mini";
const MODEL_GEMINI_PRO: &str = "gemini-2.5-pro";
const MODEL_GEMINI_FLASH: &str = "gemini-2.5-flash";

/// Not `ANTHROPIC_AUTH_TOKEN`: see the module doc on why a shared-provider
/// registry needs names Anthropic's own credential lookup does not also read.
const ZAI_API_KEY_ENV: &str = "SUMMER_ZAI_API_KEY";

/// The China-mainland GLM Coding Plan account: same product, a different
/// domain and API key than the international one above, confirmed against
/// Z.ai's own bigmodel.cn docs rather than assumed from the international
/// path.
const ZAI_CN_BASE_URL: &str = "https://open.bigmodel.cn/api/anthropic";

const ZAI_CN_API_KEY_ENV: &str = "SUMMER_ZAI_CN_API_KEY";

/// The still-active Claude ids a user may pin besides [`DEFAULT_MODEL`].
///
/// Kept as a short list of ids confirmed against Anthropic's own model table
/// rather than a full catalog: an id guessed here is the 404 this module's
/// header warns about, and `/models` discovery is the only way to be exhaustive
/// without guessing.
const MODEL_CLAUDE_OPUS_4_8: &str = "claude-opus-4-8";
const MODEL_CLAUDE_SONNET_5: &str = "claude-sonnet-5";
const MODEL_CLAUDE_HAIKU_4_5: &str = "claude-haiku-4-5";

/// Not a real endpoint: [`ProviderProfile::port`] only needs a host to parse a
/// port out of, and every GitHub Copilot host is 443. The real base URL is
/// account-specific and only known once the OAuth exchange in
/// [`crate::copilot`] returns a session token — see its `proxy-ep` parsing.
const GITHUB_COPILOT_BASE_URL: &str = "https://api.githubcopilot.com";

/// A raw GitHub OAuth access token, for anyone who already has one (from
/// `gh auth token` or another Copilot-enabled tool) and would rather set it
/// than run `/login github-copilot`'s device flow. Either path ends up
/// storing the same kind of value: [`crate::copilot`] exchanges it for a
/// short-lived Copilot session token before every use, the same as it does
/// for a device-flow token.
const GITHUB_COPILOT_TOKEN_ENV: &str = "SUMMER_GITHUB_COPILOT_TOKEN";

// ========================================================================
// Data Structures
// ========================================================================

/// A named provider: its host, model catalog, and where to find a credential.
#[derive(Debug, Clone, Copy)]
pub struct ProviderProfile {
    pub base_url: &'static str,
    /// A variable that replaces `base_url` when set. `None` for every hosted
    /// provider: their host is a fact about the provider, and a variable that
    /// could move it would send their credential somewhere else.
    pub base_url_env: Option<&'static str>,
    pub default_model: &'static str,
    pub env_vars: &'static [EnvCredential],
    /// Static headers this profile's compatibility layer requires beyond the
    /// credential itself. Empty for every profile except GitHub Copilot.
    pub extra_headers: &'static [(&'static str, &'static str)],
    pub id: ProviderId,
    /// The token to send when no credential is configured, for a server that
    /// checks none. `None` means a credential is required.
    pub keyless_token: Option<&'static str>,
    pub label: &'static str,
    pub models: &'static [ModelInfo],
    /// Whether `/login <id>` with no token starts an interactive OAuth
    /// device-code flow instead of showing the usage error. A stored or
    /// env-var token still works normally when one is supplied — this only
    /// changes what happens when one is not.
    pub oauth_device_flow: bool,
}

/// One selectable model in a provider's catalog.
#[derive(Debug, Clone, Copy)]
pub struct ModelInfo {
    pub name: &'static str,
    /// The model's context window, in tokens. A static best-known value the
    /// statusline uses to render context usage; Copilot's live `/models`
    /// endpoint carries a per-account value that could refine this but is not
    /// yet plumbed through.
    pub context_window: u64,
}

/// An environment variable a profile's credential may arrive on, and which
/// [`Auth`] variant it becomes. Checked in order, first-set wins, the same
/// shape [`Auth::from_env`](crate::config::Auth::from_env) already uses for
/// Anthropic's own two variables.
#[derive(Debug, Clone, Copy)]
pub struct EnvCredential {
    pub kind: AuthKind,
    pub name: &'static str,
}

/// Which [`Auth`] variant a resolved credential becomes; the two take
/// different headers, so losing this at the point a secret is read is a 401
/// later with no context left to explain it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthKind {
    ApiKey,
    BearerToken,
}

/// A provider's identity, stable across the registry, config files, and CLI
/// flags — the `as_str`/`FromStr` pair is the one place its string form is
/// spelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderId {
    Anthropic,
    Gemini,
    GithubCopilot,
    /// Any OpenAI-compatible server the user runs: `llama-server`, vLLM,
    /// Ollama, LM Studio.
    Local,
    OpenAi,
    ZAi,
    ZAiCn,
}

// ========================================================================
// Registry
// ========================================================================

const ANTHROPIC_ENV_VARS: &[EnvCredential] = &[
    EnvCredential {
        kind: AuthKind::ApiKey,
        name: "ANTHROPIC_API_KEY",
    },
    EnvCredential {
        kind: AuthKind::BearerToken,
        name: "ANTHROPIC_AUTH_TOKEN",
    },
];

const ZAI_ENV_VARS: &[EnvCredential] = &[EnvCredential {
    kind: AuthKind::BearerToken,
    name: ZAI_API_KEY_ENV,
}];

const ZAI_CN_ENV_VARS: &[EnvCredential] = &[EnvCredential {
    kind: AuthKind::BearerToken,
    name: ZAI_CN_API_KEY_ENV,
}];

const OPENAI_ENV_VARS: &[EnvCredential] = &[EnvCredential {
    kind: AuthKind::ApiKey,
    name: "OPENAI_API_KEY",
}];

const GEMINI_ENV_VARS: &[EnvCredential] = &[
    EnvCredential {
        kind: AuthKind::ApiKey,
        name: "GEMINI_API_KEY",
    },
    EnvCredential {
        kind: AuthKind::ApiKey,
        name: "GOOGLE_API_KEY",
    },
];

const OPENAI_MODELS: &[ModelInfo] = &[
    ModelInfo {
        name: MODEL_GPT_5,
        context_window: 400_000,
    },
    ModelInfo {
        name: MODEL_GPT_5_MINI,
        context_window: 400_000,
    },
];

const LOCAL_ENV_VARS: &[EnvCredential] = &[EnvCredential {
    kind: AuthKind::BearerToken,
    name: LOCAL_API_KEY_ENV,
}];

const GEMINI_MODELS: &[ModelInfo] = &[
    ModelInfo {
        name: MODEL_GEMINI_PRO,
        context_window: 1_048_576,
    },
    ModelInfo {
        name: MODEL_GEMINI_FLASH,
        context_window: 1_048_576,
    },
];

const GITHUB_COPILOT_ENV_VARS: &[EnvCredential] = &[EnvCredential {
    kind: AuthKind::BearerToken,
    name: GITHUB_COPILOT_TOKEN_ENV,
}];

/// The Claude catalog, newest tier first, with the context window each model
/// actually serves.
///
/// **The windows are 1M, not 200K, and the old 200K pin was a live bug rather
/// than a stale nicety.** `context_window_for` feeds the statusline's `X% ctx`
/// gauge and the compaction budget, so pinning a 1M model at 200K reported a
/// session as five times fuller than it was, and invited a compaction the window
/// did not need. Only Haiku is genuinely a 200K model.
const ANTHROPIC_MODELS: &[ModelInfo] = &[
    ModelInfo {
        name: DEFAULT_MODEL,
        context_window: 1_000_000,
    },
    ModelInfo {
        name: MODEL_CLAUDE_OPUS_4_8,
        context_window: 1_000_000,
    },
    ModelInfo {
        name: MODEL_CLAUDE_SONNET_5,
        context_window: 1_000_000,
    },
    ModelInfo {
        name: MODEL_CLAUDE_HAIKU_4_5,
        context_window: 200_000,
    },
];

/// The GLM catalog as Z.ai's own `/v1/models` served it (queried live, since
/// the static list had already fallen behind the account's catalog), with
/// context windows from the per-model pages at docs.z.ai. GLM-4.6's window
/// was expanded from 128K to 200K after launch, so the old 128K pin was wrong
/// even for a model the list already had. GLM-5.3 and 5.2 are 1M-window
/// models; 5.3 takes text-only input, which costs this harness nothing (it
/// sends no images) and is why it can also be the default.
const ZAI_MODELS: &[ModelInfo] = &[
    ModelInfo {
        name: MODEL_GLM_5_3,
        context_window: 1_000_000,
    },
    ModelInfo {
        name: MODEL_GLM_5_2,
        context_window: 1_000_000,
    },
    ModelInfo {
        name: MODEL_GLM_5_1,
        context_window: 200_000,
    },
    ModelInfo {
        name: MODEL_GLM_5_TURBO,
        context_window: 200_000,
    },
    ModelInfo {
        name: MODEL_GLM_5,
        context_window: 200_000,
    },
    ModelInfo {
        name: MODEL_GLM_4_7,
        context_window: 200_000,
    },
    ModelInfo {
        name: MODEL_GLM_4_6,
        context_window: 200_000,
    },
    ModelInfo {
        name: MODEL_GLM_4_5,
        context_window: 128_000,
    },
    ModelInfo {
        name: MODEL_GLM_4_5_AIR,
        context_window: 128_000,
    },
];

/// A curated subset of the models a GitHub Copilot subscription can reach,
/// not the full account-specific catalog Copilot's `/models` endpoint would
/// list (fetching that live is future work, not attempted here).
///
/// **Every entry here was a retired id, and the first one was the default.**
/// The table pinned `claude-3-5-sonnet` (retired 2025-10), `claude-3-opus`
/// (retired 2026-01), `gpt-4o`, `gemini-1.5-pro`, and the `o1` previews, so
/// `/login github-copilot` followed by a single turn 404'd on a model nobody
/// had chosen. A stale catalog entry costs a user one model switch; a stale
/// *default* costs them the belief that the provider is broken.
///
/// The ids are the upstream ones, which is how Copilot names them, and they
/// reuse the constants the direct Anthropic, OpenAI, and Gemini profiles
/// already pin: one list to keep current instead of two that drift apart.
/// Because the real catalog is account-specific, treat this as a starting set
/// and not an authority -- `/models` discovery is the actual fix.
const GITHUB_COPILOT_MODELS: &[ModelInfo] = &[
    ModelInfo {
        name: MODEL_CLAUDE_SONNET_5,
        context_window: 1_000_000,
    },
    ModelInfo {
        name: DEFAULT_MODEL,
        context_window: 1_000_000,
    },
    ModelInfo {
        name: MODEL_GPT_5,
        context_window: 400_000,
    },
    ModelInfo {
        name: MODEL_GPT_5_MINI,
        context_window: 400_000,
    },
    ModelInfo {
        name: MODEL_GEMINI_PRO,
        context_window: 1_048_576,
    },
];

/// The built-in profiles. Anthropic's catalog is deliberately just the one
/// pinned model id this crate already treats as an API fact
/// ([`DEFAULT_MODEL`]) — guessing at further Claude model ids for this table
/// risks the exact 404 the rest of this crate's docs warn against, so more
/// entries wait until their ids are confirmed rather than assumed.
pub const PROFILES: &[ProviderProfile] = &[
    ProviderProfile {
        base_url: DEFAULT_BASE_URL,
        base_url_env: None,
        default_model: DEFAULT_MODEL,
        env_vars: ANTHROPIC_ENV_VARS,
        extra_headers: &[],
        id: ProviderId::Anthropic,
        keyless_token: None,
        label: "Anthropic",
        models: ANTHROPIC_MODELS,
        oauth_device_flow: false,
    },
    ProviderProfile {
        base_url: GITHUB_COPILOT_BASE_URL,
        base_url_env: None,
        default_model: MODEL_CLAUDE_SONNET_5,
        env_vars: GITHUB_COPILOT_ENV_VARS,
        extra_headers: CHAT_HEADERS,
        id: ProviderId::GithubCopilot,
        keyless_token: None,
        label: "GitHub Copilot",
        models: GITHUB_COPILOT_MODELS,
        // `/login github-copilot` with no token runs the OAuth device flow
        // in `crate::copilot` instead of expecting a pasted secret.
        oauth_device_flow: true,
    },
    ProviderProfile {
        base_url: OPENAI_BASE_URL,
        base_url_env: None,
        default_model: MODEL_GPT_5,
        env_vars: OPENAI_ENV_VARS,
        extra_headers: &[],
        id: ProviderId::OpenAi,
        keyless_token: None,
        label: "OpenAI",
        models: OPENAI_MODELS,
        oauth_device_flow: false,
    },
    ProviderProfile {
        base_url: GEMINI_BASE_URL,
        base_url_env: None,
        default_model: MODEL_GEMINI_PRO,
        env_vars: GEMINI_ENV_VARS,
        extra_headers: &[],
        id: ProviderId::Gemini,
        keyless_token: None,
        label: "Google Gemini",
        models: GEMINI_MODELS,
        oauth_device_flow: false,
    },
    ProviderProfile {
        base_url: ZAI_BASE_URL,
        base_url_env: None,
        // The newest flagship at catalog-refresh time; see `ZAI_MODELS` for
        // where the ids and windows came from.
        default_model: MODEL_GLM_5_3,
        env_vars: ZAI_ENV_VARS,
        extra_headers: &[],
        id: ProviderId::ZAi,
        keyless_token: None,
        label: "Z.ai",
        models: ZAI_MODELS,
        oauth_device_flow: false,
    },
    ProviderProfile {
        base_url: ZAI_CN_BASE_URL,
        base_url_env: None,
        default_model: MODEL_GLM_5_3,
        env_vars: ZAI_CN_ENV_VARS,
        extra_headers: &[],
        id: ProviderId::ZAiCn,
        keyless_token: None,
        label: "Z.ai (China)",
        // Same GLM catalog as the international account: bigmodel.cn serves
        // the same models under the same Coding Plan, just to a separate
        // region-specific account and API key.
        models: ZAI_MODELS,
        oauth_device_flow: false,
    },
    ProviderProfile {
        base_url: LOCAL_BASE_URL,
        base_url_env: Some(LOCAL_BASE_URL_ENV),
        default_model: MODEL_LOCAL,
        env_vars: LOCAL_ENV_VARS,
        extra_headers: &[],
        id: ProviderId::Local,
        keyless_token: Some(LOCAL_NO_KEY),
        label: "Local (OpenAI-compatible)",
        // No catalog: the server decides what it serves, and a guessed list
        // would offer models it does not have.
        models: &[],
        oauth_device_flow: false,
    },
];

// ========================================================================
// ProviderProfile Implementations
// ========================================================================

impl ProviderProfile {
    /// The port the sandbox must carve out to let this provider's traffic
    /// through, if `base_url` parses and names one.
    pub fn port(&self) -> Option<u16> {
        self.resolved_base_url().ok()?.port_or_known_default()
    }

    /// The URL requests go to: `base_url`, or the override variable when this
    /// profile has one and it is set.
    pub fn resolved_base_url(&self) -> Result<Url, BadBaseUrl> {
        self.base_url_from(|name| std::env::var(name).ok())
    }

    /// The same resolution against an arbitrary source of variables, for the
    /// reason [`Self::credential_from`] takes one.
    ///
    /// **A trailing slash is added when missing**, because `Url::join`
    /// replaces the last segment of a path without one: `http://host/v1`
    /// joined with `chat/completions` is `http://host/chat/completions`, a 404
    /// that reads as a broken server rather than a missing character.
    pub fn base_url_from(
        &self,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<Url, BadBaseUrl> {
        let Some((variable, value)) = self
            .base_url_env
            .and_then(|name| lookup(name).map(|value| (name, value)))
        else {
            return Ok(Url::parse(self.base_url).expect("registry base_url parses"));
        };
        let mut url = Url::parse(value.trim()).map_err(|error| BadBaseUrl {
            message: error.to_string(),
            value: value.clone(),
            variable,
        })?;
        if !url.path().ends_with('/') {
            url.set_path(&format!("{}/", url.path()));
        }
        Ok(url)
    }

    /// The first configured env var's value, wrapped as the [`Auth`] variant
    /// that var carries. `None` means nothing here has a credential yet, not
    /// that the provider is unreachable — a stored credential in
    /// `~/.summer/auth.json` is checked separately, ahead of this.
    pub fn credential_from_env(&self) -> Option<Auth> {
        self.credential_from(|name| std::env::var(name).ok())
    }

    /// The same lookup against an arbitrary source of variables.
    ///
    /// Split out so a test can supply its own map: mutating the process
    /// environment races every other thread's `getenv`, which is why
    /// `std::env::set_var` is `unsafe` under the 2024 edition.
    pub fn credential_from(&self, lookup: impl Fn(&str) -> Option<String>) -> Option<Auth> {
        self.env_vars
            .iter()
            .find_map(|var| lookup(var.name).map(|value| var.kind.wrap(SecretString::new(value))))
            .or_else(|| {
                self.keyless_token
                    .map(|token| Auth::BearerToken(SecretString::new(token.to_string())))
            })
    }
}

/// The context window (in tokens) a model id maps to in the static registry,
/// or `None` for an id no profile catalogs (e.g. a live Copilot model the
/// registry does not pin). `None` rather than a guess, so the statusline can
/// degrade to showing usage without a percentage or window size.
pub fn context_window_for(model: &str) -> Option<u64> {
    PROFILES
        .iter()
        .flat_map(|profile| profile.models.iter())
        .find(|entry| entry.name == model)
        .map(|entry| entry.context_window)
}

// ========================================================================
// AuthKind Implementations
// ========================================================================

impl AuthKind {
    pub fn wrap(self, secret: SecretString) -> Auth {
        match self {
            Self::ApiKey => Auth::ApiKey(secret),
            Self::BearerToken => Auth::BearerToken(secret),
        }
    }
}

// ========================================================================
// ProviderId Implementations
// ========================================================================

impl ProviderId {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::Gemini => "gemini",
            Self::GithubCopilot => "github-copilot",
            Self::Local => "local",
            Self::OpenAi => "openai",
            Self::ZAi => "zai",
            Self::ZAiCn => "zai-cn",
        }
    }

    /// The registry entry for this id. `PROFILES` always has one per variant,
    /// so an absent entry is this file's own inconsistency, not caller error.
    pub fn profile(self) -> &'static ProviderProfile {
        PROFILES
            .iter()
            .find(|profile| profile.id == self)
            .expect("PROFILES has an entry for every ProviderId")
    }
}

impl std::str::FromStr for ProviderId {
    type Err = UnknownProviderId;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "anthropic" => Ok(Self::Anthropic),
            "gemini" => Ok(Self::Gemini),
            "github-copilot" => Ok(Self::GithubCopilot),
            "local" => Ok(Self::Local),
            "openai" => Ok(Self::OpenAi),
            "zai" => Ok(Self::ZAi),
            "zai-cn" => Ok(Self::ZAiCn),
            other => Err(UnknownProviderId(other.to_string())),
        }
    }
}

// ========================================================================
// Functions
// ========================================================================

/// The client that speaks to `provider`, built from an already-resolved config.
///
/// **One place, because there are now four callers and the mapping is not
/// obvious.** Every profile but two rides the Anthropic Messages API, which is
/// what makes the registry a table of profiles rather than a set of `Provider`
/// impls: Z.ai and its mainland twin differ from Anthropic only by base URL.
/// The exceptions are the chat-completions pair, where Gemini reaches OpenAI's
/// shape through Google's compatibility endpoint, and Copilot, whose credential
/// is a GitHub token that means nothing until it is exchanged.
///
/// Copied into each frontend instead, the mapping drifts: a profile added here
/// keeps working in the CLI and silently falls back to the wrong transport in
/// the bench, which is a wire-format error blamed on the model.
pub fn transport(provider: ProviderId, config: ProviderConfig) -> Box<dyn Provider> {
    match provider {
        ProviderId::GithubCopilot => {
            let Auth::BearerToken(token) = config.auth else {
                unreachable!("Copilot's one credential is BearerToken-kind, so its config is too");
            };
            Box::new(CopilotProvider::new(token, Some(&config.model)))
        }
        ProviderId::Gemini | ProviderId::Local | ProviderId::OpenAi => {
            Box::new(OpenAiProvider::new(config))
        }
        ProviderId::Anthropic | ProviderId::ZAi | ProviderId::ZAiCn => {
            Box::new(AnthropicProvider::new(config))
        }
    }
}

// ========================================================================
// Errors
// ========================================================================

#[derive(Debug, thiserror::Error)]
#[error(
    "unknown provider {0:?}; choices are \"anthropic\", \"gemini\", \"github-copilot\", \
     \"local\", \"openai\", \"zai\", \"zai-cn\""
)]
pub struct UnknownProviderId(String);

/// An override base URL that does not parse.
#[derive(Debug, thiserror::Error)]
#[error("{variable}={value:?} is not a URL: {message}")]
pub struct BadBaseUrl {
    message: String,
    value: String,
    variable: &'static str,
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in for the process environment, holding only what a test sets.
    fn env(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let vars: Vec<(String, String)> = vars
            .iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
            .collect();
        move |name| {
            vars.iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        }
    }

    #[test]
    fn every_provider_id_round_trips_through_its_string_form() {
        for profile in PROFILES {
            let parsed: ProviderId = profile.id.as_str().parse().expect("registry id parses");
            assert_eq!(parsed, profile.id);
        }
    }

    #[test]
    fn an_unknown_provider_name_is_an_error_not_a_silent_default() {
        // The bug this catches: a typo'd --provider value quietly falling
        // back to Anthropic instead of telling the user what they typed
        // was not recognized.
        //
        // The example used to be "openai", which is now a real provider. That
        // is the hazard with naming a plausible one here, so this uses a
        // near-miss of a supported id instead: a typo is what a user actually
        // produces, and it must not resolve.
        assert!("open-ai".parse::<ProviderId>().is_err());
        assert!("gemini-pro".parse::<ProviderId>().is_err());
    }

    #[test]
    fn every_profile_id_looks_itself_up() {
        for profile in PROFILES {
            assert_eq!(profile.id.profile().id, profile.id);
        }
    }

    #[test]
    fn zai_credential_from_env_reads_its_own_variable_not_anthropics() {
        // The bug this catches: reusing ANTHROPIC_AUTH_TOKEN for Z.ai would
        // make a session that only configured Anthropic silently "have" a
        // Z.ai credential too.
        let anthropic_only = env(&[("ANTHROPIC_AUTH_TOKEN", "anthropic-token")]);
        assert!(
            ProviderId::ZAi
                .profile()
                .credential_from(anthropic_only)
                .is_none(),
            "an Anthropic token must not satisfy the Z.ai profile"
        );

        let zai = env(&[("SUMMER_ZAI_API_KEY", "test-token")]);
        assert!(ProviderId::ZAi.profile().credential_from(zai).is_some());
    }

    #[test]
    fn zai_cn_credential_from_env_reads_its_own_variable_not_the_international_ones() {
        // The bug this catches: a China-account key configured on
        // SUMMER_ZAI_CN_API_KEY should not be readable as (or shadowed by) the
        // international SUMMER_ZAI_API_KEY, and vice versa: they are different
        // accounts on different domains.
        assert!(
            ProviderId::ZAiCn
                .profile()
                .credential_from(env(&[]))
                .is_none()
        );

        let international = env(&[("SUMMER_ZAI_API_KEY", "international-token")]);
        assert!(
            ProviderId::ZAiCn
                .profile()
                .credential_from(international)
                .is_none(),
            "the international variable must not satisfy the China profile"
        );

        let cn = env(&[("SUMMER_ZAI_CN_API_KEY", "cn-token")]);
        assert!(ProviderId::ZAiCn.profile().credential_from(cn).is_some());
    }

    #[test]
    fn github_copilot_credential_from_env_reads_its_own_variable() {
        assert!(
            ProviderId::GithubCopilot
                .profile()
                .credential_from(env(&[]))
                .is_none()
        );

        let copilot = env(&[("SUMMER_GITHUB_COPILOT_TOKEN", "gh-token")]);
        assert!(
            ProviderId::GithubCopilot
                .profile()
                .credential_from(copilot)
                .is_some()
        );
    }

    #[test]
    fn github_copilot_requires_the_device_flow() {
        // Pins the property `/login` depends on: no pasted token is expected
        // by default, because the OAuth device flow runs instead.
        let profile = ProviderId::GithubCopilot.profile();
        assert!(profile.oauth_device_flow);
        assert!(!profile.models.is_empty());
    }

    #[test]
    fn a_local_server_needs_no_key_and_a_hosted_one_still_does() {
        // The keyless fallback is what lets `--provider local` start with no
        // setup. Leaking it onto a hosted profile would make every session
        // "have" an OpenAI credential that 401s on the first turn.
        assert!(
            ProviderId::Local
                .profile()
                .credential_from(env(&[]))
                .is_some()
        );
        assert!(
            ProviderId::OpenAi
                .profile()
                .credential_from(env(&[]))
                .is_none()
        );
    }

    #[test]
    fn the_override_moves_the_local_server_and_never_a_hosted_one() {
        // A hosted profile that honoured the variable would send its real
        // credential to whatever host the variable names.
        let moved = env(&[(LOCAL_BASE_URL_ENV, "http://10.0.0.5:8000/v1")]);
        let local = ProviderId::Local.profile().base_url_from(&moved).unwrap();
        assert_eq!(local.as_str(), "http://10.0.0.5:8000/v1/");
        let openai = ProviderId::OpenAi.profile().base_url_from(&moved).unwrap();
        assert_eq!(openai.as_str(), OPENAI_BASE_URL);
    }

    #[test]
    fn a_missing_trailing_slash_does_not_drop_the_path_on_join() {
        // Without the slash, `chat/completions` replaces `v1` and every turn
        // 404s against a server that is running fine.
        let url = ProviderId::Local
            .profile()
            .base_url_from(env(&[(LOCAL_BASE_URL_ENV, "http://127.0.0.1:8000/v1")]))
            .unwrap();
        assert_eq!(
            url.join("chat/completions").unwrap().as_str(),
            "http://127.0.0.1:8000/v1/chat/completions"
        );
    }

    #[test]
    fn a_malformed_override_is_an_error_naming_the_variable() {
        let error = ProviderId::Local
            .profile()
            .base_url_from(env(&[(LOCAL_BASE_URL_ENV, "localhost 8080")]))
            .expect_err("not a URL")
            .to_string();
        assert!(error.contains(LOCAL_BASE_URL_ENV), "{error}");
    }

    #[test]
    fn context_window_resolves_for_a_pinned_model_and_not_an_unknown_one() {
        // The statusline's `{ctx}` divides usage by this, so a pinned model
        // must resolve to its window and an unknown id must fall through to
        // `None` (the bare-count fallback) rather than a guessed number.
        //
        // The Claude entries are 1M and Haiku is 200K, which is the bug the
        // catalog's own note describes: every Claude id used to resolve to
        // 200K, so a 1M session read as five times fuller than it was.
        assert_eq!(context_window_for(DEFAULT_MODEL), Some(1_000_000));
        assert_eq!(context_window_for("claude-opus-4-8"), Some(1_000_000));
        assert_eq!(context_window_for("claude-haiku-4-5"), Some(200_000));
        assert_eq!(context_window_for("glm-4.6"), Some(200_000));
        assert_eq!(context_window_for("glm-5.3"), Some(1_000_000));
        assert_eq!(context_window_for("not-a-real-model"), None);
    }
}
