// Copyright 2026 Mathis Groult
// SPDX-License-Identifier: GPL-3.0-or-later (KvasirTest; see NOTICE)

//! Test mutants through one schema build.
//!
//! 1. Every mutated file is rewritten as a schema holding all its mutants ([`crate::schema`]).
//! 2. `cargo check` finds mutants the schema cannot hold; they are taken out (and later run the
//!    classic way) until the schema compiles. Const contexts are left out from the start.
//! 3. The baseline builds the test executables once and runs every test in its own process with
//!    reach recording on: each test's reach is the set of guards it evaluated.
//! 4. Each mutant runs only the tests that reach it, directly (no cargo), fastest first, and stops
//!    at the first failure. A mutant no test reaches is missed without running: a guard that is
//!    never evaluated cannot change what a test sees.
//! 5. The original files are restored, then the left-out mutants run one build each.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use camino::{Utf8Path, Utf8PathBuf};
use itertools::Itertools;
use jiff::Timestamp;
use tracing::{debug, error, info, warn};

use crate::build_dir::BuildDir;
use crate::console::Console;
use crate::lab::{Lab, TestsForMutant, join_threads};
use crate::options::Options;
use crate::outcome::{LabOutcome, Phase, PhaseResult, ScenarioOutcome};
use crate::output::{OutputDir, ScenarioOutput};
use crate::package::Package;
use crate::process::Exit;
use crate::schema::{
    ACTIVE_MUTANT_VAR, REACH_FILE_VAR, RegionKind, SchemaMutant, parse_reach, render,
};
use crate::schema_runner::{
    Compiled, Executable, TestArgs, Unit, build, list_units, parse_executables,
};
use crate::timeouts::Timeouts;
use crate::workspace::Workspace;
use crate::{Mutant, Result, Scenario};

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

    let args = TestArgs::parse(&options.additional_cargo_test_args);
    let packages = test_packages(&lab.tests_for_mutant, workspace, mutants);
    let n_workers = schema_jobs(options);
    let label = build_dir.path();
    let mut output = lab
        .output_mutex
        .lock()
        .expect("lock output_dir")
        .start_scenario(&Scenario::Baseline)?;
    console.scenario_started(label, &Scenario::Baseline, output.open_log_read()?);
    let mut outcome = ScenarioOutcome::new(&output, Scenario::Baseline);

    // The schema is compiled by the real test build, so the build that accepts it is the one
    // the baseline and the mutants use.
    console.scenario_phase_started(label, Phase::Build);
    let prepare_start = Instant::now();
    let compile_time = workspace.compile_time_packages();
    let (prepared, compiled) = prepare(&build_dir, mutants, &compile_time, &mut || {
        build(
            build_dir.path(),
            &packages,
            &args,
            options,
            options.build_timeout,
            &[],
            &mut output,
        )
    })?;
    outcome.add_phase_result(PhaseResult {
        phase: Phase::Build,
        duration: compiled.duration,
        process_status: compiled.exit,
        argv: vec!["cargo".into(), "test".into(), "--no-run".into()],
    });
    console.scenario_phase_finished(label, Phase::Build);
    info!(
        in_schema = prepared.active.len(),
        left_out = prepared.left_out.len(),
        rounds = prepared.rounds,
        elapsed = ?prepare_start.elapsed(),
        "schema ready"
    );

    let baseline_start = Instant::now();
    let baseline = if compiled.exit.is_success() {
        let executables = if args.only_doc() {
            Vec::new()
        } else {
            parse_executables(&compiled.json)
        };
        run_baseline(
            &lab,
            &build_dir,
            &packages,
            &args,
            n_workers,
            &executables,
            output,
            outcome,
        )?
    } else {
        finish_baseline(&lab, label, &outcome)?;
        None
    };
    let Some(baseline) = baseline else {
        error!(
            "the schema with no mutant active did not build or pass its tests, so no mutants were tested"
        );
        drop(prepared);
        return lab
            .output_mutex
            .into_inner()
            .expect("lock output_dir")
            .finish();
    };
    info!(
        units = baseline.units.len(),
        elapsed = ?baseline_start.elapsed(),
        "baseline and reach recorded"
    );
    console.start_testing_mutants(mutants.len());

    let mutants_start = Instant::now();
    let mut prepared = prepared;
    let queue = Mutex::new(prepared.active.clone().into_iter());
    thread::scope(|scope| -> Result<()> {
        let workers = (0..n_workers.min(prepared.active.len()).max(1))
            .map(|i| {
                let (lab, build_dir, queue, baseline, args) =
                    (&lab, &build_dir, &queue, &baseline, &args);
                scope.spawn(move || {
                    let label = build_dir.path().join(format!("[schema worker {i}]"));
                    loop {
                        let Some((id, mutant)) = queue.lock().expect("lock schema queue").next()
                        else {
                            return Ok(());
                        };
                        run_schema_mutant(lab, &label, id, mutant, baseline, args)?;
                    }
                })
            })
            .collect_vec();
        join_threads(workers)
    })?;
    info!(elapsed = ?mutants_start.elapsed(), "schema mutants tested");
    for (mutant, errors) in std::mem::take(&mut prepared.unviable) {
        record_unviable(&lab, label, mutant, &errors)?;
    }

    let left_out = prepared.restore()?;
    if !left_out.is_empty() {
        info!(
            count = left_out.len(),
            "testing left-out mutants one build each"
        );
        lab.run_queue(
            &build_dir,
            baseline.timeouts,
            &Mutex::new(left_out.into_iter()),
        )?;
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

/// The packages whose tests the run may use, as the classic mode chooses them.
fn test_packages(
    tests_for_mutant: &TestsForMutant,
    workspace: &Workspace,
    mutants: &[Mutant],
) -> Vec<Arc<Package>> {
    match tests_for_mutant {
        TestsForMutant::Workspace => workspace.packages().to_vec(),
        TestsForMutant::Mutated => mutants
            .iter()
            .map(|m| Arc::clone(&m.source_file.package))
            .sorted_by_key(|p| p.name.clone())
            .dedup_by(|a, b| a.name == b.name)
            .collect(),
        TestsForMutant::Explicit(packages) => packages.clone(),
    }
}

/// One unit's baseline run: its exit, its time, and the mutants it reaches.
type UnitBaseline = (Exit, Duration, BTreeSet<u32>);

/// The baseline: every unit, its time with no mutant active, and the mutants it reaches.
struct Baseline {
    units: Vec<Unit>,
    durations: Vec<Duration>,
    reach: Vec<BTreeSet<u32>>,
    timeouts: Timeouts,
}

/// Run every unit of the built executables once, recording its reach; `None` if any failed.
#[allow(clippy::too_many_arguments)] // the baseline scenario's pieces, built by the caller
fn run_baseline(
    lab: &Lab<'_>,
    build_dir: &BuildDir,
    packages: &[Arc<Package>],
    args: &TestArgs,
    n_workers: usize,
    executables: &[Executable],
    mut output: ScenarioOutput,
    mut outcome: ScenarioOutcome,
) -> Result<Option<Baseline>> {
    let (options, console) = (lab.options, lab.console);
    let label = build_dir.path();
    let reach_dir = lab
        .output_mutex
        .lock()
        .expect("lock output_dir")
        .path()
        .join("reach");
    std::fs::create_dir_all(&reach_dir).context("create reach directory")?;

    console.scenario_phase_started(label, Phase::Test);
    let units = list_units(executables, packages, args, build_dir.path())?;
    output.message(&format!("{} test units", units.len()))?;
    let results: Mutex<Vec<Option<UnitBaseline>>> = Mutex::new(vec![None; units.len()]);
    let queue = Mutex::new(units.iter().enumerate());
    thread::scope(|scope| -> Result<()> {
        let workers = (0..n_workers.min(units.len()).max(1))
            .map(|_| {
                let (queue, results, reach_dir) = (&queue, &results, &reach_dir);
                scope.spawn(move || -> Result<()> {
                    let mut log = lab
                        .output_mutex
                        .lock()
                        .expect("lock output_dir")
                        .start_scenario(&Scenario::Baseline)?;
                    loop {
                        let Some((i, unit)) = queue.lock().expect("lock unit queue").next() else {
                            return Ok(());
                        };
                        let reach_file = reach_dir.join(format!("unit-{i}"));
                        let env = [(REACH_FILE_VAR.to_owned(), reach_file.to_string())];
                        let (exit, duration) =
                            unit.run(args, options, &env, options.test_timeout, &mut log, console)?;
                        let reach = std::fs::read_to_string(&reach_file)
                            .map(|t| parse_reach(&t))
                            .unwrap_or_default();
                        std::fs::remove_file(&reach_file).ok();
                        results.lock().expect("lock results")[i] = Some((exit, duration, reach));
                    }
                })
            })
            .collect_vec();
        join_threads(workers)
    })?;
    let results = results.into_inner().expect("results");
    let mut status = Exit::Success;
    let mut durations = Vec::with_capacity(units.len());
    let mut reach = Vec::with_capacity(units.len());
    for (unit, result) in units.iter().zip(results) {
        let (exit, duration, unit_reach) = result.expect("every unit ran");
        if !exit.is_success() {
            output.message(&format!("FAILED in baseline: {} ({exit:?})", unit.label()))?;
            if status.is_success() || exit.is_timeout() {
                status = exit;
            }
        }
        durations.push(duration);
        reach.push(unit_reach);
    }
    outcome.add_phase_result(PhaseResult {
        phase: Phase::Test,
        // The sum, not the wall time: the classic fallback runs its tests in one cargo process.
        duration: durations.iter().sum(),
        process_status: status,
        argv: vec!["kvasir-test".into(), format!("{} units", units.len())],
    });
    console.scenario_phase_finished(label, Phase::Test);
    finish_baseline(lab, label, &outcome)?;
    if !status.is_success() {
        return Ok(None);
    }
    Ok(Some(Baseline {
        timeouts: Timeouts::from_baseline(&outcome, options),
        units,
        durations,
        reach,
    }))
}

fn finish_baseline(lab: &Lab<'_>, label: &Utf8Path, outcome: &ScenarioOutcome) -> Result<()> {
    lab.output_mutex
        .lock()
        .expect("lock output dir")
        .add_scenario_outcome(outcome)?;
    lab.console
        .scenario_finished(label, &Scenario::Baseline, outcome, lab.options);
    Ok(())
}

/// Run the tests that reach one mutant, with it switched on; nothing is rebuilt.
fn run_schema_mutant(
    lab: &Lab<'_>,
    label: &Utf8Path,
    id: u32,
    mutant: Mutant,
    baseline: &Baseline,
    args: &TestArgs,
) -> Result<ScenarioOutcome> {
    let (options, console) = (lab.options, lab.console);
    let mut selected = (0..baseline.units.len())
        .filter(|&i| baseline.reach[i].contains(&id))
        .filter(|&i| match lab.tests_for_mutant {
            TestsForMutant::Mutated => {
                baseline.units[i].package.name == mutant.source_file.package.name
            }
            TestsForMutant::Workspace | TestsForMutant::Explicit(_) => true,
        })
        .collect_vec();
    selected.sort_by_key(|&i| baseline.durations[i]);

    let diff = mutant.diff(&mutant.mutated_code());
    let scenario = Scenario::Mutant(mutant);
    let mut output = lab
        .output_mutex
        .lock()
        .expect("lock output_dir to start scenario")
        .start_scenario(&scenario)?;
    output.write_diff(&diff)?;
    output.message(&format!(
        "schema mutant {id}: {} of {} tests reach it",
        selected.len(),
        baseline.units.len()
    ))?;
    console.scenario_started(label, &scenario, output.open_log_read()?);
    let mut outcome = ScenarioOutcome::new(&output, scenario.clone());
    console.scenario_phase_started(label, Phase::Test);
    let env = [(ACTIVE_MUTANT_VAR.to_owned(), id.to_string())];
    let mut status = Exit::Success;
    let mut total = Duration::ZERO;
    for &i in &selected {
        let timeout = options.test_timeout.unwrap_or_else(|| {
            options.minimum_test_timeout.max(Duration::from_secs_f64(
                (baseline.durations[i].as_secs_f64()
                    * options.test_timeout_multiplier.unwrap_or(5.0))
                .ceil(),
            ))
        });
        let (exit, duration) =
            baseline.units[i].run(args, options, &env, Some(timeout), &mut output, console)?;
        total += duration;
        if !exit.is_success() {
            status = exit;
            break;
        }
    }
    if selected.is_empty() {
        output.message("no test reaches this mutant: missed without running")?;
    }
    outcome.add_phase_result(PhaseResult {
        phase: Phase::Test,
        duration: total,
        process_status: status,
        argv: vec!["kvasir-test".into(), format!("{} units", selected.len())],
    });
    console.scenario_phase_finished(label, Phase::Test);
    lab.output_mutex
        .lock()
        .expect("lock output dir to add outcome")
        .add_scenario_outcome(&outcome)?;
    debug!(id, outcome = ?outcome.summary());
    console.scenario_finished(label, &scenario, &outcome, options);
    Ok(outcome)
}

/// Record a mutant the compiler rejected inside its own schema variant as unviable.
fn record_unviable(lab: &Lab<'_>, label: &Utf8Path, mutant: Mutant, errors: &str) -> Result<()> {
    let diff = mutant.diff(&mutant.mutated_code());
    let scenario = Scenario::Mutant(mutant);
    let mut output = lab
        .output_mutex
        .lock()
        .expect("lock output_dir to start scenario")
        .start_scenario(&scenario)?;
    output.write_diff(&diff)?;
    output.message(&format!(
        "rejected by the compiler inside its own variant of the schema build: unviable without a build of its own\n{errors}"
    ))?;
    lab.console
        .scenario_started(label, &scenario, output.open_log_read()?);
    let mut outcome = ScenarioOutcome::new(&output, scenario.clone());
    outcome.add_phase_result(PhaseResult {
        phase: Phase::Build,
        duration: Duration::ZERO,
        process_status: Exit::Failure(101),
        argv: vec!["kvasir-test".into(), "schema build".into()],
    });
    lab.output_mutex
        .lock()
        .expect("lock output dir to add outcome")
        .add_scenario_outcome(&outcome)?;
    lab.console
        .scenario_finished(label, &scenario, &outcome, lab.options);
    Ok(())
}

/// The schema written into the build directory. Dropping it restores the original files.
struct Prepared<'a> {
    build_dir: &'a BuildDir,
    /// Original text of every rewritten file, by tree-relative path.
    originals: BTreeMap<Utf8PathBuf, Arc<String>>,
    /// Mutants held by the schema, with their switch id.
    active: Vec<(u32, Mutant)>,
    /// Mutants the schema could not hold, to run the classic way.
    left_out: Vec<Mutant>,
    /// Mutants the compiler rejected inside their own variant, with its messages.
    unviable: Vec<(Mutant, String)>,
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

/// Write the schema and take out mutants until the test build accepts it. Returns the schema
/// and the last build (its exit, total time of all rounds, and JSON messages).
fn prepare<'a>(
    build_dir: &'a BuildDir,
    mutants: &[Mutant],
    compile_time: &BTreeSet<String>,
    compile: &mut dyn FnMut() -> Result<Compiled>,
) -> Result<(Prepared<'a>, Compiled)> {
    let ids: Vec<u32> = (1..=u32::try_from(mutants.len()).context("too many mutants")?).collect();
    let by_file: BTreeMap<Utf8PathBuf, Vec<usize>> = mutants
        .iter()
        .enumerate()
        .map(|(i, m)| (m.source_file.tree_relative_path.clone(), i))
        .into_group_map()
        .into_iter()
        .collect();
    let mut prepared = Prepared {
        build_dir,
        originals: by_file
            .iter()
            .map(|(path, indexes)| (path.clone(), mutants[indexes[0]].source_file.code.clone()))
            .collect(),
        active: Vec::new(),
        left_out: Vec::new(),
        unviable: Vec::new(),
        rounds: 0,
        restored: false,
    };
    // Const contexts, let-chains and code that runs while compiling cannot hold a runtime
    // switch: classic from the start.
    let mut out: BTreeSet<usize> = (0..mutants.len())
        .filter(|&i| {
            mutants[i].site.is_none() || compile_time.contains(&mutants[i].source_file.package.name)
        })
        .collect();
    let mut total = Duration::ZERO;
    let mut unviable: BTreeMap<usize, String> = BTreeMap::new();
    let compiled = loop {
        prepared.rounds += 1;
        let mut schemas = BTreeMap::new();
        for (path, indexes) in &by_file {
            let code = &prepared.originals[path];
            let in_schema = indexes
                .iter()
                .filter(|i| !out.contains(i))
                .filter_map(|&i| {
                    Some(SchemaMutant {
                        id: ids[i],
                        span: mutants[i].span,
                        replacement: &mutants[i].replacement,
                        site: mutants[i].site?,
                    })
                })
                .collect_vec();
            let schema =
                render(code, &in_schema).with_context(|| format!("render the schema of {path}"))?;
            out.extend(schema.rejected.iter().map(|&id| id as usize - 1));
            build_dir.overwrite_file(path, &schema.code)?;
            schemas.insert(path.clone(), schema);
        }
        let compiled = compile()?;
        total += compiled.duration;
        if compiled.exit.is_success() {
            break compiled;
        }
        let errors = parse_errors(&compiled.json, build_dir.path());
        let before = out.clone();
        // A kind that may not type-check, rejected inside its own variant: unviable, as a classic
        // build of the same text would be, without paying for that build.
        for e in &errors {
            if let Some(region) = schemas.get(&e.file).and_then(|s| s.innermost(&e.range))
                && region.kind == RegionKind::Variant
                && let [id] = region.ids.as_slice()
            {
                let i = *id as usize - 1;
                if mutants[i].genre.may_not_type_check() && !out.contains(&i) {
                    unviable.entry(i).or_default().push_str(&e.rendered);
                }
            }
        }
        out.extend(unviable.keys().copied());
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
        if blamed.is_empty() || blamed.is_subset(&before) {
            // A hand-written edit can break code outside its site (a moved value used later):
            // take those out before giving up on every mutant.
            let custom: BTreeSet<usize> = (0..mutants.len())
                .filter(|i| !out.contains(i) && mutants[*i].genre == crate::Genre::Custom)
                .collect();
            if !custom.is_empty() {
                warn!(
                    errors = errors.len(),
                    first = ?errors.first(),
                    count = custom.len(),
                    "schema errors not attributable to any mutant; custom mutants go classic"
                );
                out.extend(custom);
                continue;
            }
            if out.len() == mutants.len() {
                // The tree does not build even without any mutant: the baseline reports it.
                break compiled;
            }
            warn!(
                errors = errors.len(),
                first = ?errors.first(),
                "schema errors not attributable to any mutant; testing all mutants one build each"
            );
            out.extend(0..mutants.len());
            continue;
        }
        out.extend(blamed);
    };
    prepared.active = (0..mutants.len())
        .filter(|i| !out.contains(i))
        .map(|i| (ids[i], mutants[i].clone()))
        .collect();
    prepared.left_out = out
        .iter()
        .filter(|i| !unviable.contains_key(i))
        .map(|&i| mutants[i].clone())
        .collect();
    prepared.unviable = unviable
        .into_iter()
        .map(|(i, errors)| (mutants[i].clone(), errors))
        .collect();
    Ok((
        prepared,
        Compiled {
            duration: total,
            ..compiled
        },
    ))
}

/// A compiler error located in a source file of the tree.
#[derive(Debug)]
struct CompileError {
    file: Utf8PathBuf,
    range: std::ops::Range<usize>,
    /// The compiler's rendered message (empty if it gave none).
    rendered: String,
}

/// Primary spans of error-level compiler messages, with paths relative to `root`.
fn parse_errors(json_lines: &str, root: &Utf8Path) -> Vec<CompileError> {
    json_lines
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|v| v["reason"] == "compiler-message" && v["message"]["level"] == "error")
        .flat_map(|v| {
            let rendered = v["message"]["rendered"].as_str().unwrap_or("").to_owned();
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
                        rendered: rendered.clone(),
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
