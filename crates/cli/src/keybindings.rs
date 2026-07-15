//! Custom keybinding loader for `~/.summer/keybindings.json` and `.summer/keybindings.json`.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

// ========================================================================
// Data Structures
// ========================================================================

/// A custom keybinding action command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyAction {
    CursorLineStart,
    CursorLineEnd,
    MoveWordForward,
    MoveWordBack,
    KillToEnd,
    KillToStart,
    KillWordBack,
    KillWordForward,
    TransposeWords,
    TransposeChars,
    UpcaseWord,
    DowncaseWord,
    CapitalizeWord,
    Yank,
    Newline,
    Exit,
    ScrollUp,
    ScrollDown,
    SwitchMode,
}

#[derive(Debug, Clone, Deserialize)]
pub struct KeybindingRule {
    pub key: String,
    pub command: KeyAction,
}

#[derive(Debug, Clone, Default)]
pub struct Keymap {
    style: Option<String>,
    bindings: HashMap<(KeyModifiers, KeyCode), KeyAction>,
}

// ========================================================================
// Keymap Implementations
// ========================================================================

impl Keymap {
    pub fn new() -> Self {
        Self::default()
    }

    /// The configured keybindings style (`"vim"`, `"emacs"`, etc.), if specified.
    pub fn style(&self) -> Option<&str> {
        self.style.as_deref()
    }

    /// Set the keybindings style.
    pub fn set_style(&mut self, style: impl Into<String>) {
        self.style = Some(style.into());
    }

    /// Bind a key combination to an action.
    pub fn bind(&mut self, modifiers: KeyModifiers, code: KeyCode, action: KeyAction) {
        self.bindings.insert((modifiers, code), action);
    }

    /// Lookup an action for a key event.
    pub fn lookup(&self, key: &KeyEvent) -> Option<KeyAction> {
        self.bindings.get(&(key.modifiers, key.code)).copied()
    }

    /// Load keybindings from a JSON string or file content.
    pub fn parse_json(json_str: &str) -> Result<Self, String> {
        let value: serde_json::Value =
            serde_json::from_str(json_str).map_err(|e| format!("invalid keybindings JSON: {e}"))?;

        let mut keymap = Self::new();

        if value.is_array() {
            let rules: Vec<KeybindingRule> = serde_json::from_value(value)
                .map_err(|e| format!("invalid keybindings array: {e}"))?;
            for rule in rules {
                if let Some((mods, code)) = parse_key_combo(&rule.key) {
                    keymap.bind(mods, code, rule.command);
                }
            }
            return Ok(keymap);
        }

        if let Some(obj) = value.as_object() {
            if let Some(style_val) = obj
                .get("style")
                .or_else(|| obj.get("keymap"))
                .and_then(|v| v.as_str())
            {
                keymap.set_style(style_val);
            }

            if let Some(bindings_val) = obj.get("bindings") {
                if let Ok(rules) =
                    serde_json::from_value::<Vec<KeybindingRule>>(bindings_val.clone())
                {
                    for rule in rules {
                        if let Some((mods, code)) = parse_key_combo(&rule.key) {
                            keymap.bind(mods, code, rule.command);
                        }
                    }
                } else if let Ok(map) =
                    serde_json::from_value::<HashMap<String, KeyAction>>(bindings_val.clone())
                {
                    for (key, command) in map {
                        if let Some((mods, code)) = parse_key_combo(&key) {
                            keymap.bind(mods, code, command);
                        }
                    }
                }
            } else {
                // Try top-level map of {"ctrl+a": "cursor_line_start"}
                if let Ok(map) = serde_json::from_value::<HashMap<String, KeyAction>>(value) {
                    for (key, command) in map {
                        if let Some((mods, code)) = parse_key_combo(&key) {
                            keymap.bind(mods, code, command);
                        }
                    }
                }
            }
        }

        Ok(keymap)
    }

    /// Load keybindings from `keybindings.json` at `~/.summer/keybindings.json` and `.summer/keybindings.json`.
    pub fn load_from_dir(dir: &Path) -> Self {
        let file = dir.join("keybindings.json");
        if let Ok(content) = std::fs::read_to_string(&file) {
            Self::parse_json(&content).unwrap_or_default()
        } else {
            Self::default()
        }
    }
}

// ========================================================================
// Functions
// ========================================================================

/// Parse a string key combination like `"ctrl+a"`, `"alt+enter"`, `"ctrl+shift+k"`.
pub fn parse_key_combo(s: &str) -> Option<(KeyModifiers, KeyCode)> {
    let lower = s.to_lowercase();
    let parts: Vec<&str> = lower.split('+').collect();
    let mut mods = KeyModifiers::NONE;
    let mut code = None;

    for part in parts {
        match part.trim() {
            "ctrl" | "control" => mods.insert(KeyModifiers::CONTROL),
            "alt" | "option" => mods.insert(KeyModifiers::ALT),
            "shift" => mods.insert(KeyModifiers::SHIFT),
            "enter" | "return" => code = Some(KeyCode::Enter),
            "esc" | "escape" => code = Some(KeyCode::Esc),
            "backspace" => code = Some(KeyCode::Backspace),
            "tab" => code = Some(KeyCode::Tab),
            "backtab" => code = Some(KeyCode::BackTab),
            "delete" | "del" => code = Some(KeyCode::Delete),
            "home" => code = Some(KeyCode::Home),
            "end" => code = Some(KeyCode::End),
            "pageup" | "pgup" => code = Some(KeyCode::PageUp),
            "pagedown" | "pgdn" => code = Some(KeyCode::PageDown),
            "up" => code = Some(KeyCode::Up),
            "down" => code = Some(KeyCode::Down),
            "left" => code = Some(KeyCode::Left),
            "right" => code = Some(KeyCode::Right),
            c if c.len() == 1 => {
                code = Some(KeyCode::Char(c.chars().next()?));
            }
            _ => {}
        }
    }

    code.map(|c| (mods, c))
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_key_combinations() {
        let (mods, code) = parse_key_combo("ctrl+a").unwrap();
        assert_eq!(mods, KeyModifiers::CONTROL);
        assert_eq!(code, KeyCode::Char('a'));

        let (mods, code) = parse_key_combo("alt+enter").unwrap();
        assert_eq!(mods, KeyModifiers::ALT);
        assert_eq!(code, KeyCode::Enter);
    }

    #[test]
    fn parses_json_keybindings_array_and_object() {
        let json_arr = r#"[
            {"key": "ctrl+a", "command": "cursor_line_start"},
            {"key": "ctrl+e", "command": "cursor_line_end"}
        ]"#;

        let keymap = Keymap::parse_json(json_arr).unwrap();
        let key_event = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL);
        assert_eq!(keymap.lookup(&key_event), Some(KeyAction::CursorLineStart));

        let json_map = r#"{
            "ctrl+k": "kill_to_end"
        }"#;

        let keymap2 = Keymap::parse_json(json_map).unwrap();
        let key_event2 = KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL);
        assert_eq!(keymap2.lookup(&key_event2), Some(KeyAction::KillToEnd));
    }

    #[test]
    fn parses_json_with_style_and_bindings() {
        let json_full = r#"{
            "style": "vim",
            "bindings": {
                "ctrl+k": "kill_to_end"
            }
        }"#;

        let keymap = Keymap::parse_json(json_full).unwrap();
        assert_eq!(keymap.style(), Some("vim"));
        let key_event = KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL);
        assert_eq!(keymap.lookup(&key_event), Some(KeyAction::KillToEnd));
    }
}
