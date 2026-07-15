//! The model, effort, and provider pickers.
//!
//! One modal list surface serving three choices, because they are the same
//! interaction: show what is available, move through it, commit or cancel. The
//! stage the picker is in decides what committing means.
//!
//! **A model's context window is asked for, not assumed.** The static registry
//! carries a default per provider, but GitHub Copilot reports a per-model window
//! at runtime, and a stale constant here would silently mis-size the compaction
//! threshold for every turn of the session.

use std::error::Error;

use provider::{
    Auth, Effort, ProviderConfig, ProviderId, ProviderProfile, fetch_models, transport,
};
use ratatui::crossterm::event::{KeyCode, KeyModifiers};

use crate::credentials;
use crate::tui::input::InputAction;
use crate::tui::session::{resume_session, rewind_conversation};
use crate::tui::state::{PickerDisplay, PickerStage, TuiState};
use crate::tui::{DriverCx, PickerCx, redraw};

// ========================================================================
// Functions
// ========================================================================

/// `/model`: open the picker over every provider with a credential right now.
/// A provider with none is not listed, since selecting it could only end in
/// the same "no credential" state `Bootstrap::load` already reports at
/// startup — the way in is `/login`, not a picker row that always fails.
pub(super) fn open_model_picker(cx: &mut DriverCx<'_>) -> Result<(), Box<dyn Error>> {
    let profiles = credentials::configured_profiles()?;
    if profiles.is_empty() {
        cx.state.notify(format!(
            "no provider has a credential yet; try /login <provider> (known: {})",
            credentials::known_provider_names()
        ));
        return Ok(());
    }
    cx.input.clear();
    cx.state.pending_picker = Some(PickerDisplay {
        selected: 0,
        stage: PickerStage::Providers { profiles },
    });
    Ok(())
}

/// Open `/model`'s picker drilled straight into one provider's model list,
/// fetching Copilot's account-specific catalog live when that provider is it.
/// Shared by `/login` (land on the just-logged-in provider's models) and the
/// picker's own provider row, so the live-fetch and fallback behavior cannot
/// drift between the two entry points.
pub(super) async fn open_model_picker_for(
    cx: &mut PickerCx<'_>,
    profile: &'static ProviderProfile,
) -> Result<(), Box<dyn Error>> {
    let mut dynamic_models = None;
    if profile.id == ProviderId::GithubCopilot {
        if let Some(cached) = &cx.state.cached_copilot_models {
            dynamic_models = Some(cached.clone());
        } else if let Some(disk_cached) = credentials::load_cached_models() {
            cx.state.cached_copilot_models = Some(disk_cached.clone());
            dynamic_models = Some(disk_cached);
        } else if let Ok(Some(auth)) = credentials::credential_for(profile.id) {
            let Auth::BearerToken(github_token) = auth else {
                unreachable!("GitHub Copilot's credential is BearerToken");
            };
            let client = reqwest::Client::new();
            cx.state
                .notify("fetching models from GitHub Copilot...".to_string());
            let _ = redraw(cx.terminal, cx.state, cx.input);
            match fetch_models(&client, provider::SESSION_TOKEN_URL, &github_token).await {
                Ok(models) => {
                    credentials::save_cached_models(&models);
                    cx.state.cached_copilot_models = Some(models.clone());
                    dynamic_models = Some(models);
                    cx.state.clear_banner();
                }
                Err(e) => {
                    cx.state.notify_error(format!(
                        "could not fetch Copilot models: {e}; using fallback list"
                    ));
                }
            }
        }
    }
    cx.input.clear();
    cx.state.pending_picker = Some(PickerDisplay {
        selected: 0,
        stage: PickerStage::Models {
            profile,
            dynamic_models,
        },
    });
    Ok(())
}

