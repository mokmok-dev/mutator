//! Argument parsing and the run itself.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::accept::{self, Accepted};
use crate::cache::{self, Entry, Hits};
use crate::diff::Diff;
use crate::error::{Error, Result};
use crate::exec::{self, Exec, Real};
use crate::mutant::{self, Mutant, MutantId};
use crate::report::{self, Gate, GateResult, Outcome, Record, Summary};
use crate::scope::{self, Scope};

/// Mutants per `cargo mutants` invocation. Each one repeats the baseline, so keep it large.
const CHUNK: usize = 400;

/// Exit code for a gate failure, matching `cargo-mutants`.
const GATE_FAILED: i32 = 2;

/// Exit code for a failing baseline, matching `cargo-mutants`.
const BASELINE_FAILED: i32 = 4;

const USAGE_ERROR: i32 = 1;

#[derive(Debug, Parser)]
#[command(
    name = "mutator",
    about = "Diff-driven mutation testing for Rust",
    version
)]
pub struct Cli {
    #[command(flatten)]
    pub run: Run,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Args)]
pub struct Run {
    #[command(flatten)]
    pub project: Project,

    #[command(flatten)]
    pub selection: Selection,

    #[command(flatten)]
    pub gate: GateArgs,

    /// Report format, repeatable.
    #[arg(long, value_enum, default_value = "text")]
    pub reporter: Vec<Reporter>,

    /// Print the selected mutant names and run nothing. The reason a scope is wider
    /// than the functions the diff touched goes to stderr.
    #[arg(long)]
    pub list: bool,

    /// Reuse verdicts for mutants whose package is byte-for-byte unchanged.
    #[arg(long)]
    pub cache: bool,

    /// Write a Markdown job summary to this path, for `$GITHUB_STEP_SUMMARY`.
    #[arg(long, value_name = "FILE")]
    pub summary: Option<PathBuf>,
}

/// Where the project lives and how `cargo-mutants` reaches it.
#[derive(Debug, Args)]
pub struct Project {
    /// Project root.
    #[arg(long, default_value = ".", global = true)]
    pub root: PathBuf,

    /// Passed through to `cargo mutants`.
    #[arg(long, global = true)]
    pub manifest_path: Option<PathBuf>,

    /// Echo each command on stderr.
    #[arg(long, short, global = true)]
    pub verbose: bool,

    /// Passed through as `cargo mutants -j`.
    #[arg(long, global = true)]
    pub jobs: Option<u32>,

    /// Passed through as `cargo mutants -t`, in seconds.
    #[arg(long, global = true)]
    pub timeout: Option<u64>,
}

impl Project {
    fn root(&self) -> Result<PathBuf> {
        self.root.canonicalize().map_err(|source| Error::Root {
            path: self.root.clone(),
            source,
        })
    }
}

/// Which mutants to consider.
#[derive(Debug, Default, Args)]
#[group(multiple = false)]
pub struct Selection {
    /// Only mutants touched by `git diff` against the merge base of this revision.
    #[arg(long)]
    pub since: Option<String>,

    /// Only mutants touched by this unified diff file.
    #[arg(long)]
    pub diff: Option<PathBuf>,

    /// Every mutant in the project. The default when neither `--since` nor `--diff` is given.
    #[arg(long)]
    pub all: bool,
}

/// What fails the gate.
#[derive(Debug, Default, Args)]
pub struct GateArgs {
    /// Exit 2 when any mutant survived.
    #[arg(long)]
    pub fail_on_survived: bool,

    /// Exit 2 when the score is below this percentage.
    #[arg(long, value_parser = parse_percent)]
    pub threshold: Option<u32>,
}

