//! `bench`: run a checked-in task set against the live model and report a
//! rate with its interval.
//!
//! **This binary exists because the library could not be invoked.** The scoring
//! statistics, the per-run independence, and the failure taxonomy were all in
//! place with no way to reach them, so eight exit criteria that read "needs a
//! real key" had nothing that could produce the journals they wait on. What it
//! produces is the evidence: a rate, and a directory of journals from real runs.
//!
//! Two properties are worth stating before the code, because both look like
//! omissions:
//!
//! **The bench never enters the OS sandbox.** A Landlock domain is per process
//! and each run has a different root, so one process cannot enter N of them;
//! entering the first run's root would then deny every later run. What bounds a
//! bench run is the broker's userspace check, exactly as in a session without
//! `--sandbox`, and the run happens in a throwaway copy of the fixture for that
//! reason. Do not point this at a tree you care about.
//!
//! **Prompts are denied, not auto-approved.** A bench has no user, and a
//! prompter that answered "yes" would measure an agent nobody is supervising
//! while reporting a rate for the one they are. A task needing more than the
//! built-in allow list declares it in `commands`, in the diff.

// A harness: it reads task sets, writes baselines, and shells out for the commit
// sha. Operator-driven work, not the model's tool calls.
#![allow(clippy::disallowed_methods)]

use std::collections::BTreeMap;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::mpsc::channel;

use async_trait::async_trait;
use bench::{
    ALL_ON, ArmReport, Baseline, Metrics, RunSetup, Score, SkillRequirement, Task, TaskScore,
    TaskSet, Trace, agreement, diff, render_matrix, render_run, run_task,
};
use engine::{
    BrokerBuilder, Engine, EngineBuilder, Grant, Limits, Mechanism, Mechanisms, Prompter,
    SYSTEM_PROMPT, SkillGate, for_platform, prompt_digest, tools_digest,
};
use journal::WriterSink;
use policy::{CommandRule, Effect, Policy, Request};
use provider::{ProviderConfig, ProviderId, UnknownProviderId, transport};
use tools::{Builtin, Dispatcher, ShellSpec, SkillLookup, ToolSource};

// ========================================================================
// Constants
// ========================================================================

/// Runs per task when `--runs` is not given.
///
/// Five is enough to tell a task that always fails from one that always passes,
/// and not enough to distinguish 60% from 80%: the interval printed beside every
/// rate is what keeps that honest rather than a larger default nobody can afford.
const DEFAULT_RUNS: u32 = 5;

const BASELINE_FLAG: &str = "--baseline";
const DISABLE_FLAG: &str = "--disable";
const MODEL_FLAG: &str = "--model";
const PROVIDER_FLAG: &str = "--provider";
const RUNS_FLAG: &str = "--runs";
const SCRATCH_FLAG: &str = "--scratch";
const WRITE_BASELINE_FLAG: &str = "--write-baseline";

/// Compare two directories of recorded session journals instead of running
/// tasks. See [`replay_mode`].
const REPLAY_FLAG: &str = "--replay";

/// The recording a `--replay` directory is compared against.
const AGAINST_FLAG: &str = "--against";

const USAGE: &str = "usage: bench [ablate] <task-set.json> [--runs N] [--baseline PATH] \
                     [--scratch DIR] [--write-baseline]\n              \
                     [--provider ID] [--model NAME] [--disable a,b,...]\n       \
                     bench --replay <journal-dir> [--against <journal-dir>]";

/// The subcommand that runs the whole ablation matrix: the all-on control plus
/// one arm per mechanism with that mechanism disabled.
const ABLATE_SUBCOMMAND: &str = "ablate";

// ========================================================================
// Data Structures
// ========================================================================

