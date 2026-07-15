//! Local API-key storage: `~/.summer/auth.json`, user-tier only.
//!
//! Never merged from a project file, unlike [`crate::config`]'s two-layer
//! shape: this is secrets, not preferences, and a cloned repo has no business
//! supplying or overriding a credential.

use std::error::Error;
use std::path::Path;

use provider::{Auth, AuthKind, PROFILES, ProviderId, ProviderProfile};
use secrecy::SecretString;
use serde::{Deserialize, Serialize};

use crate::policy;

// ========================================================================
// Constants
// ========================================================================

const AUTH_FILE: &str = "auth.json";

// ========================================================================
// Data Structures
// ========================================================================

/// The file's shape: one optional token per known provider. Plain `String`,
/// not [`SecretString`] — the wrapper only earns its keep once something
/// holds a value in memory for a while; here each token is read, wrapped into
/// an [`Auth`], and dropped within one function.
///
/// `deny_unknown_fields` so a typo'd provider name in a hand-edited file is an
/// error instead of a silently-ignored credential, the same reasoning
/// [`crate::config::Config`] already applies to its own file.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Store {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    anthropic: Option<String>,
    /// The long-lived GitHub OAuth token, from `/login`'s device flow or a
    /// hand-set `SUMMER_GITHUB_COPILOT_TOKEN`. Never the short-lived Copilot
    /// session token: [`provider::CopilotProvider`] exchanges this for
    /// one fresh before every use, so storing that one would just go stale
    /// unread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    github_copilot: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    gemini: Option<String>,
    /// Only for a local server started with `--api-key`; one without needs
    /// nothing stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    local: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    openai: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    zai: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    zai_cn: Option<String>,
}

// ========================================================================
// Store Implementations
// ========================================================================

impl Store {
    fn get(&self, id: ProviderId) -> Option<&str> {
        match id {
            ProviderId::Anthropic => self.anthropic.as_deref(),
            ProviderId::Gemini => self.gemini.as_deref(),
            ProviderId::GithubCopilot => self.github_copilot.as_deref(),
            ProviderId::Local => self.local.as_deref(),
            ProviderId::OpenAi => self.openai.as_deref(),
            ProviderId::ZAi => self.zai.as_deref(),
            ProviderId::ZAiCn => self.zai_cn.as_deref(),
        }
    }

    fn set(&mut self, id: ProviderId, token: String) {
        match id {
            ProviderId::Anthropic => self.anthropic = Some(token),
            ProviderId::Gemini => self.gemini = Some(token),
            ProviderId::GithubCopilot => self.github_copilot = Some(token),
            ProviderId::Local => self.local = Some(token),
            ProviderId::OpenAi => self.openai = Some(token),
            ProviderId::ZAi => self.zai = Some(token),
            ProviderId::ZAiCn => self.zai_cn = Some(token),
        }
    }
}

// ========================================================================
// Functions
// ========================================================================

/// The credential for `id`, from the stored file first, then that provider's
/// own environment variables. A stored token wins on purpose: `/login` is a
/// later, explicit choice, and should not be shadowed by a variable set for
/// an unrelated reason in the same shell.
pub fn credential_for(id: ProviderId) -> Result<Option<Auth>, Box<dyn Error>> {
    let profile = id.profile();
    let store = read_store()?;
    let kind = profile
        .env_vars
        .first()
        .expect("every registry profile defines at least one env var")
        .kind;
    Ok(resolve(store.get(id), profile.credential_from_env(), kind))
}

/// Every profile with a credential available right now, from the store or its
/// own environment variables — for the sandbox's port carve-out, which has to
/// know every provider a session might switch to before it starts, not just
/// the one selected at launch.
pub fn configured_profiles() -> Result<Vec<&'static ProviderProfile>, Box<dyn Error>> {
    let store = read_store()?;
    Ok(PROFILES
        .iter()
        .filter(|profile| {
            store.get(profile.id).is_some() || profile.credential_from_env().is_some()
        })
        .collect())
}

