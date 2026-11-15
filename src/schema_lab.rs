// Copyright 2026 Mathis Groult
// KvasirTest addition to cargo-mutants (MIT): see docs/decisions/0001-mutant-schemata.md.

//! Test mutants through one schema build.
//!
//! 1. Every mutated file is rewritten as a schema holding all its mutants ([`crate::schema`]).
//! 2. `cargo check` finds mutants the schema cannot hold; they are taken out (and later run the
//!    classic way) until the schema compiles.
//! 3. The baseline builds the schema and runs the tests with no mutant active.
//! 4. Each mutant is a `cargo test` with [`ACTIVE_MUTANT_VAR`] set: nothing is rebuilt, and several
//!    run at once (cargo holds no lock while test binaries run).
//! 5. The original files are restored, then the left-out mutants run one build each.

use std::collections::{BTreeMap, BTreeSet};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

use anyhow::{Context as _, bail};
use camino::{Utf8Path, Utf8PathBuf};
use itertools::Itertools;
use jiff::Timestamp;
use tracing::{debug, error, info, warn};

use crate::build_dir::BuildDir;
use crate::cargo::cargo_argv;
use crate::console::Console;
use crate::lab::{Lab, TestsForMutant, Worker, join_threads};
use crate::options::Options;
use crate::outcome::{LabOutcome, Phase, ScenarioOutcome};
use crate::output::OutputDir;
use crate::package::{Package, PackageSelection};
use crate::schema::{ACTIVE_MUTANT_VAR, SchemaMutant, render};
use crate::timeouts::Timeouts;
use crate::workspace::Workspace;
use crate::{Mutant, Result, Scenario, cargo::run_cargo};

/// Like [`crate::lab::test_mutants`], through a schema build.
pub fn test_mutants(
    mutants: &[Mutant],
    workspace: &Workspace,
    output_dir: OutputDir,
    options: &Options,
    console: &Console,
) -> Result<LabOutcome> {
    let start_time = Instant::now();
    console.set_debug_log(output_dir.open_debug_log()?);
    output_dir.write_mutants_list(mutants)?;
    console.discovered_mutants(mutants);
    if mutants.is_empty() {
        warn!("No mutants found under the active filters");
        return Ok(LabOutcome::new(Timestamp::now()));
    }
    let build_dir = BuildDir::for_baseline(workspace, options, console)?;
    let lab = Lab {
        output_mutex: Mutex::new(output_dir),
        jobserver: None,
        tests_for_mutant: TestsForMutant::new(options, workspace),
        options,
        console,
    };

    let prepare_start = Instant::now();
    let prepared = prepare(&build_dir, mutants, options)?;
    info!(
        in_schema = prepared.active.len(),
        left_out = prepared.left_out.len(),
        rounds = prepared.rounds,
        elapsed = ?prepare_start.elapsed(),
        "schema ready"
    );

    let baseline = lab.run_baseline(&build_dir, mutants)?;
    if !baseline.success() {
        error!(
            "cargo {phase} failed on the schema with no mutant active, so no mutants were tested",
            phase = baseline.last_phase(),
        );
        drop(prepared);
        return lab
            .output_mutex
            .into_inner()
            .expect("lock output_dir")
            .finish();
    }
    let timeouts = Timeouts::from_baseline(&baseline, options);
    console.start_testing_mutants(mutants.len());

    let n_workers = schema_jobs(options).min(prepared.active.len()).max(1);
    let queue = Mutex::new(prepared.active.clone().into_iter());
    thread::scope(|scope| -> Result<()> {
        let workers = (0..n_workers)
            .map(|i| {
                let worker = lab.make_worker(&build_dir);
                let queue = &queue;
                scope.spawn(move || run_schema_queue(&worker, i, queue, timeouts))
            })
            .collect_vec();
        join_threads(workers)
    })?;

    let left_out = prepared.restore()?;
    if !left_out.is_empty() {
        info!(
            count = left_out.len(),
            "testing left-out mutants one build each"
        );
        lab.run_queue(&build_dir, timeouts, &Mutex::new(left_out.into_iter()))?;
    }

    let output_dir = lab.output_mutex.into_inner().expect("final unlock");
    console.lab_finished(&output_dir.lab_outcome, start_time, options);
    output_dir.finish()
}

/// CPUs minus 2 (the host keeps two for everything else), at least 1, unless set.
fn schema_jobs(options: &Options) -> usize {
    options.schema_jobs.unwrap_or_else(|| {
        thread::available_parallelism()
            .map_or(1, std::num::NonZero::get)
            .saturating_sub(2)
            .max(1)
    })
}