/// Apply one key to an open `/model` picker. The prompt doubles as a filter
/// box: typing (with the editor's full emacs table) narrows the list, and
/// Up/Down/Ctrl-N/Ctrl-P move the selection within what remains. Enter drills
/// into a provider's models or confirms a model; Esc cancels. Intercepts the
/// idle loop's normal key handling entirely while open, the same way a
/// permission prompt intercepts `run_turn`'s.
pub(super) async fn handle_picker_key(
    cx: &mut PickerCx<'_>,
    key: &ratatui::crossterm::event::KeyEvent,
) -> Result<(), Box<dyn Error>> {
    // The filter narrows the list the selection moves through, so the visible
    // count is whatever matches the query right now -- recomputed per key.
    let count = match &cx.state.pending_picker {
        Some(picker) => picker.matching(cx.input.buffer()).len(),
        None => return Ok(()),
    };
    let is_effort = matches!(
        cx.state.pending_picker.as_ref().map(|p| &p.stage),
        Some(PickerStage::Effort { .. })
    );
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    // Navigation moves the selection within the filtered list or slider.
    let nav: Option<i32> = if is_effort {
        if ctrl {
            match key.code {
                KeyCode::Char('n' | 'N') => Some(1),
                KeyCode::Char('p' | 'P') => Some(-1),
                _ => None,
            }
        } else {
            match key.code {
                KeyCode::Right | KeyCode::Down | KeyCode::Char('l' | 'L') => Some(1),
                KeyCode::Left | KeyCode::Up | KeyCode::Char('h' | 'H') => Some(-1),
                KeyCode::BackTab => Some(-1),
                KeyCode::Tab if shift => Some(-1),
                KeyCode::Tab => Some(1),
                _ => None,
            }
        }
    } else if ctrl {
        match key.code {
            KeyCode::Char('n' | 'N') => Some(1),
            KeyCode::Char('p' | 'P') => Some(-1),
            _ => None,
        }
    } else {
        match key.code {
            KeyCode::Down => Some(1),
            KeyCode::Up => Some(-1),
            _ => None,
        }
    };
    if let Some(delta) = nav {
        if let Some(picker) = cx.state.pending_picker.as_mut() {
            if count == 0 {
                picker.selected = 0;
            } else if delta > 0 {
                picker.selected = (picker.selected + delta as usize).min(count - 1);
            } else {
                picker.selected = picker
                    .selected
                    .saturating_sub(delta.unsigned_abs() as usize);
            }
        }
        return Ok(());
    }
    match key.code {
        // Esc on the effort stage skips it, as its banner says, and still
        // switches to the model just chosen with the effort it already had.
        // Everywhere else it backs out of the whole picker.
        KeyCode::Esc => {
            let picker = cx.state.pending_picker.take();
            cx.input.clear();
            if let Some(PickerDisplay {
                stage: PickerStage::Effort { profile, model, .. },
                ..
            }) = picker
            {
                switch_provider(cx, profile, &model)?;
            }
        }
        KeyCode::Enter => {
            confirm_picker_selection(cx).await?;
        }
        _ => match cx.input.handle(*key) {
            // Ctrl-D on an empty filter (or Ctrl-C): leave the picker, not the
            // session -- the editor's EOF reading is "close this surface".
            InputAction::Exit | InputAction::ConfirmExit => cx.state.pending_picker = None,
            InputAction::Edited | InputAction::None => {
                if let Some(picker) = cx.state.pending_picker.as_mut() {
                    let n = picker.matching(cx.input.buffer()).len();
                    picker.selected = picker.selected.min(n.saturating_sub(1));
                }
            }
            InputAction::Submit
            | InputAction::ScrollUp
            | InputAction::ScrollDown
            | InputAction::SwitchMode
            | InputAction::ShowKeybindings => {}
        },
    }
    Ok(())
}

