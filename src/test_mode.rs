// Copyright 2026 Mathis Groult
// SPDX-License-Identifier: GPL-3.0-or-later (KvasirTest; see NOTICE)

//! `kvasir-test test`: build once, run only the tests a change can reach, in parallel, and keep
//! the reach map exact by recording every run.

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::process::CommandExt as _;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use camino::{Utf8Path, Utf8PathBuf};
use clap::Parser;
use itertools::Itertools;

use crate::Result;
use crate::exit_code::ExitCode;
use crate::mirror::{SyncCache, sync};
use crate::options::Options;
use crate::package::Package;
use crate::probe::{fnv64, parse_probe_ids};
use crate::process::Exit;
use crate::reach_map::{MAP_VERSION, Plan, ReachMap, Snapshot, TestEntry, plan, select};
use crate::schema::REACH_FILE_VAR;
use crate::schema_runner::{FileLog, TestArgs, Unit, build, list_units, parse_executables};
use crate::workspace::Workspace;

/// Run only the tests that the changes since the last run can reach.
#[derive(Parser, Debug)]
#[command(
    after_help = "The first run (and any change outside function bodies) runs every test and \
                  records which functions each test executes; later runs select from that record."
)]
pub struct TestModeArgs {
    /// Test only these packages (default: every package of the workspace).
    #[arg(long = "package", short = 'p')]
    packages: Vec<String>,

    /// Run and re-record every test.
    #[arg(long)]
    full: bool,

    /// Show what would run, without building or running.
    #[arg(long)]
    plan: bool,

    /// Tests to run at once (default: CPUs minus 2, at least 1).
    #[arg(long, short = 'j', env = "KVASIR_TEST_JOBS")]
    jobs: Option<usize>,

    /// The workspace (default: the current directory).
    #[arg(long, short = 'd')]
    dir: Option<Utf8PathBuf>,

    /// Arguments as for `cargo test`: target options, a test name filter, and after a second
    /// `--` the test harness options.
    #[arg(last = true)]
    cargo_test_args: Vec<String>,
}

/// One test's result in this run.
struct Ran {
    exit: Exit,
    duration: Duration,
    reach: BTreeSet<u64>,
    /// Failed attempts before this (passing) one: a flaky test.
    failed_attempts: usize,
}

/// A failing test (not a timeout) is run again up to this many times; passing on a retry
/// reports it as flaky instead of failed (a test that fails under host load once).
pub const FLAKY_RETRIES: usize = 2;