fn run_schema_queue(
    worker: &Worker<'_>,
    worker_index: usize,
    queue: &Mutex<std::vec::IntoIter<(u32, Mutant)>>,
    timeouts: Timeouts,
) -> Result<()> {
    // The console tells running scenarios apart by directory: give each worker its own label.
    let label = worker
        .build_dir
        .path()
        .join(format!("[schema worker {worker_index}]"));
    loop {
        let Some((id, mutant)) = queue.lock().expect("lock schema queue").next() else {
            return Ok(());
        };
        run_schema_mutant(worker, &label, id, mutant, timeouts)?;
    }
}

/// Run the tests with one mutant switched on; nothing is rebuilt.
fn run_schema_mutant(
    worker: &Worker<'_>,
    label: &Utf8Path,
    id: u32,
    mutant: Mutant,
    timeouts: Timeouts,
) -> Result<ScenarioOutcome> {
    let test_packages = match worker.tests_for_mutant {
        TestsForMutant::Workspace => PackageSelection::All,
        TestsForMutant::Mutated => {
            PackageSelection::Explicit(vec![mutant.source_file.package.clone()])
        }
        TestsForMutant::Explicit(packages) => PackageSelection::Explicit(packages.clone()),
    };
    let diff = mutant.diff(&mutant.mutated_code());
    let scenario = Scenario::Mutant(mutant);
    let mut scenario_output = worker
        .output_mutex
        .lock()
        .expect("lock output_dir to start scenario")
        .start_scenario(&scenario)?;
    scenario_output.write_diff(&diff)?;
    scenario_output.message(&format!("schema mutant {id}: {ACTIVE_MUTANT_VAR}={id}"))?;
    worker
        .console
        .scenario_started(label, &scenario, scenario_output.open_log_read()?);
    let mut outcome = ScenarioOutcome::new(&scenario_output, scenario.clone());
    worker.console.scenario_phase_started(label, Phase::Test);
    let phase_result = run_cargo(
        worker.build_dir,
        worker.jobserver,
        &test_packages,
        Phase::Test,
        timeouts.test,
        &mut scenario_output,
        worker.options,
        worker.console,
        &[(ACTIVE_MUTANT_VAR.to_owned(), id.to_string())],
    )?;
    outcome.add_phase_result(phase_result);
    worker.console.scenario_phase_finished(label, Phase::Test);
    worker
        .output_mutex
        .lock()
        .expect("lock output dir to add outcome")
        .add_scenario_outcome(&outcome)?;
    debug!(id, outcome = ?outcome.summary());
    worker
        .console
        .scenario_finished(label, &scenario, &outcome, worker.options);
    Ok(outcome)
}

/// The schema written into the build directory. Dropping it restores the original files.
struct Prepared<'a> {
    build_dir: &'a BuildDir,
    /// Original text of every rewritten file, by tree-relative path.
    originals: BTreeMap<Utf8PathBuf, Arc<String>>,
    /// Mutants held by the schema, with their switch id.
    active: Vec<(u32, Mutant)>,
    /// Mutants the schema could not hold.
    left_out: Vec<Mutant>,
    rounds: usize,
    restored: bool,
}

impl Prepared<'_> {
    /// Put the original files back and hand over the left-out mutants.
    fn restore(mut self) -> Result<Vec<Mutant>> {
        self.write_originals()?;
        Ok(std::mem::take(&mut self.left_out))
    }

    fn write_originals(&mut self) -> Result<()> {
        for (path, code) in &self.originals {
            self.build_dir.overwrite_file(path, code)?;
        }
        self.restored = true;
        Ok(())
    }
}

impl Drop for Prepared<'_> {
    fn drop(&mut self) {
        if !self.restored
            && let Err(err) = self.write_originals()
        {
            error!(
                ?err,
                "failed to restore the original source files after a schema run"
            );
        }
    }
}