/// Every registry provider's id, comma-joined, for a usage message naming the
/// choices after an unrecognized `--provider`/`/login` value.
pub fn known_provider_names() -> String {
    PROFILES
        .iter()
        .map(|profile| profile.id.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Whether `/login`'s arguments should run an interactive device-flow login
/// instead of the usual pasted-token path: `provider` names a registered
/// provider whose profile marks `oauth_device_flow`, and no token was given.
/// A token is always taken at face value when one is present (the manual
/// escape hatch profile docs describe), so this only ever returns `Some` when
/// `token` is `None`.
pub fn device_flow_provider(provider: Option<&str>, token: Option<&str>) -> Option<ProviderId> {
    if token.is_some() {
        return None;
    }
    let id: ProviderId = provider?.parse().ok()?;
    id.profile().oauth_device_flow.then_some(id)
}

/// Whether `/login` cannot possibly succeed from this session, and the message
/// that says so instead. `~/.summer/auth.json` lives outside the workspace root
/// the Landlock domain confines the whole process to, so every store from
/// inside a sandboxed session fails with `EACCES` -- the same rule that stops
/// the agent authoring its own policy, which is why the answer is to stop
/// offering the doomed path (the established `~/.summer/policy.json` and
/// browser-launch precedent) rather than to carve out an exception for it.
/// `confined` is a parameter, not a read of the process global, so the gate is
/// testable without entering a real sandbox.
///
/// Only fires when `provider` names a *known* provider: a bare or misspelled
/// `/login` has a usage error of its own to show, and this message would only
/// bury it.
pub fn login_blocked(confined: bool, provider: Option<&str>) -> Option<String> {
    if !confined {
        return None;
    }
    Some(confined_login_error(provider?.parse().ok()?))
}

/// The held-error text for a `/login` the sandbox makes impossible: why the
/// write is refused, and the two ways in -- an unconfined session, or the
/// provider's environment variable (which a sandboxed session reads fine,
/// since the environment is not a file).
fn confined_login_error(id: ProviderId) -> String {
    let var = id
        .profile()
        .env_vars
        .first()
        .map(|credential| credential.name)
        .unwrap_or("its API-key environment variable");
    format!(
        "cannot store a credential from inside the sandbox: ~/.summer/auth.json is outside the \
         workspace root the sandbox confines this process to, so the kernel refuses the write. \
         Run /login {} without --sandbox (the default) in this directory, or set {var} in your \
         environment and restart.",
        id.as_str()
    )
}

/// Whether `/login <provider>` with no token should open an interactive key
/// entry (the TUI's masked input box, the plain loop's hidden-echo prompt)
/// instead of the usage error: `provider` names a registered provider, and it
/// is not the device-flow one -- whose argument-less `/login` runs its OAuth
/// flow instead, so prompting for a key there would be the wrong question.
/// A token, when given, is always taken at face value (the manual escape
/// hatch profile docs describe), so this is only ever consulted on the
/// no-token path.
pub fn secret_prompt_provider(provider: Option<&str>) -> Option<ProviderId> {
    if device_flow_provider(provider, None).is_some() {
        return None;
    }
    provider?.parse().ok()
}

/// Validate `/login`'s two arguments, shared by the TUI and the plain loop so
/// the usage and unknown-provider wording cannot drift between them. Each
/// caller still writes its own success message, since what to do next
/// (`/model` versus a restart) depends on which loop is running.
pub fn parse_login(
    provider: Option<&str>,
    token: Option<&str>,
) -> Result<(ProviderId, String), String> {
    let (Some(provider), Some(token)) = (provider, token) else {
        return Err(format!(
            "usage: /login <provider> [api-key] (known: {})",
            known_provider_names()
        ));
    };
    let Ok(id) = provider.parse::<ProviderId>() else {
        return Err(format!(
            "unknown provider `{provider}` (known: {})",
            known_provider_names()
        ));
    };
    Ok((id, token.to_string()))
}

/// Write `token` for `id` into the credential file, creating `~/.summer` if it
/// does not exist yet. Overwrites any prior entry for the same provider.
pub fn store(id: ProviderId, token: String) -> Result<(), Box<dyn Error>> {
    let Some(dir) = policy::config_dir() else {
        return Err("no home directory: cannot find ~/.summer to store a credential in".into());
    };
    std::fs::create_dir_all(&dir)?;
    let mut file = read_store()?;
    file.set(id, token);
    let path = dir.join(AUTH_FILE);
    std::fs::write(&path, serde_json::to_string_pretty(&file)?)?;
    restrict_permissions(&path);
    Ok(())
}

/// Store entry wins over an environment credential: pure so the priority
/// order is testable without touching a real file or the process environment.
fn resolve(stored: Option<&str>, from_env: Option<Auth>, kind: AuthKind) -> Option<Auth> {
    match stored {
        Some(token) => Some(kind.wrap(SecretString::new(token.to_string()))),
        None => from_env,
    }
}

fn read_store() -> Result<Store, Box<dyn Error>> {
    let Some(dir) = policy::config_dir() else {
        return Ok(Store::default());
    };
    let path = dir.join(AUTH_FILE);
    match policy::read_optional(&path)? {
        Some(text) => serde_json::from_str(&text)
            .map_err(|error| format!("{}: {error}", path.display()).into()),
        None => Ok(Store::default()),
    }
}

/// Best-effort: Unix gets real file-mode bits, Windows has no direct
/// equivalent so the file is only as protected as the user's profile
/// directory already is. A documented gap, not a silent one.
#[cfg(unix)]
fn restrict_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(metadata) = std::fs::metadata(path) {
        let mut permissions = metadata.permissions();
        permissions.set_mode(0o600);
        let _ = std::fs::set_permissions(path, permissions);
    }
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) {}

const MODELS_CACHE_FILE: &str = "models_cache.json";

/// Load cached Copilot models from `~/.summer/models_cache.json`.
pub fn load_cached_models() -> Option<Vec<provider::CopilotModel>> {
    let dir = policy::config_dir()?;
    let path = dir.join(MODELS_CACHE_FILE);
    let text = policy::read_optional(&path).ok()??;
    serde_json::from_str(&text).ok()
}

/// Save fetched Copilot models to `~/.summer/models_cache.json`.
pub fn save_cached_models(models: &[provider::CopilotModel]) {
    if let Some(dir) = policy::config_dir() {
        let path = dir.join(MODELS_CACHE_FILE);
        if let Ok(json) = serde_json::to_string_pretty(models) {
            let _ = std::fs::write(&path, json);
            restrict_permissions(&path);
        }
    }
}

/// Invalidate persistent models cache (e.g. on new login).
pub fn clear_cached_models() {
    if let Some(dir) = policy::config_dir() {
        let path = dir.join(MODELS_CACHE_FILE);
        let _ = std::fs::remove_file(path);
    }
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use secrecy::ExposeSecret;

    use super::*;

    fn expose(auth: Auth) -> String {
        match auth {
            Auth::ApiKey(secret) => secret.expose_secret().to_string(),
            Auth::BearerToken(secret) => secret.expose_secret().to_string(),
        }
    }

    #[test]
    fn a_stored_token_wins_over_an_environment_credential() {
        let from_env = Some(AuthKind::BearerToken.wrap(SecretString::new("from-env".into())));
        let resolved = resolve(Some("from-store"), from_env, AuthKind::ApiKey);
        assert_eq!(expose(resolved.unwrap()), "from-store");
    }

    #[test]
    fn an_environment_credential_is_used_when_nothing_is_stored() {
        let from_env = Some(AuthKind::BearerToken.wrap(SecretString::new("from-env".into())));
        let resolved = resolve(None, from_env, AuthKind::ApiKey);
        assert_eq!(expose(resolved.unwrap()), "from-env");
    }

    #[test]
    fn neither_source_resolves_to_no_credential() {
        assert!(resolve(None, None, AuthKind::ApiKey).is_none());
    }

    #[test]
    fn a_stored_token_takes_its_providers_preferred_auth_kind() {
        // Anthropic's first env var is an API key; a stored token should
        // arrive the same way, not as a bearer token that sends the wrong
        // header.
        let resolved = resolve(Some("sk-ant-stored"), None, AuthKind::ApiKey);
        assert!(matches!(resolved, Some(Auth::ApiKey(_))));
    }

    #[test]
    fn parse_login_without_arguments_is_a_usage_error() {
        let error = parse_login(None, None).expect_err("no arguments");
        assert!(error.starts_with("usage:"), "{error}");
    }

    #[test]
    fn parse_login_with_an_unknown_provider_names_it_in_the_error() {
        // A near-miss of a real id, not a plausible vendor name: "openai" was
        // used here until it became a supported provider, which is exactly the
        // hazard with naming one.
        let error = parse_login(Some("open-ai"), Some("sk-test")).expect_err("unknown provider");
        assert!(error.contains("open-ai"), "{error}");
    }

    #[test]
    fn parse_login_with_known_arguments_returns_the_id_and_token() {
        let (id, token) = parse_login(Some("zai"), Some("glm-test")).expect("known provider");
        assert_eq!(id, ProviderId::ZAi);
        assert_eq!(token, "glm-test");
    }

    #[test]
    fn device_flow_provider_only_fires_for_an_oauth_profile_with_no_token() {
        assert_eq!(
            device_flow_provider(Some("github-copilot"), None),
            Some(ProviderId::GithubCopilot),
            "the one registered oauth_device_flow provider, given with no token"
        );
        assert_eq!(
            device_flow_provider(Some("github-copilot"), Some("gh-token")),
            None,
            "a token present means the manual-paste path, not the device flow"
        );
        assert_eq!(
            device_flow_provider(Some("zai"), None),
            None,
            "zai has no device flow; a missing token there is the usage error instead"
        );
        assert_eq!(device_flow_provider(None, None), None);
        assert_eq!(
            device_flow_provider(Some("not-a-real-provider"), None),
            None
        );
    }

    #[test]
    fn secret_prompt_provider_names_api_key_providers_but_not_the_device_flow_one() {
        assert_eq!(
            secret_prompt_provider(Some("anthropic")),
            Some(ProviderId::Anthropic),
            "a plain API-key provider prompts for its key"
        );
        assert_eq!(
            secret_prompt_provider(Some("github-copilot")),
            None,
            "the device-flow provider runs its OAuth flow, not a key prompt"
        );
        assert_eq!(secret_prompt_provider(Some("not-a-real-provider")), None);
        assert_eq!(secret_prompt_provider(None), None);
    }

    #[test]
    fn login_blocked_names_the_unconfined_route_and_the_providers_env_var() {
        // The message has to carry both ways in, because either is right
        // depending on whether the user can restart the session.
        let blocked = login_blocked(true, Some("anthropic")).expect("confined blocks the store");
        assert!(blocked.contains("--sandbox"), "{blocked}");
        assert!(blocked.contains("ANTHROPIC_API_KEY"), "{blocked}");
        assert!(blocked.contains("kernel refuses"), "{blocked}");
        // The device-flow provider names its own token variable.
        let copilot = login_blocked(true, Some("github-copilot")).expect("same store path");
        assert!(copilot.contains("SUMMER_GITHUB_COPILOT_TOKEN"), "{copilot}");
    }

    #[test]
    fn login_blocked_leaves_usage_errors_to_their_own_path() {
        // A bare or unknown provider has a usage/unknown error to show; the
        // confinement message would only bury it.
        assert_eq!(login_blocked(true, None), None);
        assert_eq!(login_blocked(true, Some("open-ai")), None);
        assert_eq!(login_blocked(false, Some("anthropic")), None);
    }

    #[test]
    fn a_misspelled_provider_name_is_an_error_not_silence() {
        let error = serde_json::from_str::<Store>(r#"{"zia":"token"}"#)
            .expect_err("an unknown key must not parse");
        assert!(error.to_string().contains("zia"), "{error}");
    }

    #[test]
    fn store_get_and_set_round_trip_per_provider() {
        let mut store = Store::default();
        assert_eq!(store.get(ProviderId::ZAi), None);
        store.set(ProviderId::ZAi, "glm-token".into());
        assert_eq!(store.get(ProviderId::ZAi), Some("glm-token"));
        assert_eq!(
            store.get(ProviderId::Anthropic),
            None,
            "providers are independent"
        );
    }
}