/// What the command line asked for.
struct Args {
    /// Run the ablation matrix rather than one scored run.
    ablate: bool,
    /// The recording a `--replay` corpus is compared against.
    against: Option<PathBuf>,
    baseline: Option<PathBuf>,
    /// Mechanisms to disable, as `--disable plan-gate,verify-gate`.
    disabled: Vec<Mechanism>,
    /// Overrides the provider profile's default model.
    model: Option<String>,
    /// Which provider to score. Defaults to Anthropic, which is what every
    /// baseline recorded before this flag existed was measured against.
    provider: ProviderId,
    /// A directory of session journals to read as traces, instead of a task set.
    replay: Option<PathBuf>,
    runs: u32,
    scratch: PathBuf,
    /// `None` in replay mode, which needs no tasks.
    task_set: Option<PathBuf>,
    write_baseline: bool,
}

/// Builds one engine per run against the live model.
struct LiveSetup {
    /// Programs allowed on top of the built-in policy, from the task set.
    commands: Vec<String>,
    /// Where per-run journals land. The point of the exercise: a rate says an
    /// agent failed, a journal says what it did.
    journals: PathBuf,
    /// Which quality mechanisms the engines run. The default is the shipped
    /// loop; `--disable` and the `ablate` subcommand are what construct
    /// anything else.
    mechanisms: Mechanisms,
    /// Resolved once at startup and cloned per run, so a credential problem is
    /// one message before the first task rather than one panic per run.
    provider_config: ProviderConfig,
    /// Which transport `provider_config` is for. The config carries the base
    /// URL and headers; which client speaks to them is still a choice, and
    /// Copilot's is a different client rather than a different config.
    provider_id: ProviderId,
    shell: ShellSpec,
}

/// The prompter for an unattended run: deny, and say so.
///
/// `sandbox::AutoDeny` would do the same thing silently. The difference
/// matters here because a denial and a wrong answer are different failures with
/// the same score, and the line this prints is the only place they separate.
struct Unattended;

// ========================================================================
// Functions
// ========================================================================

