//! Acquiring a credential: `/login`, the key prompt, and the OAuth device flow.
//!
//! **Confinement costs land here rather than on the repository.** A credential is
//! stored under `~/.summer`, which is outside the workspace root, and the device
//! flow wants to open a browser. Both are things an `--sandbox` session
//! deliberately cannot do, so these paths report that plainly instead of failing
//! in a way that reads like a bug.
//!
//! The key prompt is a separate input surface from the line editor, for one
//! reason: what is typed into it must never reach the transcript, the command
//! buffer, or a `Debug`. A paste is routed to whichever of the two is open.

use std::error::Error;
use std::time::Duration;

use futures_util::StreamExt;
use provider::{
    ACCESS_TOKEN_URL, DEVICE_CODE_URL, PollOutcome, ProviderId, ProviderProfile, poll_device_flow,
    start_device_flow,
};
use ratatui::crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use secrecy::ExposeSecret;

use crate::credentials;
use crate::tui::picker::open_model_picker_for;
use crate::tui::state::TuiState;
use crate::tui::{DriverCx, is_ctrl_c, redraw};

// ========================================================================
// Functions
// ========================================================================

/// `/login <provider> [api-key]`: store a credential locally, the key either
/// given on the line or typed into the masked entry (which confirms through
/// [`store_credential`]). Returns the profile
/// just logged into on success, so the driver can open its model picker next.
/// The picker re-resolves the credential and rebuilds the provider live (see
/// [`crate::tui::picker::switch_provider`]), so the default sandbox-off session needs no restart;
/// under `--sandbox` a provider logged in mid-session has no port carved out
/// (the carve-out is fixed at startup), so its turns are not reachable until a
/// restart -- use a provider that already had a credential at launch instead.
pub(super) fn login(
    state: &mut TuiState,
    provider: Option<&str>,
    token: Option<&str>,
) -> Option<&'static ProviderProfile> {
    let (id, token) = match credentials::parse_login(provider, token) {
        Ok(pair) => pair,
        Err(message) => {
            state.notify_error(message);
            return None;
        }
    };
    store_credential(state, id, token)
}

/// Store `token` for `id` and report the outcome. Split out of [`login`] so
/// the masked key entry confirms through the identical path: the same
/// notification, Copilot cache clearing, and model-picker follow-up.
fn store_credential(
    state: &mut TuiState,
    id: ProviderId,
    token: String,
) -> Option<&'static ProviderProfile> {
    match credentials::store(id, token) {
        Ok(()) => {
            if id == ProviderId::GithubCopilot {
                state.cached_copilot_models = None;
                credentials::clear_cached_models();
            }
            state.notify(format!("logged in to {}.", id.profile().label));
            Some(id.profile())
        }
        Err(error) => {
            state.notify_error(format!("could not store credential: {error}"));
            None
        }
    }
}