/// Write the schema and take out mutants until `cargo check` accepts it.
fn prepare<'a>(
    build_dir: &'a BuildDir,
    mutants: &[Mutant],
    options: &Options,
) -> Result<Prepared<'a>> {
    let ids: Vec<u32> = (1..=u32::try_from(mutants.len()).context("too many mutants")?).collect();
    let by_file: BTreeMap<Utf8PathBuf, Vec<usize>> = mutants
        .iter()
        .enumerate()
        .map(|(i, m)| (m.source_file.tree_relative_path.clone(), i))
        .into_group_map()
        .into_iter()
        .collect();
    let packages: Vec<Arc<Package>> = mutants
        .iter()
        .map(|m| Arc::clone(&m.source_file.package))
        .sorted_by_key(|p| p.name.clone())
        .dedup_by(|a, b| a.name == b.name)
        .collect();
    let mut prepared = Prepared {
        build_dir,
        originals: by_file
            .iter()
            .map(|(path, indexes)| (path.clone(), mutants[indexes[0]].source_file.code.clone()))
            .collect(),
        active: Vec::new(),
        left_out: Vec::new(),
        rounds: 0,
        restored: false,
    };
    let mut out: BTreeSet<usize> = BTreeSet::new();
    loop {
        prepared.rounds += 1;
        let mut schemas = BTreeMap::new();
        for (path, indexes) in &by_file {
            let code = &prepared.originals[path];
            let in_schema = indexes
                .iter()
                .filter(|i| !out.contains(i))
                .map(|&i| SchemaMutant {
                    id: ids[i],
                    span: mutants[i].span,
                    replacement: &mutants[i].replacement,
                    site: mutants[i].site,
                })
                .collect_vec();
            let schema =
                render(code, &in_schema).with_context(|| format!("render the schema of {path}"))?;
            out.extend(schema.rejected.iter().map(|&id| id as usize - 1));
            build_dir.overwrite_file(path, &schema.code)?;
            schemas.insert(path.clone(), schema);
        }
        let errors = check(build_dir, &packages, options)?;
        if errors.is_empty() {
            break;
        }
        let blamed: BTreeSet<usize> = errors
            .iter()
            .filter_map(|e| schemas.get(&e.file).map(|s| s.blame(&e.range)))
            .flatten()
            .map(|&id| id as usize - 1)
            .collect();
        debug!(
            round = prepared.rounds,
            errors = errors.len(),
            blamed = blamed.len()
        );
        if blamed.is_empty() || blamed.is_subset(&out) {
            warn!(
                errors = errors.len(),
                first = ?errors.first(),
                "schema errors not attributable to any mutant; testing all mutants one build each"
            );
            out.extend(0..mutants.len());
            prepared.write_originals()?;
            prepared.restored = false;
            break;
        }
        out.extend(blamed);
    }
    prepared.active = (0..mutants.len())
        .filter(|i| !out.contains(i))
        .map(|i| (ids[i], mutants[i].clone()))
        .collect();
    prepared.left_out = out.iter().map(|&i| mutants[i].clone()).collect();
    Ok(prepared)
}

/// A compiler error located in a source file of the tree.
#[derive(Debug)]
struct CompileError {
    file: Utf8PathBuf,
    range: std::ops::Range<usize>,
}

/// Run `cargo check --tests` on the packages; return the errors (empty: it compiled).
fn check(
    build_dir: &BuildDir,
    packages: &[Arc<Package>],
    options: &Options,
) -> Result<Vec<CompileError>> {
    let mut argv = cargo_argv(
        &PackageSelection::Explicit(packages.to_vec()),
        Phase::Check,
        options,
    );
    argv.push("--message-format=json".to_owned());
    debug!(?argv, "schema check");
    let output = Command::new(&argv[0])
        .args(&argv[1..])
        .current_dir(build_dir.path())
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .with_context(|| format!("run {}", argv.join(" ")))?;
    let errors = parse_errors(&String::from_utf8_lossy(&output.stdout), build_dir.path());
    if !output.status.success() && errors.is_empty() {
        bail!("cargo check failed on the schema without a located error: {argv:?}");
    }
    Ok(errors)
}

/// Primary spans of error-level compiler messages, with paths relative to `root`.
fn parse_errors(json_lines: &str, root: &Utf8Path) -> Vec<CompileError> {
    json_lines
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|v| v["reason"] == "compiler-message" && v["message"]["level"] == "error")
        .flat_map(|v| {
            v["message"]["spans"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter(|s| s["is_primary"] == true)
                .filter_map(|s| {
                    let name = Utf8PathBuf::from(s["file_name"].as_str()?);
                    let file = name
                        .strip_prefix(root)
                        .map_or(name.clone(), Utf8Path::to_owned);
                    let start = usize::try_from(s["byte_start"].as_u64()?).ok()?;
                    let end = usize::try_from(s["byte_end"].as_u64()?).ok()?;
                    Some(CompileError {
                        file,
                        range: start..end,
                    })
                })
                .collect_vec()
        })
        .collect()
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn errors_are_primary_spans_relative_to_the_tree() {
        let json = r#"{"reason":"compiler-artifact","target":{}}
{"reason":"compiler-message","message":{"level":"warning","spans":[{"file_name":"src/a.rs","byte_start":1,"byte_end":2,"is_primary":true}]}}
{"reason":"compiler-message","message":{"level":"error","spans":[{"file_name":"/tree/src/a.rs","byte_start":10,"byte_end":14,"is_primary":true},{"file_name":"src/b.rs","byte_start":0,"byte_end":1,"is_primary":false}]}}
{"reason":"compiler-message","message":{"level":"error","spans":[]}}
not json
"#;
        let errors = parse_errors(json, Utf8Path::new("/tree"));
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].file, Utf8PathBuf::from("src/a.rs"));
        assert_eq!(errors[0].range, 10..14);
    }
}
