//! The TUI permission prompter.
//!
//! The permission prompt does not flow over the [`UiEvent`](engine::UiEvent) channel: that
//! channel is one-directional narration, and a prompt is a request that needs an
//! answer. It flows through the existing [`Prompter`] seam, which is already
//! `async` and already lives on the broker for exactly this reason.
//!
//! `TerminalPrompter` reads stdin on a blocking thread, which cannot coexist
//! with a raw-mode UI that owns the keyboard. So on the TUI path this replaces
//! it: the prompt is sent to the driver as a [`PromptRequest`], the driver
//! renders it inline and captures the numbered answer, and the answer rides back on
//! the oneshot. `TerminalPrompter` stays for `--plain` and non-TTY.
//!
//! No broker or policy code changes: the seam was designed for this.

use async_trait::async_trait;
use policy::{Request, RuleSpec, Scope};
use sandbox::{Grant, Prompter, WritePreview};
use tokio::sync::{mpsc, oneshot};

use crate::permission::{describe, render_rule, rule_for};

// ========================================================================
// Data Structures
// ========================================================================

/// A prompt the broker wants answered, addressed to the UI task.
pub struct PromptRequest {
    /// What the agent wants, in a sentence (the CLI's `describe`).
    pub description: String,
    /// What a write would change, as a [`tools::hunk_diff`]; `None` for every
    /// other request, and for a write the broker could not preview.
    pub diff: Option<String>,
    /// The rule-scope offer, rendered for display (the "always allow" option).
    pub offer: Option<String>,
    /// The rule itself, so the driver can build a `Rule` grant on `a`. Absent
    /// when the request does not generalize (then `a` is not offered).
    pub rule: Option<RuleSpec>,
    /// Where the answer goes. The driver sends a [`Grant`] here; a dropped sender
    /// means the UI went away, which the prompter reads as a denial.
    pub reply: oneshot::Sender<Grant>,
}

/// Prompts on the TUI, asking the UI task through a channel rather than reading
/// stdin. A blocking stdin read cannot coexist with a raw-mode UI.
pub struct TuiPrompter {
    requests: mpsc::UnboundedSender<PromptRequest>,
}

// ========================================================================
// TuiPrompter Implementations
// ========================================================================

impl TuiPrompter {
    pub fn new() -> (Self, mpsc::UnboundedReceiver<PromptRequest>) {
        let (requests, rx) = mpsc::unbounded_channel();
        (Self { requests }, rx)
    }

    /// Send `request` to the UI task, with `diff` shown under the question.
    async fn ask_with_diff(&self, request: &Request, diff: Option<String>) -> Grant {
        let rule = rule_for(request);
        let (reply_tx, reply_rx) = oneshot::channel();
        let prompt = PromptRequest {
            description: describe(request),
            diff,
            offer: rule.as_ref().map(render_rule),
            rule,
            reply: reply_tx,
        };
        // A send that fails means the UI is gone, which is not the tool's problem
        // to wait on; deny and let the model see the refusal.
        if self.requests.send(prompt).is_err() {
            return Grant::once(false);
        }
        // A dropped reply (UI exited mid-prompt) is likewise a denial: the safe
        // direction when nobody is there to answer.
        reply_rx.await.unwrap_or_else(|_| Grant::once(false))
    }
}

#[async_trait]
impl Prompter for TuiPrompter {
    async fn ask(&self, request: &Request) -> Grant {
        self.ask_with_diff(request, None).await
    }

    async fn ask_previewed(&self, request: &Request, preview: &WritePreview) -> Grant {
        let diff = tools::hunk_diff(&preview.before, &preview.after);
        self.ask_with_diff(request, Some(diff)).await
    }
}

/// Build the [`Grant`] for a one-key answer. `AllowRule` falls back to a
/// once-grant if no rule generalized, matching `TerminalPrompter`.
pub fn grant_for(answer: Answer, rule: Option<RuleSpec>) -> Grant {
    match answer {
        Answer::Allow => Grant::once(true),
        Answer::AllowRule => match rule {
            Some(rule) => Grant {
                allow: true,
                scope: Scope::Rule(rule),
            },
            None => Grant::once(true),
        },
        Answer::Deny => Grant::once(false),
    }
}