pub fn run(args: &TestModeArgs) -> Result<ExitCode> {
    let start = Instant::now();
    let dir = match &args.dir {
        Some(dir) => dir.clone(),
        None => Utf8PathBuf::try_from(std::env::current_dir()?).context("non-UTF-8 directory")?,
    };
    let workspace = Workspace::open(&dir)?;
    let root = workspace.root().to_owned();
    let target_dir = workspace.target_directory().to_owned();
    let packages: Vec<Arc<Package>> = if args.packages.is_empty() {
        workspace.packages().to_vec()
    } else {
        workspace.packages_by_name(&args.packages)
    };
    if packages.is_empty() {
        bail!("no packages to test");
    }
    let compile_time = workspace.compile_time_packages();
    let instrumented: Vec<Utf8PathBuf> = packages
        .iter()
        .filter(|p| !compile_time.contains(&p.name))
        .map(|p| p.relative_dir.clone())
        .collect();
    let state = target_dir.join("kvasir-test");
    let mirror = state.join("tree");
    let logs = state.join("logs");
    fs::create_dir_all(&state).context("create kvasir-test state directory")?;
    if logs.exists() {
        fs::remove_dir_all(&logs).ok();
    }
    fs::create_dir_all(&logs)?;

    let skip: Vec<Utf8PathBuf> = target_dir
        .strip_prefix(&root)
        .map(|t| vec![t.to_owned()])
        .unwrap_or_default();
    let sync_start = Instant::now();
    let cache_path = state.join("sync-cache.json");
    let mut cache: SyncCache = fs::read_to_string(&cache_path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let synced = sync(&root, &mirror, &instrumented, &skip, &mut cache)?;
    fs::write(&cache_path, serde_json::to_string(&cache)?).context("write sync cache")?;
    let snapshot = Snapshot {
        scope: format!(
            "packages={} args={}",
            packages.iter().map(|p| &p.name).join(","),
            args.cargo_test_args.join(" ")
        ),
        env_hash: fnv64(format!("{}\0{}", synced.other_hash, toolchain()).as_bytes()),
        files: synced.files.clone(),
    };
    let map_path = state.join("reach.json");
    let map: Option<ReachMap> = fs::read_to_string(&map_path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok());
    let the_plan = if args.full {
        Plan::Full("--full".into())
    } else {
        plan(map.as_ref(), &snapshot)
    };
    match &the_plan {
        Plan::Full(reason) => println!("Full run: {reason}."),
        Plan::Changed(changed) => {
            println!("{} changed function(s).", changed.len());
            for key in changed.iter().take(10) {
                println!("  {key}");
            }
            if changed.len() > 10 {
                println!("  ... and {} more", changed.len() - 10);
            }
        }
    }
    println!(
        "Mirror synced in {:.1}s ({} file(s) read, {} rewritten).",
        sync_start.elapsed().as_secs_f64(),
        synced.read,
        synced.written
    );

    let test_args = TestArgs::parse(&args.cargo_test_args);
    let options = Options::default();
    let env = vec![("CARGO_TARGET_DIR".to_owned(), target_dir.to_string())];
    let build_start = Instant::now();
    let compiled = build(
        &mirror,
        &packages,
        &test_args,
        &options,
        None,
        &env,
        &mut FileLog(logs.join("build.log")),
    )?;
    if !compiled.exit.is_success() {
        println!(
            "Build failed after {:.1}s: see {}",
            build_start.elapsed().as_secs_f64(),
            logs.join("build.log")
        );
        // The map stays as it was, so the next run sees the same changes.
        return Ok(ExitCode::BaselineFailed);
    }
    println!("Built in {:.1}s.", build_start.elapsed().as_secs_f64());
    let executables = if test_args.only_doc() {
        Vec::new()
    } else {
        parse_executables(&compiled.json)
    };
    let units = list_units(&executables, &packages, &test_args, &mirror)?;
    let keys: Vec<String> = units.iter().map(Unit::key).collect();
    let selected: BTreeSet<String> = match (&the_plan, &map) {
        (Plan::Changed(changed), Some(map)) => select(map, changed, &keys),
        _ => keys.iter().cloned().collect(),
    };
    let to_run: Vec<usize> = (0..units.len())
        .filter(|&i| selected.contains(&keys[i]))
        .sorted_by(|&a, &b| {
            // Slowest first keeps the workers busy until the end.
            let seconds = |i: usize| {
                map.as_ref()
                    .and_then(|m| m.tests.get(&keys[i]))
                    .map_or(f64::MAX, |t| t.seconds)
            };
            seconds(b).total_cmp(&seconds(a))
        })
        .collect();
    if args.plan {
        println!("Would run {} of {} tests:", to_run.len(), units.len());
        for &i in &to_run {
            println!("  {}", keys[i]);
        }
        return Ok(ExitCode::Success);
    }

    let run_start = Instant::now();
    let workers = args.jobs.unwrap_or_else(default_jobs).max(1);
    let results = run_units(
        &units,
        &to_run,
        &test_args,
        &options,
        &env,
        &logs,
        map.as_ref(),
        &keys,
        workers,
    )?;
    let run_time = run_start.elapsed();

    let mut new_map = match (&the_plan, map) {
        (Plan::Changed(_), Some(map)) => map,
        _ => ReachMap {
            version: MAP_VERSION,
            scope: String::new(),
            env_hash: 0,
            files: std::collections::BTreeMap::default(),
            tests: std::collections::BTreeMap::default(),
        },
    };
    new_map.version = MAP_VERSION;
    new_map.scope = snapshot.scope;
    new_map.env_hash = snapshot.env_hash;
    new_map.files = snapshot.files;
    new_map.tests.retain(|k, _| keys.contains(k));
    let mut failed = Vec::new();
    let mut flaky = Vec::new();
    for (&i, ran) in to_run.iter().zip(&results) {
        if !ran.exit.is_success() {
            failed.push((keys[i].clone(), ran.exit));
        } else if ran.failed_attempts > 0 {
            flaky.push((keys[i].clone(), ran.failed_attempts));
        }
        new_map.tests.insert(
            keys[i].clone(),
            TestEntry {
                reach: ran
                    .reach
                    .iter()
                    .filter_map(|id| synced.probes.get(id).cloned())
                    .collect(),
                seconds: ran.duration.as_secs_f64(),
                passed: ran.exit.is_success(),
            },
        );
    }
    let tmp = map_path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_string(&new_map)?).context("write reach map")?;
    fs::rename(&tmp, &map_path).context("replace reach map")?;

    println!(
        "Ran {} of {} tests in {:.1}s ({} worker(s)): {} passed{}, {} failed. Total {:.1}s.",
        to_run.len(),
        units.len(),
        run_time.as_secs_f64(),
        workers,
        to_run.len() - failed.len(),
        if flaky.is_empty() {
            String::new()
        } else {
            format!(" ({} flaky)", flaky.len())
        },
        failed.len(),
        start.elapsed().as_secs_f64()
    );
    for (key, attempts) in &flaky {
        println!(
            "FLAKY {key}: failed {attempts} time(s), then passed; failing log: {}",
            log_path(&logs, key).with_extension("attempt1.log")
        );
    }
    if to_run.is_empty() {
        println!("No test reaches the changed code.");
    }
    for (key, exit) in &failed {
        println!("FAILED {key} ({exit:?}): {}", log_path(&logs, key));
    }
    Ok(if failed.is_empty() {
        ExitCode::Success
    } else {
        ExitCode::FoundProblems
    })
}