/// Run the ablation matrix: the all-on control, then one arm per mechanism
/// with that mechanism alone disabled, every arm over the same task set.
///
/// **One mechanism per arm, never several.** An arm disabling two mechanisms
/// measures their interaction and answers no question a deletion decision can
/// use.
///
/// **Task-major, arm-inner, because provider conditions drift over time.** The
/// first matrix this ran was arm-major, and the control arm went first into a
/// twenty-minute rate-limit window: every one of its first six runs errored,
/// every later arm sailed, and the table read as though all four mechanisms
/// helped. Blocking by task puts every arm's run of a task adjacent in time, so
/// a burst of provider noise lands on the block rather than on one arm.
fn ablate_mode(
    runtime: &tokio::runtime::Runtime,
    base: &LiveSetup,
    set: &TaskSet,
    runs: u32,
    scratch: &Path,
) -> Result<(), Box<dyn Error>> {
    let root = scratch.join("ablate");
    let mut arms = vec![(ALL_ON.to_string(), Mechanisms::default())];
    for mechanism in Mechanism::ALL {
        let mut mechanisms = Mechanisms::default();
        mechanisms.disable(mechanism);
        arms.push((format!("-{}", mechanism.kebab()), mechanisms));
    }

    // One setup per arm, built once so every task's block runs the same
    // engines an arm-major order would have.
    let setups: Vec<(String, (LiveSetup, PathBuf))> = arms
        .into_iter()
        .map(|(label, mechanisms)| {
            let arm_dir = root.join(label.trim_start_matches('-'));
            let setup = LiveSetup {
                commands: base.commands.clone(),
                journals: arm_dir.join("journals"),
                mechanisms,
                provider_config: base.provider_config.clone(),
                provider_id: base.provider_id,
                shell: base.shell.clone(),
            };
            std::fs::create_dir_all(&setup.journals)?;
            Ok((label, (setup, arm_dir)))
        })
        .collect::<Result<Vec<_>, Box<dyn Error>>>()?;

    let mut reports: Vec<ArmReport> = setups
        .iter()
        .map(|(label, _)| ArmReport {
            label: label.clone(),
            reports: Vec::with_capacity(set.tasks.len()),
        })
        .collect();
    for task in &set.tasks {
        for (index, (label, (setup, arm_dir))) in setups.iter().enumerate() {
            eprintln!("bench [{label}]: {} x{}", task.name, runs);
            let report = runtime.block_on(run_task(setup, task, runs, arm_dir))?;
            reports[index].reports.push(report);
        }
    }

    print!("{}", render_matrix(&reports));
    eprintln!("[journals: {}]", root.display());
    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    let args = Args::parse(std::env::args().skip(1))?;

    if let Some(dir) = args.replay.clone() {
        return replay_mode(&dir, args.against.as_deref());
    }
    let task_set = args.task_set.clone().ok_or(USAGE)?;

    let base = task_set
        .parent()
        .ok_or("the task set has no parent directory")?
        .to_path_buf();
    let source = std::fs::read_to_string(&task_set)
        .map_err(|error| format!("{}: {error}", task_set.display()))?;
    let set = TaskSet::load(&source, &base)
        .map_err(|error| format!("{}: {error}", task_set.display()))?;

    // Resolved once, before any run, so a missing credential fails in a sentence
    // rather than N identical panics from inside the runner.
    let provider_config = credential(args.provider, args.model.as_deref())?;
    let model = provider_config.model.clone();
    let mut mechanisms = Mechanisms::default();
    for mechanism in &args.disabled {
        mechanisms.disable(*mechanism);
    }
    let journals = args.scratch.join("journals");
    std::fs::create_dir_all(&journals)?;

    let setup = LiveSetup {
        commands: set.commands.clone(),
        journals,
        mechanisms,
        provider_config,
        provider_id: args.provider,
        shell: ShellSpec::platform_default(),
    };

    let runtime = tokio::runtime::Runtime::new()?;
    if args.ablate {
        return ablate_mode(&runtime, &setup, &set, args.runs, &args.scratch);
    }

    let mut reports = Vec::with_capacity(set.tasks.len());
    for task in &set.tasks {
        eprintln!("bench: {} x{}", task.name, args.runs);
        reports.push(runtime.block_on(run_task(&setup, task, args.runs, &args.scratch))?);
    }

    let current = Baseline {
        commit: commit_sha(),
        model,
        prompt_digest: prompt_digest(SYSTEM_PROMPT),
        task_set: set.name.clone(),
        tasks: reports
            .iter()
            .map(|report| {
                let score = report.score();
                TaskScore {
                    metrics: Metrics::medians(&report.metrics()),
                    n: score.n,
                    name: report.task.clone(),
                    successes: score.successes,
                }
            })
            .collect(),
        tools_digest: tools_digest(setup.base_dispatcher()?.specs()),
    };

    let baseline_path = args
        .baseline
        .clone()
        .unwrap_or_else(|| default_baseline_path(&base, &set.name));
    // Absent is the ordinary case the first time a set is run, and it is not an
    // error: the report says "no baseline entry" per task and the run is still
    // worth having.
    let previous = match std::fs::read_to_string(&baseline_path) {
        Ok(source) => Some(Baseline::load(&source)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("{}: {error}", baseline_path.display()).into()),
    };

    print!("{}", render_run(&current, previous.as_ref(), &reports).text);
    eprintln!("[journals: {}]", setup.journals.display());

    if args.write_baseline {
        if let Some(parent) = baseline_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&baseline_path, current.to_json())?;
        eprintln!("[baseline written: {}]", baseline_path.display());
    }
    Ok(())
}

/// The commit the agent under test was built from.
///
/// A baseline without it cannot answer the only question anyone asks of an old
/// score, so a failure here becomes the literal string `unknown` rather than a
/// silently absent field: "we did not record it" is itself worth recording.
fn commit_sha() -> String {
    Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map_or_else(|| "unknown".to_string(), |sha| sha.trim().to_string())
}