impl GateArgs {
    const fn gate(&self) -> Gate {
        Gate {
            fail_on_survived: self.fail_on_survived,
            threshold_bp: self.threshold,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Reporter {
    Text,
    Json,
    Github,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Record a mutant as equivalent, with the reason no input can observe it.
    Accept(AcceptArgs),
}

#[derive(Debug, Args)]
pub struct AcceptArgs {
    /// Mutant ids, as printed by the text or JSON report.
    #[arg(required = true)]
    pub ids: Vec<String>,

    /// Why the mutant cannot be detected.
    #[arg(long)]
    pub reason: String,
}

/// Parse `NN`, `NN.N`, or `NN.NN`, optionally with a trailing `%`, as basis points.
fn parse_percent(text: &str) -> std::result::Result<u32, String> {
    let body = text.strip_suffix('%').unwrap_or(text).trim();
    if body.is_empty() || body.ends_with('.') {
        return Err(format!("{text:?} is not a percentage"));
    }
    let (whole, fraction) = body.split_once('.').unwrap_or((body, ""));
    let whole: u32 = match whole {
        "" => 0,
        digits => digits
            .parse()
            .map_err(|_| format!("{text:?} is not a percentage"))?,
    };
    let hundredths = parse_fraction(text, fraction)?;
    if whole > 100 || (whole == 100 && hundredths > 0) {
        return Err(format!("{text:?} is above 100"));
    }
    Ok(whole * 100 + hundredths)
}

/// Read a percentage fraction, which holds at most two digits, as hundredths.
fn parse_fraction(text: &str, fraction: &str) -> std::result::Result<u32, String> {
    if fraction.len() > 2 || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(format!("{text:?} needs at most two decimal digits"));
    }
    let digits: u32 = if fraction.is_empty() {
        0
    } else {
        fraction
            .parse()
            .map_err(|_| format!("{text:?} is not a percentage"))?
    };
    Ok(if fraction.len() == 1 {
        digits * 10
    } else {
        digits
    })
}

pub fn main() -> ExitCode {
    let cli = Cli::parse();
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    let mut out = stdout.lock();
    let mut err = stderr.lock();
    into_exit_code(dispatch(&cli, &Real, &mut out, &mut err))
}

/// Map a run's exit code to the process exit code.
pub fn into_exit_code(code: i32) -> ExitCode {
    u8::try_from(code).map_or(ExitCode::FAILURE, ExitCode::from)
}

/// Run one command, mapping an error to the usage exit code and a message on stdout.
pub fn dispatch(cli: &Cli, exec: &dyn Exec, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let code = match &cli.command {
        Some(Command::Accept(args)) => execute_accept(&cli.run.project, args, exec, out),
        None => execute(&cli.run, exec, out, err),
    };
    code.unwrap_or_else(|err| {
        let _ = writeln!(out, "mutator: {err:#}");
        USAGE_ERROR
    })
}

/// What a mutant run produced.
enum Ran {
    Records(Vec<Record>),
    /// The tests fail without any mutant, so no verdict means anything.
    BaselineFailed(String),
}

/// Select mutants, run them, report, and gate. Returns the process exit code.
///
/// # Errors
///
/// Returns when the project cannot be read, `cargo-mutants` fails in a way the
/// tool cannot interpret, or a report cannot be written.
pub fn execute(
    run: &Run,
    exec: &dyn Exec,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32> {
    let started = std::time::Instant::now();
    let mut session = Session {
        exec,
        err,
        project: &run.project,
        cached_packages: Vec::new(),
    };
    let root = run.project.root()?;
    // Parse the diff before listing mutants, so a malformed diff is reported as
    // such even when `cargo-mutants` is missing.
    let diff = session.diff(&run.selection, &root)?;
    let mutants = session.list(&root)?;
    let ids = mutant::assign_ids(&mutants);
    let accepted = Accepted::load(&root)?;
    let (scope, selection) = session.scope(diff.as_ref(), &root, &mutants)?;

    if run.list {
        if let Some(reason) = scope.reason.describe() {
            writeln!(err, "{reason}")?;
        }
        for index in &selection {
            writeln!(out, "{}", mutants[*index].name)?;
        }
        return Ok(0);
    }

    let stale: Vec<String> = accept::stale(
        &accepted,
        &ids.iter().map(|id| id.as_str().to_owned()).collect(),
    )
    .into_iter()
    .map(|(id, _)| id.to_owned())
    .collect();
    let selected: Vec<(&Mutant, MutantId)> = selection
        .iter()
        .map(|index| (&mutants[*index], ids[*index].clone()))
        .collect();

    // Accepted entries never run. With `--cache`, a mutant whose package is
    // byte-for-byte unchanged is decided from its stored verdict instead, unless a
    // changed file sits outside every package and can invalidate the verdict.
    let decision = if run.cache && scope.reason.reuses_verdicts() {
        session.decide(&root, &selected, &accepted)?
    } else {
        Decision::none(&selected, &accepted)
    };

    let records = match session.run_mutants(&root, &decision.pending)? {
        Ran::Records(records) => records,
        Ran::BaselineFailed(message) => {
            writeln!(out, "mutator: {message}")?;
            return Ok(BASELINE_FAILED);
        }
    };
    if run.cache {
        session.store(&root, &decision, &records)?;
    }
    let mut records = records;
    records.extend(decision.settled());

    let summary = report::build(&selected, &records, &accepted, scope, mutants.len(), stale)
        .with_elapsed(started.elapsed());
    let summary = if run.cache {
        summary.with_cache(decision.cache)
    } else {
        summary
    };
    emit(&summary, run, out)?;

    if let GateResult::Fail(message) = summary.gate(&run.gate.gate()) {
        writeln!(out, "gate: failed: {message}")?;
        return Ok(GATE_FAILED);
    }
    Ok(0)
}

/// Write every requested report, then the job summary when one was asked for.
///
/// The summary goes to a file rather than stdout because GitHub reads it from
/// `$GITHUB_STEP_SUMMARY`, and a run that also prints JSON must keep stdout parseable.
fn emit(summary: &Summary, run: &Run, out: &mut dyn Write) -> Result<()> {
    let mut seen = Vec::new();
    for reporter in &run.reporter {
        if seen.contains(reporter) {
            continue;
        }
        seen.push(*reporter);
        match reporter {
            Reporter::Text => write!(out, "{}", report::render_text(summary))?,
            Reporter::Json => write!(out, "{}", report::render_json(summary)?)?,
            Reporter::Github => write!(out, "{}", report::render_github(summary))?,
        }
    }
    if let Some(path) = &run.summary {
        let text = report::render_summary(summary, &run.gate.gate());
        std::fs::write(path, text).map_err(|source| Error::Write {
            path: path.clone(),
            source,
        })?;
    }
    Ok(())
}

/// What a run learned about a package, so the next run can reuse it.
struct Learned {
    package: String,
    fingerprint: cache::Fingerprint,
}

/// Who runs and who is settled from a stored verdict.
struct Decision<'a> {
    /// Mutants that still need `cargo-mutants`.
    pending: Vec<&'a Mutant>,
    /// Mutants a stored verdict already killed.
    reused_caught: usize,
    /// Mutants a stored verdict already showed surviving.
    reused_missed: usize,
    /// Caught records to inject for the reused kills.
    settled: Vec<Record>,
    /// What the cache knew about the selection, for the report.
    cache: report::CacheStats,
}

impl<'a> Decision<'a> {
    /// No cache: everything not accepted runs.
    fn none(selected: &[(&'a Mutant, MutantId)], accepted: &Accepted) -> Self {
        Self {
            pending: selected
                .iter()
                .filter(|(_, id)| !accepted.is_accepted(id))
                .map(|(mutant, _)| *mutant)
                .collect(),
            reused_caught: 0,
            reused_missed: 0,
            settled: Vec::new(),
            cache: report::CacheStats::default(),
        }
    }

    /// Records for verdicts a stored entry already decided, so the report and the
    /// gate treat a reused kill exactly like a fresh one.
    fn settled(&self) -> Vec<Record> {
        self.settled.clone()
    }
}

/// One run's dependencies, so each command sees the same seam and echo stream.
struct Session<'a> {
    exec: &'a dyn Exec,
    err: &'a mut dyn Write,
    project: &'a Project,
    /// Packages whose fingerprints this run computed, for storing verdicts.
    cached_packages: Vec<Learned>,
}

impl Session<'_> {
    /// Split the selection into reused verdicts and work that still needs a run.
    ///
    /// A mutant is reused only when its package's fingerprint matches a stored
    /// entry, and the fingerprint covers the package's whole local dependency
    /// closure. Killed mutants are settled from the store. Survivors are decided
    /// again so a new test can kill them, since a survivor only means that no
    /// test observed it.
    fn decide<'a>(
        &mut self,
        root: &Path,
        selected: &[(&'a Mutant, MutantId)],
        accepted: &Accepted,
    ) -> Result<Decision<'a>> {
        let cargo_mutants = cache::cargo_mutants_version(self.exec, root)?;
        let local = cache::packages(root, self.exec)?;
        let mut hits = Hits::default();
        let mut packages = Vec::new();
        for (package, dir) in &local {
            if let Some(reason) = &dir.opaque {
                let _ = writeln!(
                    self.err,
                    "mutator: not reusing verdicts for {package}: {reason}"
                );
                continue;
            }
            let files = cache::fingerprint_sources(dir);
            let fingerprint = cache::fingerprint(root, package, &files, &cargo_mutants)?;
            hits.extend(&cache::load(root, package, &fingerprint, &cargo_mutants));
            packages.push(Learned {
                package: package.clone(),
                fingerprint,
            });
        }
        self.cached_packages = packages;
        let mut decision = Decision::none(selected, accepted);
        decision.settled = selected
            .iter()
            .filter(|(mutant, id)| !accepted.is_accepted(id) && hits.killed(&mutant.name))
            .map(|(mutant, _)| Record {
                name: mutant.name.clone(),
                outcome: Outcome::Caught,
            })
            .collect();
        decision
            .pending
            .retain(|pending| !hits.killed(&pending.name));
        decision.reused_caught = decision.settled.len();
        decision.reused_missed = decision
            .pending
            .iter()
            .filter(|pending| hits.seen(&pending.name))
            .count();
        decision.cache = report::CacheStats {
            caught: decision.reused_caught,
            missed: decision.reused_missed,
            misses: decision.pending.len() - decision.reused_missed,
        };
        Ok(decision)
    }

    /// Write what this run learned, so the next unchanged package hits.
    ///
    /// Mutants decided by this run replace their stored verdict. Mutants it left
    /// out keep theirs, so a killed mutant does not become pending again. A package
    /// with no fingerprint is opaque, so nothing is written for it: a store nothing
    /// could safely read back would only be dead weight.
    fn store(&mut self, root: &Path, decision: &Decision<'_>, records: &[Record]) -> Result<()> {
        let by_name: BTreeMap<&str, Outcome> = records
            .iter()
            .map(|record| (record.name.as_str(), record.outcome))
            .collect();
        let cargo_mutants = cache::cargo_mutants_version(self.exec, root)?;
        let mut learned = BTreeMap::new();
        for package in &self.cached_packages {
            let existing =
                cache::load(root, &package.package, &package.fingerprint, &cargo_mutants);
            let mut entries: BTreeMap<String, Entry> = existing.entries().clone();
            for mutant in decision
                .pending
                .iter()
                .filter(|m| m.package == package.package)
            {
                let caught = match by_name.get(mutant.name.as_str()) {
                    Some(Outcome::Caught) => Some(true),
                    Some(Outcome::Missed) => Some(false),
                    _ => None,
                };
                if let Some(caught) = caught {
                    entries.insert(
                        mutant.name.clone(),
                        Entry {
                            package: package.package.clone(),
                            caught,
                        },
                    );
                }
            }
            learned.insert(
                package.package.clone(),
                (package.fingerprint, entries.into_iter().collect()),
            );
        }
        cache::save_all(root, &learned, &cargo_mutants)
    }

    /// Echo a command to stderr when `--verbose` is set, keeping stdout parseable.
    fn announce(&mut self, program: &str, args: &[String]) {
        if self.project.verbose {
            let _ = writeln!(self.err, "+ {program} {}", args.join(" "));
        }
    }

    fn git(&mut self, root: &Path, args: &[&str]) -> Result<String> {
        let owned: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
        self.announce("git", &owned);
        let output = self.exec.run("git", &owned, root)?;
        if !output.success() {
            return Err(Error::Git {
                command: args.join(" "),
                stderr: output.stderr.trim().to_owned(),
            });
        }
        Ok(output.stdout)
    }

    fn list(&mut self, root: &Path) -> Result<Vec<Mutant>> {
        let args = exec::list_args(self.project.manifest_path.as_deref());
        self.announce("cargo", &args);
        let output = self.exec.run("cargo", &args, root)?;
        if !output.success() {
            return Err(Error::List {
                code: output.code,
                stderr: output.stderr.trim().to_owned(),
            });
        }
        mutant::parse_list(&output.stdout)
    }

    /// The diff a run selects from, labelled by where it came from.
    ///
    /// `None` when the run selects every mutant.
    fn diff(&mut self, selection: &Selection, root: &Path) -> Result<Option<(String, Diff)>> {
        let (label, diff) = if let Some(path) = &selection.diff {
            let text = std::fs::read_to_string(path).map_err(|source| Error::Read {
                path: path.clone(),
                source,
            })?;
            let diff = Diff::parse(&text).map_err(|source| Error::DiffFile {
                path: path.clone(),
                source,
            })?;
            (format!("diff {}", path.display()), diff)
        } else if let Some(revision) = &selection.since {
            let base = self.git(root, &["merge-base", revision, "HEAD"])?;
            let base = base.trim();
            if base.is_empty() {
                return Err(Error::EmptyMergeBase {
                    revision: revision.clone(),
                });
            }
            let text = self.git(root, &["diff", "-U0", "--no-color", "--no-ext-diff", base])?;
            let diff = Diff::parse(&text).map_err(|source| Error::Diff {
                commit: base.to_owned(),
                source,
            })?;
            (format!("diff since {revision}"), diff)
        } else {
            return Ok(None);
        };
        Ok(Some((label, diff)))
    }

    /// The scope and the indices of the selected mutants.
    ///
    /// A diff that only touches tests or manifests selects nothing by function
    /// span, so those paths widen the scope to the packages they belong to. So does
    /// a source change outside every function span. A path no package owns widens
    /// it to the whole workspace.
    fn scope(
        &mut self,
        diff: Option<&(String, Diff)>,
        root: &Path,
        mutants: &[Mutant],
    ) -> Result<(Scope, Vec<usize>)> {
        let Some((label, diff)) = diff else {
            return Ok((Scope::all(), (0..mutants.len()).collect()));
        };
        // Ownership comes from `cargo metadata`, which costs a process per run, so
        // only ask when a changed path actually needs a package fallback.
        let packages = if scope::widens(diff, mutants) {
            self.packages(root)?
        } else {
            BTreeMap::new()
        };
        let (reason, selected) = scope::select(diff, &packages, mutants);
        Ok((
            Scope {
                label: label.clone(),
                reason,
            },
            selected,
        ))
    }

    /// Each workspace member's directory, relative to the project root.
    fn packages(&mut self, root: &Path) -> Result<BTreeMap<String, PathBuf>> {
        Ok(cache::packages(root, self.exec)?
            .into_iter()
            .map(|(name, package)| {
                let relative = package
                    .dir
                    .strip_prefix(root)
                    .map_or_else(|_| package.dir.clone(), Path::to_path_buf);
                (name, relative)
            })
            .collect())
    }

    fn run_mutants(&mut self, root: &Path, pending: &[&Mutant]) -> Result<Ran> {
        let mut records = Vec::new();
        for (index, chunk) in pending.chunks(CHUNK).enumerate() {
            let names: Vec<&str> = chunk.iter().map(|mutant| mutant.name.as_str()).collect();
            let out_dir = root
                .join("target")
                .join("mutator")
                .join(format!("mutants-{index}"));
            // `cargo mutants` creates its output directory but not the parent of `--output`.
            std::fs::create_dir_all(&out_dir).map_err(|source| Error::Create {
                path: out_dir.clone(),
                source,
            })?;
            let args = exec::run_args(
                &names,
                &out_dir,
                self.project.manifest_path.as_deref(),
                self.project.jobs,
                self.project.timeout,
            )?;
            self.announce("cargo", &args);
            let output = self.exec.run("cargo", &args, root)?;
            let mutants_out = out_dir.join("mutants.out");
            let outcomes_path = mutants_out.join("outcomes.json");
            if output.code == BASELINE_FAILED {
                return Ok(Ran::BaselineFailed(format!(
                    "tests fail without any mutant; see {} ({}), then {}",
                    outcomes_path.display(),
                    output.stderr.trim(),
                    mutants_out.join("log").display()
                )));
            }
            if !matches!(output.code, 0 | GATE_FAILED | 3) {
                return Err(Error::Run {
                    code: output.code,
                    stderr: output.stderr.trim().to_owned(),
                });
            }
            let text = std::fs::read_to_string(&outcomes_path).map_err(|source| Error::Read {
                path: outcomes_path.clone(),
                source,
            })?;
            if report::baseline_failed(&text)? {
                return Ok(Ran::BaselineFailed(format!(
                    "tests fail without any mutant; see {}",
                    mutants_out.join("log").display()
                )));
            }
            records.extend(report::parse_outcomes(&text)?);
        }
        Ok(Ran::Records(records))
    }
}

/// Resolve ids against the full mutant list and record them with their reason.
///
/// # Errors
///
/// Returns when the reason is empty, the project cannot be read, or an id names no mutant.
pub fn execute_accept(
    project: &Project,
    args: &AcceptArgs,
    exec: &dyn Exec,
    out: &mut dyn Write,
) -> Result<i32> {
    if args.reason.trim().is_empty() {
        return Err(Error::EmptyReason);
    }
    let root = project.root()?;
    let mut discard = std::io::sink();
    let mut session = Session {
        exec,
        err: &mut discard,
        project,
        cached_packages: Vec::new(),
    };
    let mutants = session.list(&root)?;
    let known: BTreeMap<String, MutantId> = mutant::assign_ids(&mutants)
        .into_iter()
        .map(|id| (id.as_str().to_owned(), id))
        .collect();
    let mut accepted = Accepted::load(&root)?;
    for requested in &args.ids {
        let Some(id) = known.get(requested) else {
            return Err(Error::UnknownMutant {
                id: requested.clone(),
            });
        };
        accepted.insert(id.clone(), args.reason.clone());
        writeln!(out, "accepted {}", id.as_str())?;
    }
    accepted.save(&root)?;
    Ok(0)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::Output;
    use clap::Parser;
    use std::cell::RefCell;
    use std::collections::VecDeque;

    const LIST: &str = r#"[
      {"name":"src/lib.rs:3:5: replace parse -> u32 with 0","file":"src/lib.rs","package":"mutator",
       "genre":"FnValue","replacement":"0",
       "span":{"start":{"line":3,"column":5},"end":{"line":3,"column":9}},
       "function":{"function_name":"parse","return_type":"-> u32",
                   "span":{"start":{"line":3,"column":1},"end":{"line":7,"column":2}}}},
      {"name":"src/lib.rs:20:5: replace render -> u32 with 0","file":"src/lib.rs","package":"mutator",
       "genre":"FnValue","replacement":"0",
       "span":{"start":{"line":20,"column":5},"end":{"line":20,"column":9}},
       "function":{"function_name":"render","return_type":"-> u32",
                   "span":{"start":{"line":20,"column":1},"end":{"line":24,"column":2}}}}
    ]"#;

    const BOTH: &str = r#"{"outcomes":[{"scenario":"Baseline","summary":"Success"},
      {"scenario":{"Mutant":{"name":"src/lib.rs:3:5: replace parse -> u32 with 0"}},"summary":"CaughtMutant"},
      {"scenario":{"Mutant":{"name":"src/lib.rs:20:5: replace render -> u32 with 0"}},"summary":"CaughtMutant"}]}"#;

    const ONE_MISSED: &str = r#"{"outcomes":[{"scenario":"Baseline","summary":"Success"},
      {"scenario":{"Mutant":{"name":"src/lib.rs:3:5: replace parse -> u32 with 0"}},"summary":"CaughtMutant"},
      {"scenario":{"Mutant":{"name":"src/lib.rs:20:5: replace render -> u32 with 0"}},"summary":"MissedMutant"}]}"#;

    const FAILED_BASELINE: &str = r#"{"outcomes":[{"scenario":"Baseline","summary":"Failure"}]}"#;

    const DIFF: &str = "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -21,1 +21,1 @@\n-old\n+new\n";

    const TEST_DIFF: &str = "diff --git a/tests/t.rs b/tests/t.rs\n--- a/tests/t.rs\n+++ b/tests/t.rs\n@@ -1,1 +1,1 @@\n-old\n+new\n";

    const DELETED_TEST_DIFF: &str = "diff --git a/tests/t.rs b/tests/t.rs\n--- a/tests/t.rs\n+++ /dev/null\n@@ -1,1 +0,0 @@\n-x\n";

    const RENAMED_TEST_DIFF: &str = "diff --git a/tests/old.rs b/tests/new.rs\n\
        similarity index 100%\n\
        rename from tests/old.rs\n\
        rename to tests/new.rs\n";

    /// `cargo metadata` output for one package whose manifest sits in `dir`.
    fn metadata_json(root: &Path, dir: &str, package: &str) -> String {
        let manifest = root.join(dir).join("Cargo.toml");
        let id = format!("path+file://{}#0.1.0", root.join(dir).display());
        format!(
            r#"{{"packages":[{{"id":"{id}","name":"{package}","source":null,"manifest_path":"{}"}}],
               "resolve":{{"nodes":[{{"id":"{id}","dependencies":[]}}]}},
               "workspace_root":"{}"}}"#,
            manifest.display(),
            root.display()
        )
    }

    /// Answers each command from the queue and writes outcomes where `cargo mutants` would.
    struct Fake {
        list: String,
        runs: RefCell<VecDeque<(i32, String)>>,
        git: RefCell<VecDeque<Output>>,
        run_index: RefCell<usize>,
        calls: RefCell<Vec<Vec<String>>>,
        all_caught: bool,
        /// Where `cargo metadata` says the one package's manifest sits, relative to the root.
        package_dir: &'static str,
    }

    impl Fake {
        fn new(list: &str, runs: Vec<(i32, &str)>, git: Vec<Output>) -> Self {
            Self {
                list: list.to_owned(),
                runs: RefCell::new(
                    runs.into_iter()
                        .map(|(code, json)| (code, json.to_owned()))
                        .collect(),
                ),
                git: RefCell::new(git.into()),
                run_index: RefCell::new(0),
                calls: RefCell::new(Vec::new()),
                all_caught: false,
                package_dir: "",
            }
        }

        /// Build each chunk's outcomes from the names its `--re` selects.
        fn catching_everything(list: &str) -> Self {
            Self {
                all_caught: true,
                ..Self::new(list, vec![], vec![])
            }
        }

        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.borrow().clone()
        }

        fn run_calls(&self) -> Vec<Vec<String>> {
            self.calls()
                .into_iter()
                .filter(|args| args.iter().any(|arg| arg == "--output"))
                .collect()
        }
    }

    /// The names a `cargo mutants --re` argument selects, unescaped.
    fn selected_names(args: &[String]) -> Vec<String> {
        args.iter()
            .position(|arg| arg == "--re")
            .and_then(|index| args.get(index + 1))
            .map_or_else(Vec::new, |regex| {
                regex
                    .trim_start_matches("^(?:")
                    .trim_end_matches(")$")
                    .split('|')
                    .map(|name| name.replace('\\', ""))
                    .collect()
            })
    }

    impl Exec for Fake {
        fn run(&self, program: &str, args: &[String], cwd: &Path) -> Result<Output> {
            self.calls.borrow_mut().push(args.to_vec());
            if program == "git" {
                return self.git.borrow_mut().pop_front().ok_or_else(|| {
                    crate::error::Error::Spawn {
                        program: format!("git {}", args.join(" ")),
                        source: std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            "no queued answer",
                        ),
                    }
                });
            }
            if program == "cargo" && args.first().is_some_and(|arg| arg == "metadata") {
                return Ok(ok(&metadata_json(cwd, self.package_dir, "mutator")));
            }
            if args.iter().any(|arg| arg == "--list") {
                return Ok(ok(&self.list));
            }
            let (code, json) = if self.all_caught {
                let records: Vec<String> = selected_names(args)
                    .into_iter()
                    .map(|name| {
                        format!(
                            r#"{{"scenario":{{"Mutant":{{"name":"{name}"}}}},"summary":"CaughtMutant"}}"#
                        )
                    })
                    .collect();
                (
                    0,
                    format!(
                        r#"{{"outcomes":[{{"scenario":"Baseline","summary":"Success"}},{}]}}"#,
                        records.join(",")
                    ),
                )
            } else {
                self.runs
                    .borrow_mut()
                    .pop_front()
                    .ok_or_else(|| crate::error::Error::Run {
                        code: -1,
                        stderr: format!("no queued answer for {}", args.join(" ")),
                    })?
            };
            let index = *self.run_index.borrow();
            *self.run_index.borrow_mut() = index + 1;
            let dir = cwd
                .join("target")
                .join("mutator")
                .join(format!("mutants-{index}"))
                .join("mutants.out");
            std::fs::create_dir_all(&dir)?;
            std::fs::write(dir.join("outcomes.json"), json)?;
            Ok(Output {
                code,
                stdout: String::new(),
                stderr: "run stderr".to_owned(),
            })
        }
    }

    fn ok(stdout: &str) -> Output {
        Output {
            code: 0,
            stdout: stdout.to_owned(),
            stderr: String::new(),
        }
    }

    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mutator-cli-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn run_at(root: &Path, extra: &[&str]) -> Run {
        let mut argv = vec![
            "mutator".to_owned(),
            "--root".to_owned(),
            root.display().to_string(),
        ];
        argv.extend(extra.iter().map(|arg| (*arg).to_owned()));
        Cli::parse_from(argv).run
    }

    fn execute_str(run: &Run, fake: &dyn Exec) -> (i32, String) {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = execute(run, fake, &mut out, &mut err).unwrap_or_else(|err| panic!("{err:#}"));
        (code, String::from_utf8(out).unwrap())
    }

    #[test]
    fn all_caught_exits_zero_and_names_the_scope() {
        let root = temp_root("caught");
        let fake = Fake::new(LIST, vec![(0, BOTH)], vec![]);
        let (code, text) = execute_str(&run_at(&root, &[]), &fake);
        assert_eq!(code, 0);
        assert!(
            text.starts_with("mutator: 2 of 2 mutants in scope (all mutants)\n"),
            "{text}"
        );
        assert!(
            text.contains("score: 100.00% (2 detected, 0 missed"),
            "{text}"
        );
        assert_eq!(fake.run_calls().len(), 1);
    }

    #[test]
    fn a_survivor_fails_only_with_the_flag() {
        let root = temp_root("survivor");
        let fake = Fake::new(LIST, vec![(2, ONE_MISSED)], vec![]);
        let (code, text) = execute_str(&run_at(&root, &["--fail-on-survived"]), &fake);
        assert_eq!(code, 2);
        assert!(text.contains("gate: failed: 1 missed mutant"), "{text}");

        let fake = Fake::new(LIST, vec![(2, ONE_MISSED)], vec![]);
        let (code, _) = execute_str(&run_at(&root, &[]), &fake);
        assert_eq!(code, 0);
    }

    #[test]
    fn a_failing_baseline_exits_four() {
        let root = temp_root("baseline");
        let fake = Fake::new(LIST, vec![(4, FAILED_BASELINE)], vec![]);
        let (code, text) = execute_str(&run_at(&root, &[]), &fake);
        assert_eq!(code, 4);
        assert!(text.contains("tests fail without any mutant"), "{text}");
    }

    #[test]
    fn a_baseline_failure_inside_a_gate_run_also_exits_four() {
        let root = temp_root("baseline-inline");
        let fake = Fake::new(LIST, vec![(2, FAILED_BASELINE)], vec![]);
        let (code, text) = execute_str(&run_at(&root, &[]), &fake);
        assert_eq!(code, 4);
        assert!(text.contains("log"), "{text}");
    }

    #[test]
    fn a_diff_file_selects_only_the_touched_function() {
        let root = temp_root("diff");
        let diff = root.join("x.diff");
        std::fs::write(&diff, DIFF).unwrap();
        let fake = Fake::new(LIST, vec![(0, BOTH)], vec![]);
        let (code, text) = execute_str(&run_at(&root, &["--diff", diff.to_str().unwrap()]), &fake);
        assert_eq!(code, 0);
        assert!(text.contains("1 of 2 mutants in scope (diff "), "{text}");
        assert!(text.contains("render"));
        assert!(!text.contains("  parse  "));
        let runs = fake.run_calls();
        assert_eq!(runs.len(), 1);
        assert!(runs[0].iter().any(|arg| arg.contains("render")));
        assert!(!runs[0].iter().any(|arg| arg.contains("parse")));
    }

    #[test]
    fn since_uses_the_merge_base_and_labels_the_scope() {
        let root = temp_root("since");
        let fake = Fake::new(LIST, vec![(0, ONE_MISSED)], vec![ok("abc123\n"), ok(DIFF)]);
        let (code, text) = execute_str(&run_at(&root, &["--since", "main"]), &fake);
        assert_eq!(code, 0);
        assert!(text.contains("(diff since main)"), "{text}");
        let calls = fake.calls();
        // The diff is resolved before `cargo mutants --list`, so a malformed diff
        // fails even when the tool is missing.
        assert_eq!(calls[0], vec!["merge-base", "main", "HEAD"]);
        assert_eq!(calls[1][0], "diff");
        assert_eq!(calls[1].last().map(String::as_str), Some("abc123"));
        assert!(calls[2].iter().any(|arg| arg == "--list"));
    }

    #[test]
    fn list_prints_names_and_runs_nothing() {
        let root = temp_root("list");
        let fake = Fake::new(LIST, vec![], vec![]);
        let (code, text) = execute_str(&run_at(&root, &["--list"]), &fake);
        assert_eq!(code, 0);
        assert_eq!(
            text,
            "src/lib.rs:3:5: replace parse -> u32 with 0\n\
             src/lib.rs:20:5: replace render -> u32 with 0\n"
        );
        assert_eq!(fake.run_calls(), Vec::<Vec<String>>::new());
    }

    #[test]
    fn an_empty_selection_passes_without_running_anything() {
        let root = temp_root("empty");
        let diff = root.join("x.diff");
        std::fs::write(
            &diff,
            "diff --git a/docs/guide.md b/docs/guide.md\n--- a/docs/guide.md\n+++ b/docs/guide.md\n@@ -1,1 +1,1 @@\n-a\n+b\n",
        )
        .unwrap();
        let fake = Fake::new(LIST, vec![], vec![]);
        let (code, text) = execute_str(
            &run_at(
                &root,
                &["--diff", diff.to_str().unwrap(), "--fail-on-survived"],
            ),
            &fake,
        );
        assert_eq!(code, 0);
        assert!(text.contains("no mutants in scope"), "{text}");
        assert_eq!(fake.run_calls(), Vec::<Vec<String>>::new());
    }

    #[test]
    fn a_source_file_with_no_mutants_selects_its_package_instead_of_passing() {
        let root = temp_root("no-mutants-here");
        let diff = root.join("x.diff");
        std::fs::write(
            &diff,
            "diff --git a/other.rs b/other.rs\n--- a/other.rs\n+++ b/other.rs\n@@ -1,1 +1,1 @@\n-a\n+b\n",
        )
        .unwrap();
        let fake = Fake::new(LIST, vec![(0, BOTH)], vec![]);
        let (code, text) = execute_str(
            &run_at(
                &root,
                &["--diff", diff.to_str().unwrap(), "--fail-on-survived"],
            ),
            &fake,
        );
        assert_eq!(code, 0, "{text}");
        assert!(
            text.contains(
                "selection: 1 source file changed, so every mutant of mutator is in scope"
            ),
            "{text}"
        );
        assert_eq!(fake.run_calls().len(), 1, "both mutants must be run");
    }

    #[test]
    fn the_threshold_compares_at_the_bound() {
        let root = temp_root("threshold");
        let fake = Fake::new(LIST, vec![(2, ONE_MISSED)], vec![]);
        let (code, text) = execute_str(&run_at(&root, &["--threshold", "50"]), &fake);
        assert_eq!(code, 0, "{text}");

        let fake = Fake::new(LIST, vec![(2, ONE_MISSED)], vec![]);
        let (code, text) = execute_str(&run_at(&root, &["--threshold", "50.01"]), &fake);
        assert_eq!(code, 2);
        assert!(
            text.contains("score 50.00% is below the 50.01% threshold"),
            "{text}"
        );
    }

    #[test]
    fn an_accepted_mutant_leaves_the_run_and_the_score() {
        let root = temp_root("accepted");
        let mut accepted = Accepted::default();
        accepted.insert(
            MutantId::new("src/lib.rs", "render", "FnValue", "0", 0),
            "no input reaches it".to_owned(),
        );
        accepted.save(&root).unwrap();

        let one_caught = r#"{"outcomes":[{"scenario":"Baseline","summary":"Success"},
          {"scenario":{"Mutant":{"name":"src/lib.rs:3:5: replace parse -> u32 with 0"}},"summary":"CaughtMutant"}]}"#;
        let fake = Fake::new(LIST, vec![(0, one_caught)], vec![]);
        let (code, text) = execute_str(&run_at(&root, &["--fail-on-survived"]), &fake);
        assert_eq!(code, 0, "{text}");
        assert!(text.contains("1 accepted"), "{text}");
        assert!(
            text.contains("score: 100.00% (1 detected, 0 missed, 1 accepted"),
            "{text}"
        );
        let runs = fake.run_calls();
        assert!(!runs[0].iter().any(|arg| arg.contains("render")));
    }

    #[test]
    fn an_accepted_mutant_with_no_live_mutant_is_reported_stale() {
        let root = temp_root("stale");
        let mut accepted = Accepted::default();
        accepted.insert(
            MutantId::new("src/lib.rs", "gone", "FnValue", "0", 0),
            "old".to_owned(),
        );
        accepted.save(&root).unwrap();
        let fake = Fake::new(LIST, vec![(0, BOTH)], vec![]);
        let (_, text) = execute_str(&run_at(&root, &["--reporter", "json"]), &fake);
        assert!(text.contains("src/lib.rs::gone::FnValue::0#0"), "{text}");
    }

    #[test]
    fn json_output_is_one_document() {
        let root = temp_root("json");
        let fake = Fake::new(LIST, vec![(0, BOTH)], vec![]);
        let (code, text) = execute_str(&run_at(&root, &["--reporter", "json"]), &fake);
        assert_eq!(code, 0);
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["schemaVersion"], 1);
        assert_eq!(value["rows"].as_array().map(Vec::len), Some(2));
    }

    #[test]
    fn the_summary_file_reports_the_gate_and_keeps_stdout_parseable() {
        let root = temp_root("summary");
        let path = root.join("step.md");
        let fake = Fake::new(LIST, vec![(2, ONE_MISSED)], vec![]);
        let (code, text) = execute_str(
            &run_at(
                &root,
                &[
                    "--reporter",
                    "json",
                    "--fail-on-survived",
                    "--summary",
                    path.to_str().unwrap(),
                ],
            ),
            &fake,
        );
        assert_eq!(code, 2);
        let first_line = text.lines().next().unwrap_or_default();
        serde_json::from_str::<serde_json::Value>(first_line)
            .expect("the JSON report is the first line on stdout");
        let summary = std::fs::read_to_string(&path).unwrap();
        assert!(
            summary.contains("**Gate: failed, 1 missed mutant**"),
            "{summary}"
        );
        assert!(summary.contains("| survived | 1 |"), "{summary}");
    }

    #[test]
    fn the_summary_file_is_written_even_when_the_gate_passes() {
        let root = temp_root("summary-pass");
        let path = root.join("step.md");
        let fake = Fake::new(LIST, vec![(0, BOTH)], vec![]);
        let (code, _) = execute_str(
            &run_at(&root, &["--summary", path.to_str().unwrap()]),
            &fake,
        );
        assert_eq!(code, 0);
        let summary = std::fs::read_to_string(&path).unwrap();
        assert!(summary.contains("**Gate: passed**"), "{summary}");
    }

    #[test]
    fn an_unwritable_summary_path_is_a_usage_error() {
        let root = temp_root("summary-unwritable");
        let fake = Fake::new(LIST, vec![(0, BOTH)], vec![]);
        let path = root.join("missing-dir").join("step.md");
        let cli = Cli::parse_from([
            "mutator",
            "--root",
            root.to_str().unwrap(),
            "--summary",
            path.to_str().unwrap(),
        ]);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = dispatch(&cli, &fake, &mut out, &mut err);
        assert_eq!(code, 1);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("missing-dir"), "{text}");
    }

    #[test]
    fn the_summary_counts_cache_hits_and_misses_for_a_partial_invalidation() {
        let root = workspace_project("summary-partial");
        let both = killed_all(&["app::m", "core::m"]);
        execute_str(
            &cached_run(&root, &[]),
            &CacheFake::workspace(
                &root,
                workspace_metadata(&root),
                WORKSPACE_LIST,
                vec![(0, both.as_str())],
            ),
        );
        // Only app changes, so core's verdict is reused and app's is a miss.
        std::fs::write(
            root.join("crates/app/src/lib.rs"),
            "pub fn app() { let _ = 1; }\n",
        )
        .unwrap();
        let path = root.join("step.md");
        let second = CacheFake::workspace(
            &root,
            workspace_metadata(&root),
            WORKSPACE_LIST,
            vec![(0, killed("app::m").as_str())],
        );
        let (code, _) = execute_str(
            &cached_run(&root, &["--summary", path.to_str().unwrap()]),
            &second,
        );
        assert_eq!(code, 0);
        let summary = std::fs::read_to_string(&path).unwrap();
        assert!(
            summary.contains("Cache: 1 hit, 1 miss, 1 killed verdict reused"),
            "{summary}"
        );
    }

    #[test]
    fn a_run_without_cache_never_reports_cache_numbers() {
        let root = temp_root("summary-no-cache");
        let path = root.join("step.md");
        let fake = Fake::new(LIST, vec![(0, BOTH)], vec![]);
        let (code, _) = execute_str(
            &run_at(&root, &["--summary", path.to_str().unwrap()]),
            &fake,
        );
        assert_eq!(code, 0);
        let summary = std::fs::read_to_string(&path).unwrap();
        assert!(
            !summary.contains("Cache:"),
            "no flag stores nothing, so no hit or miss exists: {summary}"
        );
    }

    #[test]
    fn accepting_an_unknown_id_fails() {
        let root = temp_root("accept-unknown");
        let fake = Fake::new(LIST, vec![], vec![]);
        let args = AcceptArgs {
            ids: vec!["nope".to_owned()],
            reason: "why".to_owned(),
        };
        let mut out = Vec::new();
        let err = execute_accept(&run_at(&root, &[]).project, &args, &fake, &mut out).unwrap_err();
        assert!(err.to_string().contains("no mutant with id nope"));
    }

    #[test]
    fn accepting_a_known_id_writes_the_store() {
        let root = temp_root("accept-known");
        let id = "src/lib.rs::render::FnValue::0#0";
        let args = AcceptArgs {
            ids: vec![id.to_owned()],
            reason: "unreachable".to_owned(),
        };
        let fake = Fake::new(LIST, vec![], vec![]);
        let mut out = Vec::new();
        assert_eq!(
            execute_accept(&run_at(&root, &[]).project, &args, &fake, &mut out).unwrap(),
            0
        );
        assert_eq!(String::from_utf8(out).unwrap(), format!("accepted {id}\n"));
        let accepted = Accepted::load(&root).unwrap();
        assert!(accepted.is_accepted(&MutantId::new("src/lib.rs", "render", "FnValue", "0", 0)));
    }

    #[test]
    fn an_empty_reason_is_refused() {
        let root = temp_root("accept-empty");
        let args = AcceptArgs {
            ids: vec!["x".to_owned()],
            reason: "  ".to_owned(),
        };
        let fake = Fake::new(LIST, vec![], vec![]);
        let mut out = Vec::new();
        assert!(execute_accept(&run_at(&root, &[]).project, &args, &fake, &mut out).is_err());
    }

    #[test]
    fn percent_parsing_is_exact() {
        for (text, expected) in [
            ("100", 10_000),
            ("50", 5_000),
            ("50.5", 5_050),
            ("50.05", 5_005),
            ("50.05%", 5_005),
            ("0", 0),
            (".5", 50),
        ] {
            assert_eq!(parse_percent(text), Ok(expected), "{text}");
        }
        for text in ["101", "100.01", "50.005", "abc", "-1", "", "50."] {
            assert!(parse_percent(text).is_err(), "{text:?} should be rejected");
        }
    }

    #[test]
    fn the_selectors_are_mutually_exclusive() {
        assert!(Cli::try_parse_from(["mutator", "--since", "main", "--all"]).is_err());
        assert!(Cli::try_parse_from(["mutator", "--since", "main", "--diff", "x"]).is_err());
        assert!(Cli::try_parse_from(["mutator", "--diff", "x", "--all"]).is_err());
    }

    /// A project on disk with one package, so fingerprints and `cargo metadata` are real.
    fn cache_project(name: &str) -> PathBuf {
        let root = temp_root(&format!("cache-{name}"));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n"),
        )
        .unwrap();
        root
    }

    /// Serves `cargo metadata`, `cargo mutants --version`, and runs from the queue.
    struct CacheFake {
        root: PathBuf,
        runs: RefCell<std::collections::VecDeque<(i32, String)>>,
        runs_started: RefCell<Vec<Vec<String>>>,
        /// Where `cargo metadata` says the one package's manifest sits, relative to the root.
        package_dir: &'static str,
        /// The `cargo metadata` document to serve, instead of the one-package default.
        metadata: Option<String>,
        /// The `cargo mutants --list` document to serve.
        list: &'static str,
    }

    impl CacheFake {
        fn new(root: &Path, runs: Vec<(i32, &str)>) -> Self {
            Self {
                root: root.to_path_buf(),
                runs: RefCell::new(
                    runs.into_iter()
                        .map(|(code, json)| (code, json.to_owned()))
                        .collect(),
                ),
                runs_started: RefCell::new(Vec::new()),
                package_dir: "",
                metadata: None,
                list: CACHE_LIST,
            }
        }

        /// Serve `metadata` and `list`, for a project with more than one package.
        fn workspace(
            root: &Path,
            metadata: String,
            list: &'static str,
            runs: Vec<(i32, &str)>,
        ) -> Self {
            Self {
                metadata: Some(metadata),
                list,
                ..Self::new(root, runs)
            }
        }
    }

    impl Exec for CacheFake {
        fn run(&self, program: &str, args: &[String], _cwd: &Path) -> Result<Output> {
            if program == "cargo" && args.first().is_some_and(|a| a == "metadata") {
                if let Some(metadata) = &self.metadata {
                    return Ok(ok(metadata));
                }
                let dir = self.root.join(self.package_dir);
                let id = format!("path+file://{}#0.1.0", dir.display());
                let manifest = dir
                    .join("Cargo.toml")
                    .to_str()
                    .map_or_else(String::new, str::to_owned);
                return Ok(ok(&format!(
                    r#"{{"packages":[{{"id":"{id}","name":"cachepkg","source":null,
                        "manifest_path":"{manifest}"}}],
                       "resolve":{{"nodes":[{{"id":"{id}","dependencies":[]}}]}},
                       "workspace_root":"{}"}}"#,
                    dir.display()
                )));
            }
            if args.iter().any(|arg| arg == "--version") {
                return Ok(ok("cargo-mutants 27.1.0\n"));
            }
            if args.iter().any(|arg| arg == "--list") {
                return Ok(ok(self.list));
            }
            if args.iter().any(|arg| arg == "--output") {
                self.runs_started.borrow_mut().push(args.to_vec());
                let (code, json) =
                    self.runs
                        .borrow_mut()
                        .pop_front()
                        .ok_or_else(|| crate::error::Error::Run {
                            code: -1,
                            stderr: "unexpected run".to_owned(),
                        })?;
                let dir = args
                    .iter()
                    .position(|arg| arg == "--output")
                    .and_then(|index| args.get(index + 1))
                    .map_or_else(PathBuf::new, PathBuf::from)
                    .join("mutants.out");
                std::fs::create_dir_all(&dir)?;
                std::fs::write(dir.join("outcomes.json"), json)?;
                return Ok(Output {
                    code,
                    stdout: String::new(),
                    stderr: String::new(),
                });
            }
            Ok(Output {
                code: 0,
                stdout: String::new(),
                stderr: String::new(),
            })
        }
    }

    const CACHE_LIST: &str = r#"[
      {"name":"m1","file":"src/lib.rs","package":"cachepkg","genre":"FnValue","replacement":"0",
       "span":{"start":{"line":1,"column":1},"end":{"line":1,"column":2}},
       "function":{"function_name":"f","return_type":"-> ()",
                   "span":{"start":{"line":1,"column":1},"end":{"line":3,"column":2}}}}
    ]"#;

    fn killed(name: &str) -> String {
        format!(
            r#"{{"outcomes":[{{"scenario":"Baseline","summary":"Success"}},{{"scenario":{{"Mutant":{{"name":"{name}"}}}},"summary":"CaughtMutant"}}]}}"#
        )
    }

    /// One `cargo mutants` answer that kills every name, since one call may carry several.
    fn killed_all(names: &[&str]) -> String {
        let records: Vec<String> = names
            .iter()
            .map(|name| {
                format!(
                    r#"{{"scenario":{{"Mutant":{{"name":"{name}"}}}},"summary":"CaughtMutant"}}"#
                )
            })
            .collect();
        format!(
            r#"{{"outcomes":[{{"scenario":"Baseline","summary":"Success"}},{}]}}"#,
            records.join(",")
        )
    }

    fn survived(name: &str) -> String {
        format!(
            r#"{{"outcomes":[{{"scenario":"Baseline","summary":"Success"}},{{"scenario":{{"Mutant":{{"name":"{name}"}}}},"summary":"MissedMutant"}}]}}"#
        )
    }

    fn cached_run(root: &Path, extra: &[&str]) -> Run {
        let mut argv = vec![
            "mutator".to_owned(),
            "--root".to_owned(),
            root.display().to_string(),
            "--cache".to_owned(),
        ];
        argv.extend(extra.iter().map(|arg| (*arg).to_owned()));
        Cli::parse_from(argv).run
    }

    #[test]
    fn a_second_unchanged_run_reuses_the_kill_without_rebuilding() {
        let root = cache_project("reuse");
        let first = execute_str(
            &cached_run(&root, &[]),
            &CacheFake::new(&root, vec![(0, killed("m1").as_str())]),
        );
        assert_eq!(first.0, 0, "{}", first.1);
        assert!(first.1.contains("KILLED"), "{}", first.1);

        let second = CacheFake::new(&root, vec![]);
        let (code, text) = execute_str(&cached_run(&root, &["--fail-on-survived"]), &second);
        assert_eq!(code, 0, "{text}");
        assert!(
            text.contains("KILLED"),
            "the reused verdict must still count: {text}"
        );
        assert!(text.contains("1 killed verdict reused"), "{text}");
        assert!(
            second.runs_started.borrow().is_empty(),
            "nothing should run"
        );
    }

    #[test]
    fn a_source_change_invalidates_the_entry() {
        let root = cache_project("invalidated");
        execute_str(
            &cached_run(&root, &[]),
            &CacheFake::new(&root, vec![(0, killed("m1").as_str())]),
        );
        std::fs::write(root.join("src/lib.rs"), "pub fn f() { let _ = 1; }\n").unwrap();

        let second = CacheFake::new(&root, vec![(0, killed("m1").as_str())]);
        let (code, text) = execute_str(&cached_run(&root, &[]), &second);
        assert_eq!(code, 0, "{text}");
        assert_eq!(
            second.runs_started.borrow().len(),
            1,
            "the change must force a run"
        );
        assert!(!text.contains("reused"), "{text}");
    }

    #[test]
    fn a_module_with_no_mutant_still_invalidates_the_package() {
        let root = cache_project("sibling-file");
        std::fs::write(root.join("src/lib.rs"), "pub mod other;\n\npub fn f() {}\n").unwrap();
        std::fs::write(root.join("src/other.rs"), "pub fn k() -> i32 { 1 }\n").unwrap();
        execute_str(
            &cached_run(&root, &[]),
            &CacheFake::new(&root, vec![(0, killed("m1").as_str())]),
        );
        // No mutant lives in `other.rs`, but a test of this package can compare against
        // what `k` returns, which decides whether `f`'s mutant is caught.
        std::fs::write(root.join("src/other.rs"), "pub fn k() -> i32 { -5 }\n").unwrap();

        let second = CacheFake::new(&root, vec![(0, killed("m1").as_str())]);
        let (code, text) = execute_str(&cached_run(&root, &[]), &second);
        assert_eq!(code, 0, "{text}");
        assert_eq!(
            second.runs_started.borrow().len(),
            1,
            "a file no mutant lives in still decides this mutant's verdict: {text}"
        );
        assert!(!text.contains("reused"), "{text}");
    }

    /// A workspace with `app` and `core`, where `app` depends on `core` by path.
    fn workspace_project(name: &str) -> PathBuf {
        let root = temp_root(&format!("cache-ws-{name}"));
        for package in ["app", "core"] {
            let dir = root.join("crates").join(package);
            std::fs::create_dir_all(dir.join("src")).unwrap();
            std::fs::write(dir.join("src/lib.rs"), format!("pub fn {package}() {{}}\n")).unwrap();
            let dependency = if package == "app" {
                "\n[dependencies]\ncore = { path = \"../core\" }\n"
            } else {
                ""
            };
            std::fs::write(
                dir.join("Cargo.toml"),
                format!(
                    "[package]\nname = \"{package}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n{dependency}"
                ),
            )
            .unwrap();
        }
        std::fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/app\", \"crates/core\"]\nresolver = \"3\"\n",
        )
        .unwrap();
        root
    }

    /// `cargo metadata` for `workspace_project`, with the dependency edge app -> core.
    fn workspace_metadata(root: &Path) -> String {
        let app = root.join("crates/app");
        let core = root.join("crates/core");
        let app_id = format!("path+file://{}#0.1.0", app.display());
        let core_id = format!("path+file://{}#0.1.0", core.display());
        format!(
            r#"{{"packages":[
                {{"id":"{app_id}","name":"app","source":null,
                  "manifest_path":"{}"}},
                {{"id":"{core_id}","name":"core","source":null,
                  "manifest_path":"{}"}}],
               "resolve":{{"nodes":[
                {{"id":"{app_id}","dependencies":["{core_id}"]}},
                {{"id":"{core_id}","dependencies":[]}}]}},
               "workspace_root":"{}"}}"#,
            app.join("Cargo.toml").display(),
            core.join("Cargo.toml").display(),
            root.display()
        )
    }

    /// One mutant in each package, named for the package it lives in.
    const WORKSPACE_LIST: &str = r#"[
      {"name":"app::m","file":"crates/app/src/lib.rs","package":"app","genre":"FnValue",
       "replacement":"0",
       "span":{"start":{"line":1,"column":1},"end":{"line":1,"column":2}},
       "function":{"function_name":"app","return_type":"-> ()",
                   "span":{"start":{"line":1,"column":1},"end":{"line":3,"column":2}}}},
      {"name":"core::m","file":"crates/core/src/lib.rs","package":"core","genre":"FnValue",
       "replacement":"0",
       "span":{"start":{"line":1,"column":1},"end":{"line":1,"column":2}},
       "function":{"function_name":"core","return_type":"-> ()",
                   "span":{"start":{"line":1,"column":1},"end":{"line":3,"column":2}}}}
    ]"#;

    #[test]
    fn a_dependency_change_invalidates_the_dependent_package_too() {
        let root = workspace_project("dep-change");
        let both = killed_all(&["app::m", "core::m"]);
        execute_str(
            &cached_run(&root, &[]),
            &CacheFake::workspace(
                &root,
                workspace_metadata(&root),
                WORKSPACE_LIST,
                vec![(0, both.as_str())],
            ),
        );
        std::fs::write(
            root.join("crates/core/src/lib.rs"),
            "pub fn core() { let _ = 1; }\n",
        )
        .unwrap();

        let second = CacheFake::workspace(
            &root,
            workspace_metadata(&root),
            WORKSPACE_LIST,
            vec![(0, both.as_str())],
        );
        let (code, text) = execute_str(&cached_run(&root, &[]), &second);
        assert_eq!(code, 0, "{text}");
        assert!(
            !text.contains("verdict reused"),
            "nothing may be reused: {text}"
        );
        let names: Vec<String> = second
            .runs_started
            .borrow()
            .iter()
            .flat_map(|args| selected_names(args))
            .collect();
        assert!(names.contains(&"app::m".to_owned()), "{names:?}");
        assert!(names.contains(&"core::m".to_owned()), "{names:?}");
    }

    #[test]
    fn a_dependency_test_file_invalidates_the_dependent_package() {
        let root = workspace_project("dep-test-change");
        let both = killed_all(&["app::m", "core::m"]);
        execute_str(
            &cached_run(&root, &[]),
            &CacheFake::workspace(
                &root,
                workspace_metadata(&root),
                WORKSPACE_LIST,
                vec![(0, both.as_str())],
            ),
        );
        // A dependency's `tests/` file can be reached as data: its library may
        // `include!` it, so it decides app's verdicts without any app file changing.
        std::fs::create_dir_all(root.join("crates/core/tests")).unwrap();
        std::fs::write(
            root.join("crates/core/tests/fixture.rs"),
            "pub fn k() -> i32 { 1 }\n",
        )
        .unwrap();

        let second = CacheFake::workspace(
            &root,
            workspace_metadata(&root),
            WORKSPACE_LIST,
            vec![(0, both.as_str())],
        );
        let (code, text) = execute_str(&cached_run(&root, &[]), &second);
        assert_eq!(code, 0, "{text}");
        let names: Vec<String> = second
            .runs_started
            .borrow()
            .iter()
            .flat_map(|args| selected_names(args))
            .collect();
        assert!(
            names.contains(&"app::m".to_owned()),
            "a dependency's test file can change app's verdicts: {names:?}"
        );
        assert!(
            names.contains(&"core::m".to_owned()),
            "core's own test file changes its verdicts: {names:?}"
        );
    }

    #[test]
    fn an_unrelated_package_still_reuses_its_verdicts() {
        let root = workspace_project("unrelated");
        let both = killed_all(&["app::m", "core::m"]);
        execute_str(
            &cached_run(&root, &[]),
            &CacheFake::workspace(
                &root,
                workspace_metadata(&root),
                WORKSPACE_LIST,
                vec![(0, both.as_str())],
            ),
        );
        // app depends on core, so changing app cannot change core's verdicts.
        std::fs::write(
            root.join("crates/app/src/lib.rs"),
            "pub fn app() { let _ = 1; }\n",
        )
        .unwrap();

        let second = CacheFake::workspace(
            &root,
            workspace_metadata(&root),
            WORKSPACE_LIST,
            vec![(0, killed("app::m").as_str())],
        );
        let (code, text) = execute_str(&cached_run(&root, &[]), &second);
        assert_eq!(code, 0, "{text}");
        assert!(text.contains("1 killed verdict reused"), "{text}");
        let names: Vec<String> = second
            .runs_started
            .borrow()
            .iter()
            .flat_map(|args| selected_names(args))
            .collect();
        assert_eq!(names, vec!["app::m".to_owned()], "only app must rerun");
    }

    #[test]
    fn a_build_configuration_change_invalidates_every_package() {
        let root = workspace_project("config-change");
        let both = killed_all(&["app::m", "core::m"]);
        execute_str(
            &cached_run(&root, &[]),
            &CacheFake::workspace(
                &root,
                workspace_metadata(&root),
                WORKSPACE_LIST,
                vec![(0, both.as_str())],
            ),
        );
        std::fs::create_dir_all(root.join(".cargo")).unwrap();
        std::fs::write(
            root.join(".cargo/config.toml"),
            "[build]\nrustflags = [\"--cfg\", \"extra\"]\n",
        )
        .unwrap();

        let second = CacheFake::workspace(
            &root,
            workspace_metadata(&root),
            WORKSPACE_LIST,
            vec![(0, both.as_str())],
        );
        let (code, text) = execute_str(&cached_run(&root, &[]), &second);
        assert_eq!(code, 0, "{text}");
        assert!(
            !text.contains("verdict reused"),
            "a new rustflag changes what every test builds: {text}"
        );
        assert_eq!(
            second.runs_started.borrow().len(),
            1,
            "every package must run again: {text}"
        );
    }

    #[test]
    fn a_package_whose_dependency_graph_is_unknown_is_never_reused() {
        let root = cache_project("opaque");
        let metadata = format!(
            r#"{{"packages":[{{"id":"path+file://{}#0.1.0","name":"cachepkg",
                "source":null,"manifest_path":"{}"}}],"resolve":null,
                "workspace_root":"{}"}}"#,
            root.display(),
            root.join("Cargo.toml").display(),
            root.display()
        );
        let first = CacheFake {
            metadata: Some(metadata.clone()),
            ..CacheFake::new(&root, vec![(0, killed("m1").as_str())])
        };
        let (code, text) = execute_str(&cached_run(&root, &[]), &first);
        assert_eq!(code, 0, "{text}");

        let second = CacheFake {
            metadata: Some(metadata),
            ..CacheFake::new(&root, vec![(0, killed("m1").as_str())])
        };
        let (code, text) = execute_str(&cached_run(&root, &["--fail-on-survived"]), &second);
        assert_eq!(code, 0, "{text}");
        assert_eq!(
            second.runs_started.borrow().len(),
            1,
            "an unknown closure must not reuse a verdict: {text}"
        );
        assert!(!text.contains("reused"), "{text}");
    }

    #[test]
    fn a_stored_survivor_runs_again_so_a_new_test_can_kill_it() {
        let root = cache_project("survivor");
        let first = execute_str(
            &cached_run(&root, &["--fail-on-survived"]),
            &CacheFake::new(&root, vec![(2, survived("m1").as_str())]),
        );
        assert_eq!(first.0, 2, "{}", first.1);

        let second = CacheFake::new(&root, vec![(0, killed("m1").as_str())]);
        let (code, text) = execute_str(&cached_run(&root, &["--fail-on-survived"]), &second);
        assert_eq!(code, 0, "{text}");
        assert_eq!(second.runs_started.borrow().len(), 1);
        assert!(text.contains("1 survivors decided again"), "{text}");
        assert!(
            text.contains(", 0 misses"),
            "the only pending mutant had a stored verdict, so nothing was a miss: {text}"
        );
    }

    #[test]
    fn without_the_flag_nothing_is_read_or_written() {
        let root = cache_project("disabled");
        let fake = CacheFake::new(&root, vec![(0, killed("m1").as_str())]);
        let (code, text) = execute_str(&run_at(&root, &[]), &fake);
        assert_eq!(code, 0, "{text}");
        assert!(!root.join(crate::cache::DIR).exists(), "no cache directory");
        assert!(!text.contains("cache:"), "{text}");
    }

    #[test]
    fn an_accepted_mutant_is_reused_too() {
        let root = cache_project("accepted");
        execute_str(
            &cached_run(&root, &[]),
            &CacheFake::new(&root, vec![(0, killed("m1").as_str())]),
        );

        let mut accepted = Accepted::default();
        accepted.insert(
            MutantId::new("src/lib.rs", "f", "FnValue", "0", 0),
            "why".to_owned(),
        );
        accepted.save(&root).unwrap();

        let second = CacheFake::new(&root, vec![]);
        let (code, text) = execute_str(&cached_run(&root, &["--fail-on-survived"]), &second);
        assert_eq!(code, 0, "{text}");
        assert!(text.contains("ACCEPTED"), "{text}");
        assert!(second.runs_started.borrow().is_empty());
    }

    #[test]
    fn a_test_only_diff_selects_its_packages_mutants_rather_than_passing() {
        let root = temp_root("test-only");
        let diff = root.join("x.diff");
        std::fs::write(&diff, TEST_DIFF).unwrap();
        let fake = Fake::new(LIST, vec![(0, BOTH)], vec![]);
        let (code, text) = execute_str(&run_at(&root, &["--diff", diff.to_str().unwrap()]), &fake);
        assert_eq!(code, 0, "{text}");
        assert!(
            text.starts_with("mutator: 2 of 2 mutants in scope (diff "),
            "{text}"
        );
        assert!(
            text.contains("selection: 1 test file changed, so every mutant of mutator is in scope"),
            "{text}"
        );
        assert_eq!(fake.run_calls().len(), 1, "both mutants must be run");
    }

    #[test]
    fn a_test_only_diff_reports_a_survivor_instead_of_silently_passing() {
        let root = temp_root("test-kills");
        let diff = root.join("x.diff");
        std::fs::write(&diff, TEST_DIFF).unwrap();
        let fake = Fake::new(LIST, vec![(2, ONE_MISSED)], vec![]);
        let (code, text) = execute_str(
            &run_at(
                &root,
                &["--diff", diff.to_str().unwrap(), "--fail-on-survived"],
            ),
            &fake,
        );
        assert_eq!(code, 2, "the survivor must fail the gate: {text}");
        assert!(text.contains("1 missed mutant"), "{text}");
    }

    #[test]
    fn a_test_diff_widens_the_scope_in_the_json_report() {
        let root = temp_root("test-only-json");
        let diff = root.join("x.diff");
        std::fs::write(&diff, TEST_DIFF).unwrap();
        let fake = Fake::new(LIST, vec![(0, BOTH)], vec![]);
        let (_, text) = execute_str(
            &run_at(
                &root,
                &["--diff", diff.to_str().unwrap(), "--reporter", "json"],
            ),
            &fake,
        );
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["selection"]["reason"], "Packages");
        assert_eq!(value["selection"]["packages"][0], "mutator");
        assert_eq!(value["rows"].as_array().map(Vec::len), Some(2));
    }

    #[test]
    fn a_deleted_test_file_widens_the_scope() {
        let root = temp_root("test-deleted");
        let diff = root.join("x.diff");
        std::fs::write(&diff, DELETED_TEST_DIFF).unwrap();
        let fake = Fake::new(LIST, vec![(0, BOTH)], vec![]);
        let (code, text) = execute_str(&run_at(&root, &["--diff", diff.to_str().unwrap()]), &fake);
        assert_eq!(code, 0, "{text}");
        assert!(
            text.contains("selection: 1 test file changed, so every mutant of mutator is in scope"),
            "{text}"
        );
        assert_eq!(fake.run_calls().len(), 1);
    }

    #[test]
    fn a_renamed_test_file_widens_the_scope() {
        let root = temp_root("test-renamed");
        let diff = root.join("x.diff");
        std::fs::write(&diff, RENAMED_TEST_DIFF).unwrap();
        let fake = Fake::new(LIST, vec![(0, BOTH)], vec![]);
        let (code, text) = execute_str(&run_at(&root, &["--diff", diff.to_str().unwrap()]), &fake);
        assert_eq!(code, 0, "{text}");
        assert!(
            text.contains(
                "selection: 2 test files changed, so every mutant of mutator is in scope"
            ),
            "{text}"
        );
        assert_eq!(fake.run_calls().len(), 1);
    }

    #[test]
    fn a_new_test_that_kills_a_missed_mutant_produces_an_updated_result() {
        let root = temp_root("test-kills-updated");
        let first = Fake::new(LIST, vec![(2, ONE_MISSED)], vec![]);
        let (code, text) = execute_str(&run_at(&root, &["--fail-on-survived"]), &first);
        assert_eq!(
            code, 2,
            "the mutant is missed before the test exists: {text}"
        );

        let diff = root.join("x.diff");
        std::fs::write(&diff, TEST_DIFF).unwrap();
        let second = Fake::new(LIST, vec![(0, BOTH)], vec![]);
        let (code, text) = execute_str(
            &run_at(
                &root,
                &["--diff", diff.to_str().unwrap(), "--fail-on-survived"],
            ),
            &second,
        );
        assert_eq!(code, 0, "the new test kills it, so the gate passes: {text}");
        assert!(text.contains("2 detected, 0 missed"), "{text}");
    }

    #[test]
    fn a_test_outside_every_package_selects_the_whole_workspace() {
        let root = temp_root("test-unowned");
        let diff = root.join("x.diff");
        std::fs::write(&diff, TEST_DIFF).unwrap();
        let fake = Fake {
            package_dir: "crates/app",
            ..Fake::new(LIST, vec![(0, BOTH)], vec![])
        };
        let (code, text) = execute_str(&run_at(&root, &["--diff", diff.to_str().unwrap()]), &fake);
        assert_eq!(code, 0, "{text}");
        assert!(
            text.contains("every mutant is in scope because tests/t.rs belongs to no package"),
            "{text}"
        );
        assert_eq!(fake.run_calls().len(), 1, "both mutants must be run");
    }

    #[test]
    fn a_production_diff_prints_no_selection_reason() {
        let root = temp_root("touched-only");
        let diff = root.join("x.diff");
        std::fs::write(&diff, DIFF).unwrap();
        let fake = Fake::new(LIST, vec![(0, BOTH)], vec![]);
        let (_, text) = execute_str(&run_at(&root, &["--diff", diff.to_str().unwrap()]), &fake);
        assert!(!text.contains("selection:"), "{text}");
    }

    #[test]
    fn a_signature_change_selects_the_package() {
        let root = temp_root("signature");
        let diff = root.join("x.diff");
        std::fs::write(
            &diff,
            "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n\
             @@ -5,1 +5,1 @@\n-fn parse(s: &str) -> u32 {\n+fn parse(s: String) -> u64 {\n",
        )
        .unwrap();
        let fake = Fake::new(LIST, vec![(0, BOTH)], vec![]);
        let (code, text) = execute_str(&run_at(&root, &["--diff", diff.to_str().unwrap()]), &fake);
        assert_eq!(code, 0, "{text}");
        assert!(
            text.contains(
                "selection: 1 source file changed, so every mutant of mutator is in scope"
            ),
            "{text}"
        );
        assert_eq!(fake.run_calls().len(), 1, "both mutants must be run");
    }

    #[test]
    fn a_build_script_change_selects_the_package() {
        let root = temp_root("build-script");
        let diff = root.join("x.diff");
        std::fs::write(
            &diff,
            "diff --git a/build.rs b/build.rs\n--- a/build.rs\n+++ b/build.rs\n@@ -1,1 +1,1 @@\n-a\n+b\n",
        )
        .unwrap();
        let fake = Fake::new(LIST, vec![(0, BOTH)], vec![]);
        let (code, text) = execute_str(&run_at(&root, &["--diff", diff.to_str().unwrap()]), &fake);
        assert_eq!(code, 0, "{text}");
        assert!(
            text.contains(
                "selection: 1 build script changed, so every mutant of mutator is in scope"
            ),
            "{text}"
        );
        assert_eq!(fake.run_calls().len(), 1);
    }

    #[test]
    fn a_shared_configuration_change_selects_the_whole_workspace() {
        let root = temp_root("config");
        let diff = root.join("x.diff");
        std::fs::write(
            &diff,
            "diff --git a/.cargo/config.toml b/.cargo/config.toml\n--- a/.cargo/config.toml\n\
             +++ b/.cargo/config.toml\n@@ -1,1 +1,1 @@\n-a\n+b\n",
        )
        .unwrap();
        let fake = Fake {
            package_dir: "crates/app",
            ..Fake::new(LIST, vec![(0, BOTH)], vec![])
        };
        let (code, text) = execute_str(&run_at(&root, &["--diff", diff.to_str().unwrap()]), &fake);
        assert_eq!(code, 0, "{text}");
        assert!(
            text.contains(
                "every mutant is in scope because .cargo/config.toml belongs to no package"
            ),
            "{text}"
        );
        assert_eq!(fake.run_calls().len(), 1);
    }

    #[test]
    fn a_renamed_source_file_selects_both_packages() {
        let root = temp_root("source-renamed");
        let diff = root.join("x.diff");
        std::fs::write(
            &diff,
            "diff --git a/src/parse.rs b/src/scan.rs\nsimilarity index 100%\n\
             rename from src/parse.rs\nrename to src/scan.rs\n",
        )
        .unwrap();
        let fake = Fake::new(LIST, vec![(0, BOTH)], vec![]);
        let (code, text) = execute_str(&run_at(&root, &["--diff", diff.to_str().unwrap()]), &fake);
        assert_eq!(code, 0, "{text}");
        assert!(
            text.contains(
                "selection: 2 source files changed, so every mutant of mutator is in scope"
            ),
            "{text}"
        );
        assert_eq!(fake.run_calls().len(), 1);
    }

    #[test]
    fn list_explains_a_widened_selection_on_stderr() {
        let root = temp_root("list-reason");
        let diff = root.join("x.diff");
        std::fs::write(&diff, TEST_DIFF).unwrap();
        let fake = Fake::new(LIST, vec![], vec![]);
        let run = run_at(&root, &["--diff", diff.to_str().unwrap(), "--list"]);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = execute(&run, &fake, &mut out, &mut err).unwrap();
        assert_eq!(code, 0);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "src/lib.rs:3:5: replace parse -> u32 with 0\n\
             src/lib.rs:20:5: replace render -> u32 with 0\n"
        );
        assert_eq!(
            String::from_utf8(err).unwrap(),
            "selection: 1 test file changed, so every mutant of mutator is in scope\n"
        );
    }

    #[test]
    fn a_malformed_diff_is_reported_before_cargo_mutants_is_asked_anything() {
        let root = temp_root("bad-diff");
        let diff = root.join("x.diff");
        std::fs::write(
            &diff,
            "diff --git a/a.rs b/a.rs\n+++ b/a.rs\n@@ broken @@\n",
        )
        .unwrap();
        let fake = Fake::new(LIST, vec![], vec![]);
        let run = run_at(&root, &["--diff", diff.to_str().unwrap(), "--list"]);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let error = execute(&run, &fake, &mut out, &mut err).unwrap_err();
        assert!(
            matches!(&error, crate::error::Error::DiffFile { source, .. } if source.text == "@@ broken @@"),
            "{error:#}"
        );
        assert_eq!(
            fake.calls(),
            Vec::<Vec<String>>::new(),
            "the diff must be parsed before any process runs"
        );
    }

    #[test]
    fn list_stays_quiet_when_the_diff_decided_alone() {
        let root = temp_root("list-quiet");
        let diff = root.join("x.diff");
        std::fs::write(&diff, DIFF).unwrap();
        let fake = Fake::new(LIST, vec![], vec![]);
        let run = run_at(&root, &["--diff", diff.to_str().unwrap(), "--list"]);
        let mut out = Vec::new();
        let mut err = Vec::new();
        assert_eq!(execute(&run, &fake, &mut out, &mut err).unwrap(), 0);
        assert_eq!(String::from_utf8(err).unwrap(), "");
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "src/lib.rs:20:5: replace render -> u32 with 0\n"
        );
    }

    #[test]
    fn a_new_test_in_a_package_forces_a_rerun_so_a_missed_mutant_can_be_killed() {
        let root = cache_project("test-only-cache");
        execute_str(
            &cached_run(&root, &["--fail-on-survived"]),
            &CacheFake::new(&root, vec![(2, survived("m1").as_str())]),
        );

        std::fs::create_dir_all(root.join("tests")).unwrap();
        std::fs::write(root.join("tests/t.rs"), "#[test]\nfn t() {}\n").unwrap();
        let diff = root.join("x.diff");
        std::fs::write(&diff, TEST_DIFF).unwrap();

        let second = CacheFake::new(&root, vec![(0, killed("m1").as_str())]);
        let (code, text) = execute_str(
            &cached_run(
                &root,
                &["--diff", diff.to_str().unwrap(), "--fail-on-survived"],
            ),
            &second,
        );
        assert_eq!(code, 0, "{text}");
        assert_eq!(
            second.runs_started.borrow().len(),
            1,
            "the changed test must force a run: {text}"
        );
        assert!(!text.contains("reused"), "{text}");
    }

    #[test]
    fn a_test_outside_every_package_reuses_no_stored_verdict() {
        let root = cache_project("test-unowned-cache");
        let diff = root.join("x.diff");
        std::fs::write(&diff, TEST_DIFF).unwrap();
        let first = CacheFake {
            package_dir: "crates/app",
            ..CacheFake::new(&root, vec![(0, killed("m1").as_str())])
        };
        let (code, text) = execute_str(
            &cached_run(&root, &["--diff", diff.to_str().unwrap()]),
            &first,
        );
        assert_eq!(code, 0, "{text}");

        let second = CacheFake {
            package_dir: "crates/app",
            ..CacheFake::new(&root, vec![(0, killed("m1").as_str())])
        };
        let (code, text) = execute_str(
            &cached_run(
                &root,
                &["--diff", diff.to_str().unwrap(), "--fail-on-survived"],
            ),
            &second,
        );
        assert_eq!(code, 0, "{text}");
        assert_eq!(
            second.runs_started.borrow().len(),
            1,
            "a path no package fingerprints must not be settled from the store: {text}"
        );
        assert!(!text.contains("reused"), "{text}");
    }

    #[test]
    fn a_long_selection_is_split_into_chunks() {
        let mutants: Vec<String> = (0..=CHUNK)
            .map(|index| {
                format!(
                    r#"{{"name":"n{index}","file":"src/lib.rs","package":"p","genre":"FnValue","replacement":"{index}","span":{{"start":{{"line":{},"column":1}},"end":{{"line":{},"column":2}}}}}}"#,
                    index + 1,
                    index + 1
                )
            })
            .collect();
        let list = format!("[{}]", mutants.join(","));
        let root = temp_root("chunk");
        let fake = Fake::catching_everything(&list);
        let (code, text) = execute_str(&run_at(&root, &[]), &fake);
        assert_eq!(code, 0, "{text}");
        let chunk_sizes: Vec<usize> = fake
            .run_calls()
            .iter()
            .map(|args| selected_names(args).len())
            .collect();
        assert_eq!(chunk_sizes, vec![CHUNK, 1]);
        assert!(text.contains(&format!("{} of {} mutants in scope", CHUNK + 1, CHUNK + 1)));
    }
}

