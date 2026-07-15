//! The quality-mechanism switches: one addressable flag per mechanism the
//! harness can run, so each can be turned off and measured.
//!
//! **A mechanism that cannot be turned off cannot be measured, and one that
//! cannot be measured cannot be deleted.** The tree carries several quality
//! mechanisms — the end-of-turn compiler check, the plan gate, the skill gate,
//! skeleton reads — and each was added because it sounded like it would help.
//! The only honest test of that is an ablation: run the task set with one
//! mechanism disabled and compare against the same runs with everything on.
//! That needs one switch per mechanism, named the same way everywhere, which is
//! what this module is.
//!
//! It lives in `tools` because the mechanisms span two crates — the engine
//! owns three gates, the `read` tool owns skeleton mode — and a switch table
//! split across those crates would drift. `ShellSpec` lives here for the same
//! reason: the shared ancestor is the one place both consumers can see.

use std::fmt;

// ========================================================================
// Data Structures
// ========================================================================

/// One quality mechanism, as the `--disable` flag names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mechanism {
    /// Asking the model to write the summary that replaces a compacted range,
    /// instead of recording the mechanical count of what was dropped.
    CompactionSummaries,
    /// The one-reminder nudge and `Stop::PlanIncomplete` that keep a turn from
    /// ending with plan steps outstanding.
    PlanGate,
    /// The `read` tool's `skeleton` mode: signatures with bodies elided, a
    /// token saving on files the model is consulting rather than editing.
    SkeletonReads,
    /// The screen that refuses an edit to a path whose configured skill the
    /// session has not loaded.
    SkillGate,
    /// The compiler check at the end of a turn that edited code, whose
    /// diagnostics are fed back for self-correction.
    VerifyGate,
}

/// Which mechanisms run. All of them by default: the loop a user gets with no
/// configuration is the loop with every mechanism on, and the ablation harness
/// is what turns them off, one at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mechanisms {
    /// Whether compaction asks the model to summarize the range it drops.
    ///
    /// **Necessary but not sufficient**, unlike its neighbours here: the engine
    /// also needs `EngineBuilder::summarize_compactions`, which is a separate
    /// question. This flag asks "does this experiment want the mechanism"; that
    /// one asks "may this frontend make an out-of-band provider call at all",
    /// which replay cannot (it answers the Nth `turn` positionally, so an extra
    /// call desynchronizes every turn after it). Both must be true.
    pub compaction_summaries: bool,
    pub plan_gate: bool,
    pub skeleton_reads: bool,
    pub skill_gate: bool,
    pub verify_gate: bool,
}

/// A `--disable` name that matches no mechanism.
#[derive(Debug, PartialEq, Eq)]
pub struct UnknownMechanism(String);

// ========================================================================
// Mechanism Implementations
// ========================================================================

impl Mechanism {
    /// Every mechanism, in the order the ablation runs them. One arm per
    /// entry, plus the all-on control, is the whole matrix.
    pub const ALL: [Mechanism; 5] = [
        Mechanism::CompactionSummaries,
        Mechanism::PlanGate,
        Mechanism::SkeletonReads,
        Mechanism::SkillGate,
        Mechanism::VerifyGate,
    ];

    /// The name the `--disable` flag and the ablation report use.
    pub fn kebab(self) -> &'static str {
        match self {
            Mechanism::CompactionSummaries => "compaction-summaries",
            Mechanism::PlanGate => "plan-gate",
            Mechanism::SkeletonReads => "skeleton-reads",
            Mechanism::SkillGate => "skill-gate",
            Mechanism::VerifyGate => "verify-gate",
        }
    }
}

impl fmt::Display for Mechanism {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.kebab())
    }
}

impl fmt::Display for UnknownMechanism {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let known: Vec<&str> = Mechanism::ALL.iter().map(|m| m.kebab()).collect();
        write!(
            f,
            "unknown mechanism `{}`, expected one of {}",
            self.0,
            known.join(", ")
        )
    }
}

impl std::str::FromStr for Mechanism {
    type Err = UnknownMechanism;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        match name {
            "compaction-summaries" => Ok(Mechanism::CompactionSummaries),
            "plan-gate" => Ok(Mechanism::PlanGate),
            "skeleton-reads" => Ok(Mechanism::SkeletonReads),
            "skill-gate" => Ok(Mechanism::SkillGate),
            "verify-gate" => Ok(Mechanism::VerifyGate),
            other => Err(UnknownMechanism(other.to_string())),
        }
    }
}

// ========================================================================
// Mechanisms Implementations
// ========================================================================

impl Default for Mechanisms {
    /// Every mechanism on. The default is the shipped loop, not the ablated
    /// one: turning something off is an experiment someone asked for.
    fn default() -> Self {
        Self {
            compaction_summaries: true,
            plan_gate: true,
            skeleton_reads: true,
            skill_gate: true,
            verify_gate: true,
        }
    }
}

impl Mechanisms {
    /// Turn one mechanism off.
    ///
    /// A method rather than a field write at the call site because the flag a
    /// name maps to is exactly the mapping an ablation arm cannot get wrong:
    /// disabling `plan-gate` while flipping `verify_gate` would run two
    /// experiments labelled as one.
    pub fn disable(&mut self, mechanism: Mechanism) {
        match mechanism {
            Mechanism::CompactionSummaries => self.compaction_summaries = false,
            Mechanism::PlanGate => self.plan_gate = false,
            Mechanism::SkeletonReads => self.skeleton_reads = false,
            Mechanism::SkillGate => self.skill_gate = false,
            Mechanism::VerifyGate => self.verify_gate = false,
        }
    }
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn test_a_name_disables_exactly_one_mechanism() {
        // The mapping an ablation arm rests on. A `disable` that flipped the
        // wrong flag runs two experiments labelled as one, and every number
        // the matrix produces after that is noise.
        let mut mechanisms = Mechanisms::default();
        mechanisms.disable(Mechanism::from_str("verify-gate").expect("a known name"));
        assert_eq!(
            mechanisms,
            Mechanisms {
                verify_gate: false,
                ..Mechanisms::default()
            }
        );
    }

    #[test]
    fn test_an_unknown_name_names_what_would_have_worked() {
        // A typo in a `--disable` list must cost the caller the list of names,
        // not a silently all-on run that reports itself as an ablation.
        let error = "compilation-gate"
            .parse::<Mechanism>()
            .expect_err("not a mechanism name");
        assert!(error.to_string().contains("verify-gate"), "{error}");
    }
}