/// Enter on an open picker: a provider row drills into that provider's
/// models; a model row switches the running engine to it.
async fn confirm_picker_selection(cx: &mut PickerCx<'_>) -> Result<(), Box<dyn Error>> {
    let Some(picker) = cx.state.pending_picker.take() else {
        return Ok(());
    };
    // Resolve the selection against the filter that was live when Enter was
    // pressed: `selected` indexes the filtered list, so the original row is the
    // one at that position in `matching`.
    let query = cx.input.buffer().to_string();
    let Some(&index) = picker.matching(&query).get(picker.selected) else {
        // Nothing matches: put the picker back unchanged so the user can widen
        // the query rather than silently losing the stage they were on.
        cx.state.pending_picker = Some(picker);
        return Ok(());
    };
    // A real selection: the filter has served its purpose, drop it.
    cx.input.clear();
    match picker.stage {
        PickerStage::Providers { profiles } => {
            if let Some(profile) = profiles.get(index).copied() {
                open_model_picker_for(cx, profile).await?;
            }
        }
        PickerStage::Models {
            profile,
            dynamic_models,
        } => {
            let (model_name, effort_caps) = if let Some(dynamic) = &dynamic_models {
                dynamic.get(index).map_or((None, None), |m| {
                    (Some(m.id.clone()), m.reasoning_effort.clone())
                })
            } else {
                let name = profile.models.get(index).map(|m| m.name.to_string());
                (name, None)
            };
            let Some(model_name) = model_name else {
                return Ok(());
            };

            // Build effort levels list: "off" is first, followed by supported/standard levels.
            let mut levels = vec!["off".to_string()];
            if let Some(caps) = effort_caps.as_ref().filter(|c| !c.is_empty()) {
                for lvl in caps {
                    if lvl != "off" && !levels.contains(lvl) {
                        levels.push(lvl.clone());
                    }
                }
            } else {
                levels.extend(vec![
                    "low".to_string(),
                    "medium".to_string(),
                    "high".to_string(),
                    "xhigh".to_string(),
                    "max".to_string(),
                ]);
            }

            let supports_effort = match &effort_caps {
                Some(caps) => !caps.is_empty(),
                None => {
                    profile.id == ProviderId::Anthropic
                        || model_name.contains("opus")
                        || model_name.contains("sonnet")
                        || model_name.contains("o1")
                        || model_name.contains("o3")
                }
            };

            let selected = if let Some(current_eff) = effort_for(cx.state, &model_name) {
                levels
                    .iter()
                    .position(|l| l == current_eff.as_str())
                    .unwrap_or(0)
            } else if supports_effort {
                levels
                    .iter()
                    .position(|l| l == "xhigh")
                    .or_else(|| levels.iter().position(|l| l == "high"))
                    .unwrap_or(0)
            } else {
                0 // "off" for non-reasoning models
            };

            cx.state.notify(format!(
                "choose reasoning effort for {model_name} (Esc to skip)"
            ));
            cx.state.pending_picker = Some(PickerDisplay {
                selected,
                stage: PickerStage::Effort {
                    profile,
                    model: model_name,
                    levels,
                },
            });
        }
        PickerStage::Sessions { rows } => {
            if let Some(row) = rows.get(index) {
                let id = row.id.clone();
                resume_session(cx.engine_slot, cx.state, id)?;
            }
        }
        PickerStage::Rewind { rows } => {
            if let Some(text) = rows.get(index) {
                rewind_conversation(cx, index + 1, text);
            }
        }
        PickerStage::Effort {
            profile,
            model,
            levels,
        } => {
            // Record the chosen level (or clear if off) and switch provider.
            // `index`, the row in the full list, as every other stage uses:
            // `selected` indexes the filtered list, so typing `x` and pressing
            // Enter on `xhigh` applied `off`, the first row unfiltered.
            if let Some(level) = levels.get(index) {
                if level == "off" || level == "none" {
                    cx.state.efforts.remove(&model);
                } else if let Some(effort) = Effort::parse(level) {
                    cx.state.efforts.insert(model.clone(), effort);
                }
            }
            switch_provider(cx, profile, &model)?;
        }
    }
    Ok(())
}