/// CPUs minus 2 (the host keeps two for everything else), at least 1.
fn default_jobs() -> usize {
    thread::available_parallelism()
        .map_or(1, std::num::NonZero::get)
        .saturating_sub(2)
        .max(1)
}

/// `rustc -vV`: a toolchain change means a full run.
fn toolchain() -> String {
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_owned());
    Command::new(rustc)
        .arg("-vV")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

fn log_path(logs: &Utf8Path, key: &str) -> Utf8PathBuf {
    let name: String = key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    logs.join(format!("{name}.log"))
}

#[allow(clippy::too_many_arguments)] // the run's shared inputs
fn run_units(
    units: &[Unit],
    to_run: &[usize],
    test_args: &TestArgs,
    options: &Options,
    env: &[(String, String)],
    logs: &Utf8Path,
    map: Option<&ReachMap>,
    keys: &[String],
    workers: usize,
) -> Result<Vec<Ran>> {
    let results: Mutex<Vec<Option<Ran>>> = Mutex::new((0..to_run.len()).map(|_| None).collect());
    let queue = Mutex::new(to_run.iter().enumerate());
    thread::scope(|scope| -> Result<()> {
        let handles = (0..workers.min(to_run.len()).max(1))
            .map(|_| {
                let (queue, results) = (&queue, &results);
                scope.spawn(move || -> Result<()> {
                    loop {
                        let Some((slot, &i)) = queue.lock().expect("lock queue").next() else {
                            return Ok(());
                        };
                        let timeout = map
                            .and_then(|m| m.tests.get(&keys[i]))
                            .map(|t| Duration::from_secs_f64((t.seconds * 10.0).max(60.0)));
                        let log = log_path(logs, &keys[i]);
                        let mut ran = run_one(&units[i], test_args, options, env, &log, timeout)?;
                        let mut failed = 0;
                        while failed < FLAKY_RETRIES
                            && !ran.exit.is_success()
                            && !ran.exit.is_timeout()
                        {
                            failed += 1;
                            // Keep the failing log next to the retry's.
                            fs::rename(&log, log.with_extension(format!("attempt{failed}.log")))
                                .ok();
                            ran = run_one(&units[i], test_args, options, env, &log, timeout)?;
                        }
                        if ran.exit.is_success() {
                            ran.failed_attempts = failed;
                        }
                        results.lock().expect("lock results")[slot] = Some(ran);
                    }
                })
            })
            .collect_vec();
        for h in handles {
            h.join().expect("test worker panicked")?;
        }
        Ok(())
    })?;
    Ok(results
        .into_inner()
        .expect("results")
        .into_iter()
        .map(|r| r.expect("every selected test ran"))
        .collect())
}

/// Run one test in its own process group with reach recording on.
fn run_one(
    unit: &Unit,
    test_args: &TestArgs,
    options: &Options,
    env: &[(String, String)],
    log: &Utf8Path,
    timeout: Option<Duration>,
) -> Result<Ran> {
    let start = Instant::now();
    let reach_file = log.with_extension("reach");
    fs::remove_file(&reach_file).ok();
    let argv = unit.argv(test_args, options);
    let out = fs::File::create(log).with_context(|| format!("create {log}"))?;
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .envs(unit.env())
        .envs(env.iter().map(|(k, v)| (k, v)))
        .env(REACH_FILE_VAR, &reach_file)
        .current_dir(&unit.cwd)
        .stdin(Stdio::null())
        .stdout(out.try_clone()?)
        .stderr(out)
        .process_group(0)
        .spawn()
        .with_context(|| format!("run {}", argv.join(" ")))?;
    let exit = loop {
        if let Some(status) = child.try_wait()? {
            break Exit::from(status);
        }
        if timeout.is_some_and(|t| start.elapsed() > t) {
            let pgid = nix::unistd::Pid::from_raw(i32::try_from(child.id()).context("pid")?);
            nix::sys::signal::killpg(pgid, nix::sys::signal::Signal::SIGKILL).ok();
            child.wait().ok();
            break Exit::Timeout;
        }
        crate::interrupt::check_interrupted()?;
        thread::sleep(Duration::from_millis(10));
    };
    let reach = fs::read_to_string(&reach_file)
        .map(|t| parse_probe_ids(&t))
        .unwrap_or_default();
    fs::remove_file(&reach_file).ok();
    Ok(Ran {
        exit,
        duration: start.elapsed(),
        reach,
        failed_attempts: 0,
    })
}