/// `tasks/<set>.json` sits beside `baselines/<set>.json`, one level up.
fn default_baseline_path(task_set_dir: &Path, set: &str) -> PathBuf {
    task_set_dir
        .parent()
        .unwrap_or(task_set_dir)
        .join("baselines")
        .join(format!("{set}.json"))
}

// ========================================================================
// Args Implementations
// ========================================================================

impl Args {
    fn parse(args: impl Iterator<Item = String>) -> Result<Self, Box<dyn Error>> {
        let mut args = args.peekable();
        // The one subcommand-shaped entry point, checked before the flag loop so
        // `bench ablate tasks/starter.json --runs 3` reads the way it runs.
        let ablate = args.peek().is_some_and(|arg| arg == ABLATE_SUBCOMMAND);
        if ablate {
            args.next();
        }

        let mut task_set = None;
        let mut against = None;
        let mut replay = None;
        let mut baseline = None;
        let mut disabled = Vec::new();
        let mut model = None;
        let mut provider = ProviderId::Anthropic;
        let mut runs = DEFAULT_RUNS;
        let mut scratch = PathBuf::from("target/bench");
        let mut write_baseline = false;

        while let Some(arg) = args.next() {
            match arg.as_str() {
                AGAINST_FLAG => against = Some(PathBuf::from(value(&mut args, AGAINST_FLAG)?)),
                REPLAY_FLAG => replay = Some(PathBuf::from(value(&mut args, REPLAY_FLAG)?)),
                BASELINE_FLAG => baseline = Some(PathBuf::from(value(&mut args, BASELINE_FLAG)?)),
                DISABLE_FLAG => disabled = mechanisms(&value(&mut args, DISABLE_FLAG)?)?,
                MODEL_FLAG => model = Some(value(&mut args, MODEL_FLAG)?),
                // `to_string` rather than `?`: `main` renders a boxed error
                // with `Debug`, and this one's `Debug` is a tuple struct while
                // its `Display` lists the ids that would have worked.
                PROVIDER_FLAG => {
                    provider = value(&mut args, PROVIDER_FLAG)?
                        .parse()
                        .map_err(|error: UnknownProviderId| error.to_string())?;
                }
                RUNS_FLAG => runs = value(&mut args, RUNS_FLAG)?.parse()?,
                SCRATCH_FLAG => scratch = PathBuf::from(value(&mut args, SCRATCH_FLAG)?),
                WRITE_BASELINE_FLAG => write_baseline = true,
                _ if arg.starts_with('-') => return Err(format!("{USAGE}\nunknown: {arg}").into()),
                _ => task_set = Some(PathBuf::from(arg)),
            }
        }

        // Replay mode needs no task set; task mode does. Checked here rather
        // than at use so a missing argument is one message, not a panic three
        // calls deep.
        if replay.is_none() && task_set.is_none() {
            return Err(USAGE.into());
        }
        // An ablation disables mechanisms itself, one arm at a time, so a
        // `--disable` beside it would describe a matrix that never runs.
        if ablate && !disabled.is_empty() {
            return Err(format!(
                "{USAGE}\nablate picks its own mechanisms; --disable is for one scored run"
            )
            .into());
        }
        Ok(Self {
            ablate,
            against,
            baseline,
            disabled,
            model,
            provider,
            replay,
            runs,
            scratch,
            task_set,
            write_baseline,
        })
    }
}

/// Parse a comma-separated `--disable` list.
///
/// Rejected here, before any run, for the reason a bad credential is: a typo
/// that surfaces after a paid run has started has cost the round trip it was
/// supposed to prevent.
fn mechanisms(list: &str) -> Result<Vec<Mechanism>, Box<dyn Error>> {
    list.split(',')
        .filter(|name| !name.is_empty())
        .map(|name| {
            name.parse()
                .map_err(|error| format!("{DISABLE_FLAG}: {error}").into())
        })
        .collect()
}

