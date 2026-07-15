//! Provenance digests for `SessionStarted`.
//!
//! **A digest is a promise that two runs saw the same thing**, so every one of
//! these renders its input canonically rather than leaning on `Debug`. A digest
//! derived from a formatter changes when the formatter does, which turns a
//! golden-journal replay into a failure with no cause and no fix.

use policy::{Effect, ListenRule, Policy};
use sha2::{Digest, Sha256};
use tools::ToolSpec;

// ========================================================================
// Functions
// ========================================================================

pub(crate) fn sha256_hex(input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    hex_lower(&digest)
}

/// Hash the system prompt, as `SessionStarted.prompt_digest` records it.
///
/// Public because a score is not comparable across a prompt change, so
/// `bench` has to write the digest into a baseline beside the rate. It
/// calls this rather than hashing the prompt itself, because a baseline whose
/// digest is computed differently from the journal's is a digest that agrees by
/// luck.
pub fn prompt_digest(system_prompt: &str) -> String {
    sha256_hex(system_prompt)
}

/// Hash the effective policy: the merged result, not any one source file.
///
/// This is what `SessionStarted.policy_digest` records, and it is the merged
/// policy on purpose. A journal reader asking "what was this agent allowed to
/// do" wants the answer the broker actually enforced, not the four layers it was
/// assembled from. It is also what a trust-on-first-use prompt would key on if
/// the project-file tier ever moves to that model.
///
/// Rendered canonically rather than derived from `Debug`, so the digest is a
/// property of the policy and not of a formatter.
pub(crate) fn policy_digest(policy: &Policy) -> String {
    let mut canonical = String::new();
    canonical.push_str("root\x00");
    canonical.push_str(&policy.root.display().to_string());
    canonical.push('\n');

    for rule in &policy.paths {
        canonical.push_str("path\x00");
        canonical.push_str(effect_name(rule.effect));
        canonical.push('\x00');
        canonical.push_str(&rule.pattern);
        canonical.push('\n');
    }
    for rule in &policy.commands {
        canonical.push_str("command\x00");
        canonical.push_str(effect_name(rule.effect));
        canonical.push('\x00');
        canonical.push_str(&rule.program);
        canonical.push('\n');
    }
    for rule in &policy.network.connect {
        canonical.push_str("connect\x00");
        canonical.push_str(effect_name(rule.effect));
        canonical.push('\x00');
        canonical.push_str(&rule.host);
        canonical.push('\x00');
        canonical.push_str(&rule.port.map_or_else(|| "*".to_string(), |p| p.to_string()));
        canonical.push('\n');
    }
    canonical.push_str("listen\x00");
    canonical.push_str(&listen_name(&policy.network.listen));
    canonical.push('\n');

    sha256_hex(&canonical)
}

pub(crate) fn effect_name(effect: Effect) -> &'static str {
    match effect {
        Effect::Allow => "allow",
        Effect::Ask => "ask",
        Effect::Deny => "deny",
    }
}

pub(crate) fn listen_name(rule: &ListenRule) -> String {
    match rule {
        ListenRule::Any { ports } => format!("any:{}", render_ports(ports)),
        ListenRule::Deny => "deny".to_string(),
        ListenRule::Loopback { ports } => format!("loopback:{}", render_ports(ports)),
    }
}

pub(crate) fn render_ports(ports: &[u16]) -> String {
    if ports.is_empty() {
        return "*".to_string();
    }
    ports
        .iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

/// Hash the tool set: names, descriptions, and argument schemas, sorted by name.
///
/// Public for the same reason as [`prompt_digest`]: a tool description is what
/// the model routes on, so a rate measured before a description change and one
/// measured after are two different questions.
pub fn tools_digest(specs: Vec<&ToolSpec>) -> String {
    let mut specs = specs;
    specs.sort_by_key(|spec| spec.name.as_str());
    let mut canonical = String::new();
    for spec in specs {
        canonical.push_str(&spec.name);
        canonical.push('\x00');
        canonical.push_str(&spec.description);
        canonical.push('\x00');
        canonical.push_str(&spec.input_schema.to_string());
        canonical.push('\n');
    }
    sha256_hex(&canonical)
}

pub(crate) fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}