/// Apply one key to an open API-key entry: characters append (masked on
/// screen, never in the command buffer), Backspace deletes, Enter stores the
/// key and lands on that provider's model picker -- the same follow-up
/// `/login <provider> [api-key]` has -- and Esc or Ctrl-C cancels. Enter on an
/// empty buffer is ignored, so a slip cannot store an empty credential.
pub(super) async fn handle_secret_key(
    cx: &mut DriverCx<'_>,
    key: &ratatui::crossterm::event::KeyEvent,
) -> Result<(), Box<dyn Error>> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Esc => {
            cx.state.pending_secret = None;
            cx.state.notify("login cancelled".to_string());
        }
        KeyCode::Char('c') if ctrl => {
            cx.state.pending_secret = None;
            cx.state.notify("login cancelled".to_string());
        }
        KeyCode::Backspace => {
            if let Some(secret) = cx.state.pending_secret.as_mut() {
                secret.pop();
            }
        }
        KeyCode::Char(ch) if !ctrl => {
            if let Some(secret) = cx.state.pending_secret.as_mut() {
                secret.push(ch);
            }
        }
        KeyCode::Enter => {
            let Some(secret) = cx.state.pending_secret.take() else {
                return Ok(());
            };
            if secret.is_empty() {
                // Nothing typed: keep the entry open rather than storing an
                // empty credential (or dead-ending on a usage error).
                cx.state.pending_secret = Some(secret);
            } else if let Some(profile) =
                store_credential(cx.state, secret.id, secret.secret().to_string())
            {
                open_model_picker_for(&mut cx.picker(), profile).await?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// `/login <id>` with no token, for a provider whose profile marks
/// `oauth_device_flow`: run GitHub's device-code flow to completion inline,
/// the same shape [`super::run_turn`] runs a turn in — its own nested event loop
/// owns every keystroke while it runs, so Esc or Ctrl-C can cancel while the
/// rest of the driver stays paused.
///
/// A thin wrapper around [`run_device_flow_login_inner`] for one reason:
/// `pending_login` must clear on *every* exit from the inner function (it has
/// several early returns -- a failed request, a cancel from either of its two
/// waits, the closed-event-stream case, and normal completion), and setting
/// it here once and clearing it here once after the single `.await` is
/// simpler than remembering to reset it at each of those points individually.
pub(super) async fn run_device_flow_login(
    cx: &mut DriverCx<'_>,
    id: ProviderId,
) -> Result<(), Box<dyn Error>> {
    cx.state.pending_login = Some(format!("{}: starting device-flow login…", id.as_str()));
    let result = run_device_flow_login_inner(cx, id).await;
    cx.state.pending_login = None;
    result
}

/// Does the actual work of [`run_device_flow_login`]. Asks before opening the
/// verification URL in a browser, since launching one is a visible action on
/// the user's machine a silent default should not take. There is only one
/// `oauth_device_flow` provider today (GitHub Copilot), so this calls its
/// device-flow functions directly rather than through an indirection nothing
/// else would use yet.
async fn run_device_flow_login_inner(
    cx: &mut DriverCx<'_>,
    id: ProviderId,
) -> Result<(), Box<dyn Error>> {
    let client = reqwest::Client::new();
    let code = match start_device_flow(&client, DEVICE_CODE_URL).await {
        Ok(code) => code,
        Err(error) => {
            cx.state.notify_error(format!("{}: {error}", id.as_str()));
            return Ok(());
        }
    };
    cx.state.notify(format!(
        "{}: go to {} and enter code {}",
        id.as_str(),
        code.verification_uri,
        code.user_code
    ));
    cx.state.notify(format!(
        "{}: open that URL in your browser now? [Y/n]",
        id.as_str()
    ));
    cx.state.pending_login = Some(format!("{}: waiting for your answer [y/n]…", id.as_str()));
    let open_browser = loop {
        redraw(cx.terminal, cx.state, cx.input)?;
        match cx.events.next().await {
            Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                if is_ctrl_c(&key) || matches!(key.code, KeyCode::Esc) {
                    cx.state.notify(format!("{}: login cancelled", id.as_str()));
                    return Ok(());
                }
                match key.code {
                    KeyCode::Char('y' | 'Y') | KeyCode::Enter => break true,
                    KeyCode::Char('n' | 'N') => break false,
                    _ => {}
                }
            }
            Some(_) => {}
            // The event stream closed: the same "give up" signal `drive`'s own
            // loop treats as end of input.
            None => return Ok(()),
        }
    };
    if open_browser {
        match crate::open_url(&code.verification_uri) {
            Ok(()) => cx.state.notify(format!("{}: opened in your browser.", id.as_str())),
            Err(error) => cx.state.notify_error(format!(
                "{}: could not open a browser automatically ({error}); open the URL above yourself.",
                id.as_str()
            )),
        }
    }

    cx.state.pending_login = Some(format!(
        "{}: waiting for you to approve in the browser…",
        id.as_str()
    ));
    let mut interval = code.interval;
    let outcome = loop {
        redraw(cx.terminal, cx.state, cx.input)?;
        tokio::select! {
            biased;
            () = tokio::time::sleep(interval) => {
                match poll_device_flow(&client, ACCESS_TOKEN_URL, &code.device_code).await {
                    Ok(PollOutcome::Pending) => {}
                    Ok(PollOutcome::SlowDown { interval: slower }) => {
                        interval = slower.unwrap_or(interval) + Duration::from_secs(5);
                        cx.state.pending_login = Some(format!(
                            "{}: GitHub asked us to slow down; checking every {}s…",
                            id.as_str(),
                            interval.as_secs()
                        ));
                    }
                    other => break other,
                }
            }
            event = cx.events.next() => {
                if let Some(Ok(Event::Key(key))) = event
                    && key.kind == KeyEventKind::Press
                    && (matches!(key.code, KeyCode::Esc) || is_ctrl_c(&key))
                {
                    cx.state.notify(format!("{}: login cancelled", id.as_str()));
                    return Ok(());
                }
            }
        }
    };

    match outcome {
        Ok(PollOutcome::Authorized { github_token }) => {
            match credentials::store(id, github_token.expose_secret().to_string()) {
                // The picker re-resolves the credential and rebuilds the
                // provider (see [`crate::tui::picker::switch_provider`]), so landing on the model
                // list now takes effect immediately -- no restart, unlike the
                // plain REPL path which has no live engine swap.
                Ok(()) => {
                    cx.state
                        .notify(format!("logged in to {}.", id.profile().label));
                    open_model_picker_for(&mut cx.picker(), id.profile()).await?;
                }
                Err(error) => cx
                    .state
                    .notify_error(format!("could not store credential: {error}")),
            }
        }
        Ok(PollOutcome::Expired) => cx.state.notify_error(format!(
            "{}: the device code expired; run /login {} again",
            id.as_str(),
            id.as_str()
        )),
        Ok(PollOutcome::Denied) => cx
            .state
            .notify_error(format!("{}: authorization was denied", id.as_str())),
        Ok(PollOutcome::Pending | PollOutcome::SlowDown { .. }) => {
            unreachable!("the loop above only breaks on Authorized, Expired, Denied, or Err")
        }
        Err(error) => cx.state.notify_error(format!("{}: {error}", id.as_str())),
    }
    Ok(())
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use crate::tui::input::InputEditor;
    use crate::tui::route_paste;
    use crate::tui::state::SecretInput;
    use crate::tui::testing::state_with;

    use super::*;

    #[test]
    fn login_without_arguments_shows_usage() {
        let mut state = state_with(0, &[]);
        login(&mut state, None, None);
        match state.pending_messages.last().map(|n| n.text.as_str()) {
            Some(text) => assert!(text.starts_with("Usage:"), "{text}"),
            other => panic!("expected a usage message, got {other:?}"),
        }
    }

    #[test]
    fn a_paste_lands_in_the_open_key_entry_not_the_command_buffer() {
        // The exact regression: bracketed paste went to the command buffer
        // unconditionally, so pasting the key into the masked entry looked
        // like a no-op (that buffer is not rendered while the entry is open)
        // and left the key sitting in the clear.
        let mut state = state_with(0, &[]);
        state.pending_secret = Some(SecretInput::new(ProviderId::Anthropic));
        let mut input = InputEditor::new();
        route_paste(&mut state, &mut input, "  sk-ant-pasted\r\n");
        assert_eq!(
            state.pending_secret.expect("entry stays open").secret(),
            "sk-ant-pasted",
            "the paste is the key, trimmed like a clipboard needs"
        );
        assert!(
            input.buffer().is_empty(),
            "the key must not sit in the clear-text command buffer"
        );

        // With no entry open, a paste is composed text as it always was.
        let mut plain = state_with(0, &[]);
        let mut input = InputEditor::new();
        route_paste(&mut plain, &mut input, "line one\nline two");
        assert_eq!(input.buffer(), "line one\nline two");
    }

    #[test]
    fn login_with_an_unknown_provider_is_an_error() {
        // Neither `credentials::store` nor a partial write must happen for a
        // name that does not parse: the error path returns before either.
        let mut state = state_with(0, &[]);
        login(&mut state, Some("open-ai"), Some("sk-test"));
        match state.pending_messages.last().map(|n| n.text.as_str()) {
            Some(text) => assert!(text.to_lowercase().contains("unknown provider"), "{text}"),
            other => panic!("expected an unknown-provider message, got {other:?}"),
        }
    }
}