/// `/effort [level]`: set the reasoning effort for the current model, applied
/// to the running provider live and persisted as the default for the next
/// session. No argument prints the current level and the choices; `off` (or
/// `none`) sends no `output_config.effort` at all, for models that reject it.
pub(super) fn set_effort(cx: &mut DriverCx<'_>, level: &str) {
    let model = cx.state.status.model.clone();
    let choices = "low medium high xhigh max, or off";
    if level.is_empty() {
        cx.state.notify(format!(
            "effort for {model}: {}  (set one of: {choices})",
            effort_for(cx.state, &model)
                .map(Effort::as_str)
                .unwrap_or("off")
        ));
        return;
    }
    if matches!(level.to_ascii_lowercase().as_str(), "off" | "none") {
        cx.state.efforts.remove(&model);
        cx.state.status.effort = None;
        if let Some(engine) = cx.engine_slot.as_mut() {
            engine.set_effort(None);
        }
        let saved = crate::config::save_provider_prefs(
            cx.state.provider_id.as_str(),
            &model,
            None,
            cx.state.status.context_window,
        );
        match saved {
            Ok(()) => cx
                .state
                .notify(format!("effort for {model} turned off (saved as default)")),
            Err(error) => cx.state.notify_error(format!(
                "effort for {model} turned off this session; could not save: {error}"
            )),
        };
        return;
    }
    let Some(effort) = Effort::parse(level) else {
        cx.state
            .notify_error(format!("unknown effort `{level}`; try one of: {choices}"));
        return;
    };
    cx.state.efforts.insert(model.clone(), effort);
    cx.state.status.effort = Some(effort.as_str().to_owned());
    if let Some(engine) = cx.engine_slot.as_mut() {
        engine.set_effort(Some(effort));
    }
    let level_str = effort.as_str();
    let saved = crate::config::save_provider_prefs(
        cx.state.provider_id.as_str(),
        &model,
        Some(level_str),
        cx.state.status.context_window,
    );
    match saved {
        Ok(()) => cx.state.notify(format!(
            "effort for {model} set to {level_str} (saved as default)"
        )),
        Err(error) => cx.state.notify_error(format!(
            "effort for {model} set to {level_str} this session; could not save: {error}"
        )),
    }
}

/// The reasoning effort chosen for `model`, or `None` (off) when none has been
/// set this session. Off is the default because some models 400 on
/// `output_config.effort`; a `/effort <level>` turns it on per model.
pub(super) fn effort_for(state: &TuiState, model: &str) -> Option<Effort> {
    state.efforts.get(model).copied()
}

/// The context window for `model`: Copilot's live `/models` value when the
/// catalog is in hand (the static registry pins only a subset of Copilot's
/// models, so a live id like a just-fetched one is unknown to it), otherwise
/// the static registry. `None` only when neither source knows the model, in
/// which case the statusline falls back to a bare token count.
fn resolve_context_window(model: &str, copilot: Option<&[provider::CopilotModel]>) -> Option<u64> {
    copilot
        .and_then(|models| models.iter().find(|m| m.id == model))
        .and_then(|m| m.max_context_window_tokens)
        .or_else(|| provider::context_window_for(model))
}