/// A one-key answer to a prompt. The UI sends this; the driver turns it into a
/// [`Grant`] because it holds the rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    Allow,
    AllowRule,
    Deny,
}

/// Map a key character to an answer: the number printed beside each option.
///
/// **Digits, not letters.** The prompt can appear while the user is typing
/// their next message, and `y`, `a`, `n` and `d` are in most sentences: "and
/// also" used to grant a remembered rule. A digit is rarely mid-word, and is
/// what Claude Code's prompts take for the same reason. Anything else is
/// `None`, so the prompt keeps waiting.
pub fn answer_for_key(ch: char, offer: Option<&str>) -> Option<Answer> {
    let index = ch.to_digit(10)?.checked_sub(1)? as usize;
    (index < prompt_options(offer).len()).then(|| answer_for_selection(index, offer))
}

/// Map a selected bullet index to its corresponding Answer.
pub fn answer_for_selection(selected: usize, offer: Option<&str>) -> Answer {
    match selected {
        0 => Answer::Allow,
        1 if offer.is_some() => Answer::AllowRule,
        _ => Answer::Deny,
    }
}

/// Return the list of bullet choices `(shortcut, label)` available for a prompt.
pub fn prompt_options(offer: Option<&str>) -> Vec<(&'static str, String)> {
    if let Some(rule) = offer {
        vec![
            ("1", "Allow once".to_string()),
            ("2", format!("Always allow: {rule}")),
            ("3", "Deny".to_string()),
        ]
    } else {
        vec![("1", "Allow once".to_string()), ("2", "Deny".to_string())]
    }
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use policy::PathAccess;

    fn write_rule() -> RuleSpec {
        RuleSpec::Path {
            access: PathAccess::Write,
            pattern: "/repo/src/**".into(),
        }
    }

    #[test]
    fn allow_rule_uses_the_rule_scope() {
        let grant = grant_for(Answer::AllowRule, Some(write_rule()));
        assert!(grant.allow);
        assert!(matches!(grant.scope, Scope::Rule(_)));
    }

    #[test]
    fn allow_rule_without_a_rule_falls_back_to_once() {
        let grant = grant_for(Answer::AllowRule, None);
        assert!(grant.allow);
        assert!(matches!(grant.scope, Scope::Once));
    }

    #[test]
    fn letters_typed_mid_sentence_answer_nothing() {
        // The bug: typing "and also" while a prompt appeared granted a rule.
        for ch in ['y', 'a', 'n', 'd', 'Y'] {
            assert_eq!(answer_for_key(ch, Some("write /repo/src/**")), None, "{ch}");
        }
    }

    #[test]
    fn each_number_answers_the_option_printed_beside_it() {
        assert_eq!(answer_for_key('1', None), Some(Answer::Allow));
        assert_eq!(answer_for_key('2', None), Some(Answer::Deny));
        assert_eq!(answer_for_key('3', None), None);
        let offer = Some("write /repo/src/**");
        assert_eq!(answer_for_key('2', offer), Some(Answer::AllowRule));
        assert_eq!(answer_for_key('3', offer), Some(Answer::Deny));
        assert_eq!(answer_for_key('0', offer), None);
    }

    #[test]
    fn prompt_options_and_selection_mapping() {
        let opts_no_rule = prompt_options(None);
        assert_eq!(opts_no_rule.len(), 2);
        assert_eq!(opts_no_rule[0].0, "1");
        assert_eq!(opts_no_rule[1].0, "2");

        assert_eq!(answer_for_selection(0, None), Answer::Allow);
        assert_eq!(answer_for_selection(1, None), Answer::Deny);

        let opts_with_rule = prompt_options(Some("run touch file"));
        assert_eq!(opts_with_rule.len(), 3);
        assert_eq!(opts_with_rule[0].0, "1");
        assert_eq!(opts_with_rule[1].0, "2");
        assert_eq!(opts_with_rule[2].0, "3");

        assert_eq!(answer_for_selection(0, Some("rule")), Answer::Allow);
        assert_eq!(answer_for_selection(1, Some("rule")), Answer::AllowRule);
        assert_eq!(answer_for_selection(2, Some("rule")), Answer::Deny);
    }

    #[tokio::test]
    async fn a_shell_call_prompts_through_the_channel_and_runs_when_allowed() {
        // The T2 gate, headlessly: a `shell` call is not on the built-in allow
        // list, so the broker asks. The TUI path resolves that ask through this
        // prompter's channel rather than blocking stdin, and answering `y`
        // (Allow) lets the tool run. No terminal, no stdin.
        use std::sync::Arc;
        use std::sync::mpsc::channel;

        use engine::{BrokerBuilder, EngineBuilder, Limits, Observer, UiEvent};
        use journal::{CallId, StopReason, Usage, WriterSink};
        use policy::Policy;
        use provider::{MockProvider, Provider, ScriptedTurn, StreamEvent, ToolCallStart, TurnEnd};
        use sandbox::NoneBackend;
        use tokio::sync::mpsc;
        use tools::{Builtin, Dispatcher, ToolSource};

        let root = std::env::temp_dir().join(format!("summer-tui-prompt-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("root");
        let marker = root.join("ran.marker");
        let _ = std::fs::remove_file(&marker);

        let (prompter, mut prompt_rx) = TuiPrompter::new();
        let (decisions, permissions) = channel();
        let broker = BrokerBuilder::new(Policy::builtin(root.clone()))
            .backend(Box::new(NoneBackend::new("test")))
            .prompter(Box::new(prompter))
            .decisions(decisions)
            .build(true)
            .expect("none backend clears the startup check under allow_unenforced");

        let shell_call = vec![
            StreamEvent::ToolCallStart(ToolCallStart {
                call: CallId::new("toolu_1".into()),
                name: "shell".into(),
            }),
            StreamEvent::ToolCallDelta(format!(r#"{{"command":"touch {}"}}"#, marker.display())),
            StreamEvent::BlockEnd,
            StreamEvent::TurnEnd(TurnEnd {
                stop: StopReason::ToolUse,
                usage: Usage::zero(),
            }),
        ];
        let done = vec![
            StreamEvent::TextDelta("done".into()),
            StreamEvent::BlockEnd,
            StreamEvent::TurnEnd(TurnEnd {
                stop: StopReason::EndTurn,
                usage: Usage::zero(),
            }),
        ];
        let provider: Box<dyn Provider> = Box::new(MockProvider::new(vec![
            ScriptedTurn { events: shell_call },
            ScriptedTurn { events: done },
        ]));

        let (tx, _rx): (Observer, mpsc::UnboundedReceiver<UiEvent>) = mpsc::unbounded_channel();
        let mut engine = EngineBuilder::new(
            root.clone(),
            "system".into(),
            provider,
            Dispatcher::new(Builtin::new().tools()).expect("unique names"),
            Arc::new(broker),
            Box::new(WriterSink::new(std::io::sink())),
            permissions,
        )
        .limits(Limits::defaults())
        .observer(tx)
        .build()
        .expect("engine builds");

        // Run the turn on a task; the prompt arrives on the channel while it runs.
        let handle = tokio::spawn(async move { engine.turn("touch the marker".into()).await });

        // Answer the prompt: Allow once. A closed channel (no answer) would deny.
        let request = prompt_rx
            .recv()
            .await
            .expect("the broker asked for permission");
        assert!(
            request.description.contains("run"),
            "the prompt describes the ask: {}",
            request.description
        );
        let _ = request.reply.send(Grant::once(true));

        let summary = handle
            .await
            .expect("the task joined")
            .expect("the turn completed once the prompt was answered");
        assert_eq!(summary.tool_calls, 1);
        assert!(
            marker.exists(),
            "the shell ran after the prompt was answered `y` from the channel"
        );
        let _ = std::fs::remove_file(&marker);
    }
}