#[cfg(test)]
mod dispatch_tests {
    use super::*;
    use crate::exec::{Exec, Output};
    use clap::Parser;
    use std::cell::RefCell;

    /// Serves a fixed list and writes a fixed outcomes file for any run.
    struct Scripted {
        list: &'static str,
        outcome: &'static str,
        code: i32,
        git: RefCell<Vec<Output>>,
    }

    impl Exec for Scripted {
        fn run(&self, program: &str, args: &[String], _cwd: &Path) -> Result<Output> {
            if program == "git" {
                return Ok(self.git.borrow_mut().remove(0));
            }
            if args.iter().any(|arg| arg == "--list") {
                return Ok(Output {
                    code: 0,
                    stdout: self.list.to_owned(),
                    stderr: String::new(),
                });
            }
            let dir = args
                .iter()
                .position(|arg| arg == "--output")
                .and_then(|index| args.get(index + 1))
                .map_or_else(std::path::PathBuf::new, std::path::PathBuf::from)
                .join("mutants.out");
            std::fs::create_dir_all(&dir)?;
            std::fs::write(dir.join("outcomes.json"), self.outcome)?;
            Ok(Output {
                code: self.code,
                stdout: String::new(),
                stderr: String::new(),
            })
        }
    }

    const ONE: &str = r#"[
      {"name":"src/lib.rs:3:5: replace parse -> u32 with 0","file":"src/lib.rs","package":"mutator",
       "genre":"FnValue","replacement":"0",
       "span":{"start":{"line":3,"column":5},"end":{"line":3,"column":9}},
       "function":{"function_name":"parse","return_type":"-> u32",
                   "span":{"start":{"line":3,"column":1},"end":{"line":7,"column":2}}}}
    ]"#;

    const CAUGHT: &str = r#"{"outcomes":[{"scenario":"Baseline","summary":"Success"},
      {"scenario":{"Mutant":{"name":"src/lib.rs:3:5: replace parse -> u32 with 0"}},"summary":"CaughtMutant"}]}"#;

    fn temp_root(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("mutator-dispatch-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn scripted() -> Scripted {
        Scripted {
            list: ONE,
            outcome: CAUGHT,
            code: 0,
            git: RefCell::new(vec![
                Output {
                    code: 0,
                    stdout: "abc\n".to_owned(),
                    stderr: String::new(),
                },
                Output {
                    code: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                },
            ]),
        }
    }

    fn cli_at(root: &Path, extra: &[&str]) -> Cli {
        let mut argv = vec![
            "mutator".to_owned(),
            "--root".to_owned(),
            root.display().to_string(),
        ];
        argv.extend(extra.iter().map(|arg| (*arg).to_owned()));
        Cli::parse_from(argv)
    }

    #[test]
    fn dispatch_returns_the_run_code_and_the_report() {
        let root = temp_root("ok");
        let mut out = Vec::new();
        let mut err = Vec::new();
        assert_eq!(
            dispatch(&cli_at(&root, &[]), &scripted(), &mut out, &mut err),
            0
        );
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("1 of 1 mutants in scope"), "{text}");
    }

    #[test]
    fn dispatch_maps_an_error_to_the_usage_code() {
        let root = temp_root("error");
        let mut out = Vec::new();
        let mut err = Vec::new();
        let failing = Scripted {
            git: RefCell::new(vec![Output {
                code: 128,
                stdout: String::new(),
                stderr: "not a git repository".to_owned(),
            }]),
            ..scripted()
        };
        let code = dispatch(
            &cli_at(&root, &["--since", "nope"]),
            &failing,
            &mut out,
            &mut err,
        );
        assert_eq!(code, 1);
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.starts_with("mutator: git merge-base nope HEAD failed"),
            "{text}"
        );
    }

    #[test]
    fn dispatch_runs_the_accept_subcommand() {
        let root = temp_root("accept");
        let mut out = Vec::new();
        let mut err = Vec::new();
        let cli = Cli::parse_from([
            "mutator",
            "accept",
            "src/lib.rs::parse::FnValue::0#0",
            "--reason",
            "unreachable",
            "--root",
            root.to_str().unwrap(),
        ]);
        assert_eq!(dispatch(&cli, &scripted(), &mut out, &mut err), 0);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "accepted src/lib.rs::parse::FnValue::0#0\n"
        );
    }

    #[test]
    fn verbose_commands_go_to_stderr() {
        let root = temp_root("verbose");
        let mut out = Vec::new();
        let mut err = Vec::new();
        assert_eq!(
            dispatch(
                &cli_at(&root, &["--verbose"]),
                &scripted(),
                &mut out,
                &mut err
            ),
            0
        );
        let echoed = String::from_utf8(err).unwrap();
        assert!(
            echoed.starts_with("+ cargo mutants --list --json\n"),
            "{echoed}"
        );
        assert!(echoed.contains("+ cargo mutants --no-times"), "{echoed}");
        assert!(
            !String::from_utf8(out).unwrap().contains("+ cargo"),
            "stdout stays parseable"
        );
    }

    #[test]
    fn quiet_runs_echo_nothing() {
        let root = temp_root("quiet");
        let mut out = Vec::new();
        let mut err = Vec::new();
        assert_eq!(
            dispatch(&cli_at(&root, &[]), &scripted(), &mut out, &mut err),
            0
        );
        assert_eq!(String::from_utf8(err).unwrap(), "");
    }

    #[test]
    fn the_main_entry_point_reports_a_usage_code_for_a_bad_percentage() {
        assert!(Cli::try_parse_from(["mutator", "--threshold", "101"]).is_err());
        assert!(Cli::try_parse_from(["mutator", "--threshold", "50.5"]).is_ok());
    }

    #[test]
    fn exit_codes_map_to_the_process_code() {
        assert_eq!(into_exit_code(0), std::process::ExitCode::SUCCESS);
        assert_eq!(into_exit_code(2), std::process::ExitCode::from(2));
        assert_eq!(into_exit_code(256), std::process::ExitCode::FAILURE);
        assert_eq!(into_exit_code(-1), std::process::ExitCode::FAILURE);
    }
}