/// Actually swap the running engine's provider: resolve the credential again
/// (the same resolution `Bootstrap::load` used at startup) and hand a fresh
/// transport for the chosen model to the engine. Safe against the sandbox's
/// carve-out because the picker's provider list only ever came from
/// [`credentials::configured_profiles`], the exact set `Bootstrap::load`
/// carved a port out for.
pub(super) fn switch_provider(
    cx: &mut PickerCx<'_>,
    profile: &'static ProviderProfile,
    model_name: &str,
) -> Result<(), Box<dyn Error>> {
    let Some(auth) = credentials::credential_for(profile.id)? else {
        cx.state.notify_error(format!(
            "no credential for {} anymore; use /login {} <token>",
            profile.label,
            profile.id.as_str()
        ));
        return Ok(());
    };
    let Some(engine) = cx.engine_slot.as_mut() else {
        return Ok(());
    };
    let config = match ProviderConfig::for_profile(profile, Some(model_name), auth) {
        Ok(config) => config,
        Err(error) => {
            cx.state.notify_error(error.to_string());
            return Ok(());
        }
    };
    // Through the shared mapping: a hand-written branch here built every
    // non-Copilot provider on the Anthropic transport, so switching to OpenAI
    // or Gemini sent Messages-API requests to a chat-completions endpoint.
    engine.set_provider(transport(profile.id, config));
    engine.set_effort(effort_for(cx.state, model_name));
    cx.state.context_budget = Some(u64::from(engine.context_budget()));
    cx.state.status.model = model_name.to_string();
    cx.state.status.effort = effort_for(cx.state, model_name).map(|e| e.as_str().to_owned());
    cx.state.status.context_window =
        resolve_context_window(model_name, cx.state.cached_copilot_models.as_deref());
    cx.state.provider = profile.label.to_string();
    cx.state.provider_id = profile.id;
    let effort = effort_for(cx.state, model_name);
    let window = cx.state.status.context_window;
    match crate::config::save_provider_prefs(
        profile.id.as_str(),
        model_name,
        effort.map(Effort::as_str),
        window,
    ) {
        Ok(()) => cx.state.notify(format!(
            "switched to {} ({}) and saved as default",
            profile.label, model_name
        )),
        Err(error) => cx.state.notify_error(format!(
            "switched to {} ({}) for this session; could not save as default: {error}",
            profile.label, model_name
        )),
    }
    Ok(())
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {

    use ratatui::crossterm::event::KeyEvent;

    use crate::tui::testing::state_with;

    use crate::tui::input::InputEditor;

    use super::*;
    #[test]
    fn resolve_context_window_prefers_the_live_copilot_value() {
        // The static registry pins only a subset of Copilot's models, so a live
        // id (from `/models`) is unknown to it. The live catalog must supply the
        // window so the statusline shows `0% ctx (size)` rather than `0K ctx`.
        let copilot = vec![provider::CopilotModel {
            id: "claude-sonnet-4-5".into(),
            name: None,
            multiplier: None,
            max_context_window_tokens: Some(1_000_000),
            reasoning_effort: None,
        }];
        assert_eq!(
            resolve_context_window("claude-sonnet-4-5", Some(&copilot)),
            Some(1_000_000),
            "live value wins for an unlisted model"
        );
        assert_eq!(
            resolve_context_window("claude-opus-4-8", None),
            Some(1_000_000),
            "registry resolves a pinned model with no live catalog"
        );
        assert_eq!(
            resolve_context_window("mystery-model", Some(&copilot)),
            None,
            "neither source knows it"
        );
    }

    #[tokio::test]
    async fn picker_navigates_with_ctrl_n_and_ctrl_p() {
        use super::handle_picker_key;
        use crate::tui::state::{PickerDisplay, PickerStage};
        use provider::PROFILES;
        use ratatui::Terminal;
        use ratatui::backend::CrosstermBackend;
        use std::io;

        let mut state = state_with(0, &[]);
        state.pending_picker = Some(PickerDisplay {
            selected: 0,
            stage: PickerStage::Models {
                profile: &PROFILES[2],
                dynamic_models: None,
            },
        });

        let mut input = InputEditor::new();
        let backend = CrosstermBackend::new(io::stdout());
        let mut terminal = Terminal::new(backend).unwrap();
        let mut engine_slot = None;

        let mut cx = super::PickerCx {
            terminal: &mut terminal,
            engine_slot: &mut engine_slot,
            state: &mut state,
            input: &mut input,
        };

        let ctrl_n = KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL);
        let ctrl_p = KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL);

        assert_eq!(cx.state.pending_picker.as_ref().unwrap().selected, 0);

        handle_picker_key(&mut cx, &ctrl_n).await.unwrap();
        assert_eq!(cx.state.pending_picker.as_ref().unwrap().selected, 1);

        handle_picker_key(&mut cx, &ctrl_p).await.unwrap();
        assert_eq!(cx.state.pending_picker.as_ref().unwrap().selected, 0);
    }

    #[tokio::test]
    async fn effort_slider_navigates_with_tab_and_backtab() {
        use super::handle_picker_key;
        use crate::tui::state::{PickerDisplay, PickerStage};
        use provider::PROFILES;
        use ratatui::Terminal;
        use ratatui::backend::CrosstermBackend;
        use std::io;

        let mut state = state_with(0, &[]);
        state.pending_picker = Some(PickerDisplay {
            selected: 0,
            stage: PickerStage::Effort {
                profile: &PROFILES[0],
                model: "claude-opus-4-8".into(),
                levels: vec![
                    "off".into(),
                    "low".into(),
                    "medium".into(),
                    "high".into(),
                    "xhigh".into(),
                    "max".into(),
                ],
            },
        });

        let mut input = InputEditor::new();
        let backend = CrosstermBackend::new(io::stdout());
        let mut terminal = Terminal::new(backend).unwrap();
        let mut engine_slot = None;

        let mut cx = super::PickerCx {
            terminal: &mut terminal,
            engine_slot: &mut engine_slot,
            state: &mut state,
            input: &mut input,
        };

        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        let backtab = KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE);
        let shift_tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT);

        assert_eq!(cx.state.pending_picker.as_ref().unwrap().selected, 0);

        handle_picker_key(&mut cx, &tab).await.unwrap();
        assert_eq!(
            cx.state.pending_picker.as_ref().unwrap().selected,
            1,
            "Tab moves forward"
        );

        handle_picker_key(&mut cx, &tab).await.unwrap();
        assert_eq!(
            cx.state.pending_picker.as_ref().unwrap().selected,
            2,
            "Tab moves forward again"
        );

        handle_picker_key(&mut cx, &backtab).await.unwrap();
        assert_eq!(
            cx.state.pending_picker.as_ref().unwrap().selected,
            1,
            "BackTab moves backward"
        );

        handle_picker_key(&mut cx, &shift_tab).await.unwrap();
        assert_eq!(
            cx.state.pending_picker.as_ref().unwrap().selected,
            0,
            "Shift+Tab moves backward"
        );
    }

    #[tokio::test]
    async fn open_model_picker_for_uses_cached_copilot_models() {
        use super::open_model_picker_for;
        use crate::tui::state::PickerStage;
        use provider::{CopilotModel, ProviderId};
        use ratatui::Terminal;
        use ratatui::backend::CrosstermBackend;
        use std::io;

        let profile = ProviderId::GithubCopilot.profile();
        let mut state = state_with(0, &[]);
        let mock_models = vec![CopilotModel {
            id: "cached-gpt-4o".into(),
            name: Some("Cached GPT-4o".into()),
            multiplier: None,
            max_context_window_tokens: None,
            reasoning_effort: None,
        }];
        state.cached_copilot_models = Some(mock_models.clone());

        let mut input = InputEditor::new();
        let backend = CrosstermBackend::new(io::stdout());
        let mut terminal = Terminal::new(backend).unwrap();
        let mut engine_slot = None;

        let mut cx = super::PickerCx {
            terminal: &mut terminal,
            engine_slot: &mut engine_slot,
            state: &mut state,
            input: &mut input,
        };

        open_model_picker_for(&mut cx, profile).await.unwrap();

        let picker = cx.state.pending_picker.as_ref().expect("picker opened");
        match &picker.stage {
            PickerStage::Models {
                dynamic_models: Some(models),
                ..
            } => {
                assert_eq!(models.len(), 1);
                assert_eq!(models[0].id, "cached-gpt-4o");
            }
            other => panic!("expected cached dynamic models, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn models_without_reasoning_effort_default_to_off_in_effort_picker() {
        use super::confirm_picker_selection;
        use crate::tui::state::{PickerDisplay, PickerStage};
        use provider::{CopilotModel, ProviderId};
        use ratatui::Terminal;
        use ratatui::backend::CrosstermBackend;
        use std::io;

        let profile = ProviderId::GithubCopilot.profile();
        let mut state = state_with(0, &[]);
        let unsupported_model = CopilotModel {
            id: "claude-haiku-4.5".into(),
            name: Some("Claude Haiku 4.5".into()),
            multiplier: None,
            max_context_window_tokens: None,
            reasoning_effort: None, // No reasoning effort support!
        };
        state.pending_picker = Some(PickerDisplay {
            selected: 0,
            stage: PickerStage::Models {
                profile,
                dynamic_models: Some(vec![unsupported_model]),
            },
        });

        let mut input = InputEditor::new();
        let backend = CrosstermBackend::new(io::stdout());
        let mut terminal = Terminal::new(backend).unwrap();
        let mut engine_slot = None;

        let mut cx = super::PickerCx {
            terminal: &mut terminal,
            engine_slot: &mut engine_slot,
            state: &mut state,
            input: &mut input,
        };

        confirm_picker_selection(&mut cx).await.unwrap();

        // Must open effort stage defaulting to index 0 ("off")
        let picker = cx.state.pending_picker.as_ref().expect("picker open");
        assert_eq!(picker.selected, 0, "defaults to 'off'");
        match &picker.stage {
            PickerStage::Effort { levels, model, .. } => {
                assert_eq!(model, "claude-haiku-4.5");
                assert_eq!(levels[0], "off");
            }
            other => panic!("expected Effort picker stage, got {other:?}"),
        }
    }
}