/// Read a directory of session journals as traces, and optionally compare it
/// against another such directory.
///
/// **This is what makes a session a test.** With one directory it prints what
/// each recorded session was asked and what it did, which is the corpus. With
/// `--against` it pairs journals by filename and reports, per pair, how much of
/// the recorded work the other run reproduced, plus what each did that the other
/// did not. Recording the same prompts under model A and model B and comparing
/// the two directories is a model evaluation that needs no judge and no
/// hand-written assertions.
fn replay_mode(dir: &Path, against: Option<&Path>) -> Result<(), Box<dyn Error>> {
    let recorded = traces_in(dir)?;
    if recorded.is_empty() {
        return Err(format!("{}: no readable journals", dir.display()).into());
    }

    let Some(against) = against else {
        for (name, trace) in &recorded {
            println!("{name}");
            println!("  prompt: {}", first_line(&trace.prompt));
            println!("  steps:  {}", trace.steps.join(" -> "));
            println!(
                "  ended:  {}",
                if trace.completed {
                    "answered"
                } else {
                    "did not finish"
                }
            );
        }
        return Ok(());
    };

    let replayed = traces_in(against)?;
    let mut total = Score { successes: 0, n: 0 };
    for (name, recorded) in &recorded {
        let Some(other) = replayed.get(name) else {
            println!("{name}: no counterpart in {}", against.display());
            continue;
        };
        let score = agreement(recorded, other);
        let divergence = diff(recorded, other);
        total.successes += score.successes;
        total.n += score.n;

        let (low, high) = score.interval();
        println!(
            "{name}: agreement {:.2} (95% CI {low:.2}-{high:.2}), {}/{} steps",
            score.rate(),
            score.successes,
            score.n
        );
        if !divergence.missing.is_empty() {
            println!("  not reproduced: {}", divergence.missing.join(", "));
        }
        if !divergence.extra.is_empty() {
            println!("  additional:     {}", divergence.extra.join(", "));
        }
    }

    let (low, high) = total.interval();
    println!(
        "\noverall: {:.2} (95% CI {low:.2}-{high:.2}) over {} steps",
        total.rate(),
        total.n
    );
    Ok(())
}

/// Every journal in `dir`, keyed by file name.
fn traces_in(dir: &Path) -> Result<BTreeMap<String, Trace>, Box<dyn Error>> {
    let mut traces = BTreeMap::new();
    for entry in std::fs::read_dir(dir).map_err(|error| format!("{}: {error}", dir.display()))? {
        let path = entry?.path();
        if path.extension().is_none_or(|ext| ext != "jsonl") {
            continue;
        }
        let Some(trace) = Trace::from_journal(&path) else {
            eprintln!("bench: {} could not be read as a journal", path.display());
            continue;
        };
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        traces.insert(name, trace);
    }
    Ok(traces)
}

/// A prompt's first line, so a multi-line task does not flood the listing.
fn first_line(prompt: &str) -> &str {
    prompt.lines().next().unwrap_or("")
}

fn value(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, Box<dyn Error>> {
    args.next()
        .ok_or_else(|| format!("{flag} needs a value").into())
}

// ========================================================================
// LiveSetup Implementations
// ========================================================================

impl LiveSetup {
    /// The tool set, which is also what `tools_digest` is taken over.
    ///
    /// Built from the same call the engines use, so the digest in a baseline
    /// describes the tools the runs actually had rather than a second list that
    /// happens to agree today. The digest is over the *base* set — no task's
    /// skill tool — because a per-task tool would make the digest a function of
    /// whichever task happened to be asked about.
    fn base_dispatcher(&self) -> Result<Dispatcher, Box<dyn Error>> {
        Ok(Dispatcher::new(
            Builtin::with_mechanisms(self.mechanisms, self.shell.clone()).tools(),
        )?)
    }

    /// The tool set for `task`, which is the base set plus the `skill` tool a
    /// task that declares skill rules needs — the model cannot load what it is
    /// not offered, and a gate that names a skill with no tool to load it is a
    /// gate no run can ever satisfy.
    fn dispatcher(&self, task: &Task, repo: &Path) -> Result<Dispatcher, Box<dyn Error>> {
        let builtin = Builtin::with_mechanisms(self.mechanisms, self.shell.clone());
        if task.skills.is_empty() {
            return Ok(Dispatcher::new(builtin.tools())?);
        }
        // The run repo, not the fixture: the registry is what the `skill` tool
        // answers from, and each run's copy is the tree the engine is rooted
        // at.
        Ok(Dispatcher::new(
            builtin
                .with_skill(Arc::new(FixtureSkills::load(repo)))
                .tools(),
        )?)
    }

    /// The built-in policy for `repo`, widened by the task set's declared
    /// programs.
    fn policy(&self, repo: &Path) -> Policy {
        let mut policy = Policy::builtin(repo.to_path_buf());
        policy
            .commands
            .extend(self.commands.iter().map(|program| CommandRule {
                effect: Effect::Allow,
                program: program.clone(),
            }));
        policy
    }
}

impl RunSetup for LiveSetup {
    fn journal(&self, repo: &Path) -> Option<PathBuf> {
        Some(self.journals.join(format!("{}.jsonl", run_name(repo))))
    }

    fn build(&self, task: &Task, repo: &Path) -> Engine {
        // Every failure below is a setup failure, not a run failure, and the
        // trait returns an `Engine` because a run that could not be set up is
        // not a data point about the agent. Panicking keeps it out of the score
        // instead of scoring it as a loss.
        let (decisions, permissions) = channel();
        let broker = BrokerBuilder::new(self.policy(repo))
            .backend(for_platform())
            .prompter(Box::new(Unattended))
            .decisions(decisions)
            // The bench never calls `enter`; see the module docs. Asking for the
            // unenforced build is how that stays a stated choice rather than an
            // omission nobody notices.
            .build(true)
            .expect("the built-in policy is enforceable");

        let journal = std::fs::File::create(
            self.journal(repo)
                .expect("LiveSetup names a journal for every run"),
        )
        .expect("a journal file in the scratch directory");

        // A task that declares skill rules gets them enforced, with the skill
        // bodies from its own fixture. The catalog is appended either way, so
        // the only difference an ablation arm makes is enforcement: the gate
        // versus the advisory catalog is exactly the mechanism's claim.
        let mut skills = SkillGate::default();
        let mut system_prompt = SYSTEM_PROMPT.to_string();
        if !task.skills.is_empty() {
            let registry = FixtureSkills::load(repo);
            system_prompt = format!("{system_prompt}\n\n{}", registry.catalog());
            for rule in &task.skills {
                if registry.lookup(&rule.skill).is_none() {
                    eprintln!(
                        "bench: task `{}` requires skill `{}` but its fixture has no \
                         .summer/skills/{}/SKILL.md",
                        task.name, rule.skill, rule.skill
                    );
                    continue;
                }
                rule_compile(&mut skills, rule);
            }
        }

        EngineBuilder::new(
            repo.to_path_buf(),
            system_prompt,
            transport(self.provider_id, self.provider_config.clone()),
            self.dispatcher(task, repo)
                .expect("the built-in tools are distinct"),
            Arc::new(broker),
            Box::new(WriterSink::new(journal)),
            permissions,
        )
        .limits(Limits::defaults())
        .mechanisms(self.mechanisms)
        // Permitted, so the `compaction-summaries` arm has something to vary:
        // the engine needs this *and* the mechanism, and with it left off the
        // arm would measure a mechanism that was never running in the control
        // either. Safe because a scored run drives a live provider; `--replay`
        // returns before any engine is built, and never uses `Recorded` here.
        .summarize_compactions()
        .skill_gate(skills)
        .build()
        .expect("an engine records its SessionStarted")
    }
}

/// Attach one declared rule, saying so rather than panicking when its pattern
/// does not parse: a task set naming an invalid glob is a set-authoring bug the
/// operator should read in the diff, not a run-ender.
fn rule_compile(skills: &mut SkillGate, rule: &SkillRequirement) {
    if let Err(error) = skills.require(&rule.paths, rule.skill.clone()) {
        eprintln!(
            "bench: ignoring skill rule for `{}`: its paths pattern is not a valid \
             glob: {error}",
            rule.skill
        );
    }
}

/// The credential for `provider`, from the environment.
///
/// **The environment only, unlike the CLI**, which also reads the credential a
/// `/login` stored in `~/.summer`. A bench run is a scored experiment, and one
/// that silently picked up an ambient stored credential would make "which key
/// produced this rate" a question about the operator's home directory. The
/// error names the variables to set, because a bench that fails on a missing
/// credential without saying which one costs a round trip to the registry.
fn credential(provider: ProviderId, model: Option<&str>) -> Result<ProviderConfig, String> {
    let profile = provider.profile();
    let auth = profile.credential_from_env().ok_or_else(|| {
        let names: Vec<&str> = profile.env_vars.iter().map(|var| var.name).collect();
        format!(
            "no credential for {}: set {}",
            profile.label,
            names.join(" or ")
        )
    })?;
    ProviderConfig::for_profile(profile, model, auth).map_err(|error| error.to_string())
}

/// The name a run's journal is filed under: the directory the run copied its
/// repo into, which the runner makes unique per task and per run.
fn run_name(repo: &Path) -> String {
    repo.file_name()
        .map_or_else(|| "run".to_string(), |name| name.to_string_lossy().into())
}

// ========================================================================
// FixtureSkills Implementations
// ========================================================================

/// The skills a task's own fixture carries, as a name → body registry.
///
/// Keyed by directory name rather than frontmatter, deliberately: this reads a
/// checked-in fixture whose names are its directories, and the `skill` tool
/// only ever needs the body. The user-facing registry in the CLI parses
/// frontmatter because it must describe skills it never loads; this one does
/// not, and the simpler read is one less parser to keep honest.
struct FixtureSkills {
    bodies: BTreeMap<String, String>,
}

impl FixtureSkills {
    /// Every `.summer/skills/<name>/SKILL.md` under `repo`. A repo with none is
    /// an empty registry, not an error: the gate's rules are what decide
    /// whether that matters, and they are checked against what was found.
    fn load(repo: &Path) -> Self {
        let mut bodies = BTreeMap::new();
        let dir = repo.join(".summer").join("skills");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Self { bodies };
        };
        for entry in entries.flatten() {
            let skill_dir = entry.path();
            let Some(name) = skill_dir.file_name() else {
                continue;
            };
            let Ok(body) = std::fs::read_to_string(skill_dir.join("SKILL.md")) else {
                continue;
            };
            bodies.insert(name.to_string_lossy().into_owned(), body);
        }
        Self { bodies }
    }

    /// The catalog appended to the system prompt, in the same shape the CLI's
    /// registry appends, so the model is told the same thing either way.
    fn catalog(&self) -> String {
        let mut out = String::from("<available_skills>\n");
        for name in self.bodies.keys() {
            out.push_str(&format!("  {name}\n"));
        }
        out.push_str(
            "</available_skills>\n\
             To follow a skill, call the `skill` tool with its name and apply the instructions \
             it returns.",
        );
        out
    }
}

impl SkillLookup for FixtureSkills {
    fn lookup(&self, name: &str) -> Option<String> {
        self.bodies.get(name).cloned()
    }
}

// ========================================================================
// Unattended Implementations
// ========================================================================

#[async_trait]
impl Prompter for Unattended {
    async fn ask(&self, request: &Request) -> Grant {
        eprintln!(
            "bench: denied (no user to ask): {request:?}\n  \
             add the program to the task set's `commands` if the task needs it."
        );
        Grant::once(false)
    }
}
