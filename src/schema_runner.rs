// Copyright 2026 Mathis Groult
// KvasirTest addition to cargo-mutants (MIT): see docs/decisions/0001-mutant-schemata.md.

//! Test units of a schema run: every test of the built test executables, run one per process
//! without cargo, plus one unit for a package's doc tests (those are built per run by rustdoc, so
//! they go through `cargo test --doc`).
//!
//! One test per process gives each test its own reach (the guards it evaluated) and lets a mutant
//! run only the tests that reach it, fastest first.

use std::collections::BTreeSet;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use camino::{Utf8Path, Utf8PathBuf};
use itertools::Itertools;
use tracing::debug;

use crate::Result;
use crate::cargo::{cargo_argv, cargo_bin};
use crate::console::Console;
use crate::options::Options;
use crate::outcome::Phase;
use crate::output::ScenarioOutput;
use crate::package::{Package, PackageSelection};
use crate::process::{Exit, Process};

/// `cargo test` arguments split by who reads them.
#[derive(Debug, Default, Eq, PartialEq)]
pub struct TestArgs {
    /// Options for cargo itself (target selection, features, ...).
    pub cargo: Vec<String>,
    /// Test name filters (cargo's `TESTNAME` and positional arguments after `--`).
    pub filters: Vec<String>,
    /// Options for the test harness (after `--`), without the filters.
    pub harness: Vec<String>,
}

/// Cargo options that take a separate value (`--bin NAME`).
const CARGO_VALUE_OPTIONS: &[&str] = &[
    "-p",
    "--package",
    "--exclude",
    "--bin",
    "--test",
    "--example",
    "--bench",
    "-F",
    "--features",
    "--profile",
    "--target",
    "--target-dir",
    "--manifest-path",
    "-j",
    "--jobs",
    "--message-format",
    "--color",
    "--config",
    "-Z",
    "--lockfile-path",
];

/// libtest options that take a separate value (`--skip NAME`).
const HARNESS_VALUE_OPTIONS: &[&str] = &[
    "--skip",
    "--test-threads",
    "--format",
    "--color",
    "--logfile",
    "-Z",
    "--shuffle-seed",
];

/// Cargo options that select targets; without any, `cargo test` also runs doc tests.
const TARGET_OPTIONS: &[&str] = &[
    "--lib",
    "--bin",
    "--bins",
    "--test",
    "--tests",
    "--example",
    "--examples",
    "--bench",
    "--benches",
    "--all-targets",
    "--doc",
];

impl TestArgs {
    /// Split the arguments a user gives after `cargo mutants ... --`, as `cargo test` reads them.
    pub fn parse(args: &[String]) -> TestArgs {
        let mut parsed = TestArgs::default();
        let mut it = args.iter();
        while let Some(arg) = it.next() {
            if arg == "--" {
                parsed.split_harness(it.by_ref());
                break;
            }
            if !arg.starts_with('-') {
                parsed.filters.push(arg.clone());
                continue;
            }
            parsed.cargo.push(arg.clone());
            if CARGO_VALUE_OPTIONS.contains(&arg.as_str())
                && let Some(value) = it.next()
            {
                parsed.cargo.push(value.clone());
            }
        }
        parsed
    }

    fn split_harness<'a>(&mut self, args: impl Iterator<Item = &'a String>) {
        let mut args = args;
        while let Some(arg) = args.next() {
            if !arg.starts_with('-') {
                self.filters.push(arg.clone());
                continue;
            }
            self.harness.push(arg.clone());
            if HARNESS_VALUE_OPTIONS.contains(&arg.as_str())
                && let Some(value) = args.next()
            {
                self.harness.push(value.clone());
            }
        }
    }

    fn selects_targets(&self) -> bool {
        self.cargo
            .iter()
            .any(|a| TARGET_OPTIONS.contains(&a.split('=').next().unwrap_or(a)))
    }

    pub(crate) fn only_doc(&self) -> bool {
        self.cargo.iter().any(|a| a == "--doc")
    }

    fn wants(&self, flag: &str) -> bool {
        self.harness.iter().any(|a| a == flag)
    }
}

/// What a unit runs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UnitKind {
    /// One test of a test executable.
    Test {
        executable: Utf8PathBuf,
        /// `kind:name` of the cargo target.
        target: String,
        name: String,
    },
    /// All doc tests of a package (through `cargo test --doc`).
    DocTests,
}

/// One independently runnable piece of the test suite.
#[derive(Clone, Debug)]
pub struct Unit {
    pub kind: UnitKind,
    pub package: Arc<Package>,
    /// The package directory: cargo runs tests from there.
    pub cwd: Utf8PathBuf,
}

impl Unit {
    /// A stable key: `package/kind:target/test name`, or `package/doc tests`.
    pub fn key(&self) -> String {
        match &self.kind {
            UnitKind::Test { target, name, .. } => {
                format!("{}/{target}/{name}", self.package.name)
            }
            UnitKind::DocTests => format!("{}/doc tests", self.package.name),
        }
    }

    /// A short name for logs.
    pub fn label(&self) -> String {
        match &self.kind {
            UnitKind::Test {
                executable, name, ..
            } => {
                format!("{} {name}", executable.file_name().unwrap_or_default())
            }
            UnitKind::DocTests => format!("doc tests of {}", self.package.name),
        }
    }

    pub(crate) fn argv(&self, test_args: &TestArgs, options: &Options) -> Vec<String> {
        match &self.kind {
            UnitKind::Test {
                executable, name, ..
            } => {
                let mut argv = vec![executable.to_string(), "--exact".to_owned(), name.clone()];
                argv.extend(test_args.harness.iter().cloned());
                argv
            }
            UnitKind::DocTests => {
                let mut argv = cargo_argv(
                    &PackageSelection::Explicit(vec![Arc::clone(&self.package)]),
                    Phase::Test,
                    &Options {
                        additional_cargo_test_args: Vec::new(),
                        ..options.clone()
                    },
                );
                argv.push("--doc".to_owned());
                argv.extend(test_args.filters.iter().cloned());
                argv.push("--".to_owned());
                argv.extend(test_args.harness.iter().cloned());
                argv
            }
        }
    }

    pub(crate) fn env(&self) -> Vec<(String, String)> {
        let mut env = vec![
            ("INSTA_UPDATE".to_owned(), "no".to_owned()),
            ("INSTA_FORCE_PASS".to_owned(), "0".to_owned()),
            ("CARGO".to_owned(), cargo_bin()),
            ("CARGO_MANIFEST_DIR".to_owned(), self.cwd.to_string()),
            ("CARGO_PKG_NAME".to_owned(), self.package.name.clone()),
            ("CARGO_PKG_VERSION".to_owned(), self.package.version.clone()),
        ];
        if let UnitKind::Test { executable, .. } = &self.kind {
            env.push(("LD_LIBRARY_PATH".to_owned(), library_path(executable)));
        }
        env
    }

    /// Run the unit; `extra_env` switches a mutant on or names the reach file.
    #[allow(clippy::too_many_arguments)] // all needed by Process::start
    pub fn run(
        &self,
        test_args: &TestArgs,
        options: &Options,
        extra_env: &[(String, String)],
        timeout: Option<Duration>,
        output: &mut ScenarioOutput,
        console: &Console,
    ) -> Result<(Exit, Duration)> {
        let start = Instant::now();
        let argv = self.argv(test_args, options);
        let mut env = self.env();
        env.extend_from_slice(extra_env);
        let mut process = Process::start(&argv, &env, &self.cwd, timeout, None, output)?;
        let exit = loop {
            if let Some(exit) = process.poll()? {
                break exit;
            }
            console.tick();
            std::thread::sleep(Duration::from_millis(20));
        };
        output.message(&format!(
            "{}: {exit:?} in {:?}",
            self.label(),
            start.elapsed()
        ))?;
        Ok((exit, start.elapsed()))
    }
}

/// A test executable built by `cargo test --no-run`.
#[derive(Debug, Eq, PartialEq)]
pub struct Executable {
    pub path: Utf8PathBuf,
    /// `kind:name` of the cargo target (stable, unlike the hashed file name).
    pub target: String,
    pub cwd: Utf8PathBuf,
}

/// Test executables named in `cargo test --no-run --message-format=json` output.
pub fn parse_executables(json_lines: &str) -> Vec<Executable> {
    json_lines
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["reason"] == "compiler-artifact" && v["profile"]["test"] == true)
        .filter_map(|v| {
            let manifest = Utf8PathBuf::from(v["manifest_path"].as_str()?);
            Some(Executable {
                path: Utf8PathBuf::from(v["executable"].as_str()?),
                target: format!(
                    "{}:{}",
                    v["target"]["kind"][0].as_str()?,
                    v["target"]["name"].as_str()?
                ),
                cwd: manifest.parent()?.to_owned(),
            })
        })
        .collect()
}

/// Test names from a libtest `--list` output.
pub fn parse_test_list(list: &str) -> Vec<String> {
    list.lines()
        .filter_map(|l| l.strip_suffix(": test"))
        .map(str::to_owned)
        .collect()
}

/// One test build: its exit, how long it took, and cargo's JSON messages.
#[derive(Debug)]
pub struct Compiled {
    pub exit: Exit,
    pub duration: Duration,
    pub json: String,
}

/// Where a build writes its log: a mutation scenario's log, or a plain file in `test` mode.
pub trait BuildLog {
    fn message(&mut self, text: &str) -> Result<()>;
    /// A handle appending to the log (for the child's stderr).
    fn append(&self) -> Result<std::fs::File>;
    /// Where to keep cargo's JSON messages.
    fn json_path(&self) -> Utf8PathBuf;
}

impl BuildLog for ScenarioOutput {
    fn message(&mut self, text: &str) -> Result<()> {
        ScenarioOutput::message(self, text)
    }

    fn append(&self) -> Result<std::fs::File> {
        self.open_log_append()
    }

    fn json_path(&self) -> Utf8PathBuf {
        self.output_dir
            .join(self.log_path())
            .with_extension("build.json")
    }
}

/// A plain log file.
pub struct FileLog(pub Utf8PathBuf);

impl BuildLog for FileLog {
    fn message(&mut self, text: &str) -> Result<()> {
        use std::io::Write as _;
        writeln!(self.append()?, "{text}").context("write log")
    }

    fn append(&self) -> Result<std::fs::File> {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.0)
            .with_context(|| format!("open {}", self.0))
    }

    fn json_path(&self) -> Utf8PathBuf {
        self.0.with_extension("json")
    }
}

/// Build the test executables of `packages` (`cargo test --no-run`, JSON messages); compiler
/// errors are also written, rendered, to the log.
#[allow(clippy::too_many_arguments)] // the build's inputs, each needed
pub fn build(
    root: &Utf8Path,
    packages: &[Arc<Package>],
    test_args: &TestArgs,
    options: &Options,
    timeout: Option<Duration>,
    env: &[(String, String)],
    output: &mut dyn BuildLog,
) -> Result<Compiled> {
    let start = Instant::now();
    let mut argv = cargo_argv(
        &PackageSelection::Explicit(packages.to_vec()),
        Phase::Build,
        options,
    );
    argv.extend(test_args.cargo.iter().filter(|a| *a != "--doc").cloned());
    argv.push("--message-format=json".to_owned());
    output.message(&argv.join(" "))?;
    let json_path = output.json_path();
    let child = Command::new(&argv[0])
        .args(&argv[1..])
        .envs(env.iter().map(|(k, v)| (k, v)))
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(std::fs::File::create(&json_path).context("create build json log")?)
        .stderr(output.append()?)
        .spawn()
        .with_context(|| format!("run {}", argv.join(" ")))?;
    let exit = wait_with_timeout(child, timeout)?;
    let json = std::fs::read_to_string(&json_path).context("read build json")?;
    for rendered in json
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["reason"] == "compiler-message" && v["message"]["level"] == "error")
        .filter_map(|v| v["message"]["rendered"].as_str().map(str::to_owned))
    {
        output.message(&rendered)?;
    }
    Ok(Compiled {
        exit,
        duration: start.elapsed(),
        json,
    })
}

fn wait_with_timeout(mut child: std::process::Child, timeout: Option<Duration>) -> Result<Exit> {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status.into());
        }
        if timeout.is_some_and(|t| start.elapsed() > t) {
            child.kill().ok();
            child.wait().ok();
            return Ok(Exit::Timeout);
        }
        crate::interrupt::check_interrupted()?;
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Every unit of the run: each listed test of each executable, and doc tests when `cargo test`
/// would run them.
pub fn list_units(
    executables: &[Executable],
    packages: &[Arc<Package>],
    args: &TestArgs,
    root: &Utf8Path,
) -> Result<Vec<Unit>> {
    let mut units = Vec::new();
    for exe in executables {
        let package = packages
            .iter()
            .find(|p| root.join(&p.relative_dir) == exe.cwd)
            .cloned()
            .with_context(|| format!("no package for test executable {}", exe.path))?;
        let mut names = list(exe, args, false)?;
        if !(args.wants("--ignored") || args.wants("--include-ignored")) {
            let ignored: BTreeSet<String> = list(exe, args, true)?.into_iter().collect();
            names.retain(|n| !ignored.contains(n));
        } else if args.wants("--ignored") {
            names = list(exe, args, true)?;
        }
        debug!(executable = %exe.path, tests = names.len());
        units.extend(names.into_iter().map(|name| Unit {
            kind: UnitKind::Test {
                executable: exe.path.clone(),
                target: exe.target.clone(),
                name,
            },
            package: Arc::clone(&package),
            cwd: exe.cwd.clone(),
        }));
    }
    if !args.selects_targets() || args.only_doc() {
        for package in packages.iter().filter(|p| has_lib(p)) {
            units.push(Unit {
                kind: UnitKind::DocTests,
                package: Arc::clone(package),
                cwd: root.join(&package.relative_dir),
            });
        }
    }
    Ok(units)
}

fn has_lib(package: &Package) -> bool {
    package
        .top_sources
        .iter()
        .any(|s| s.file_name() == Some("lib.rs"))
}

/// The library path cargo gives test executables: their deps directory (dylib dependencies) and
/// the toolchain's library directory (proc-macro crates link `libstd` dynamically), before any
/// path already set.
fn library_path(executable: &Utf8Path) -> String {
    static TOOLCHAIN_LIB: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    let toolchain_lib = TOOLCHAIN_LIB.get_or_init(|| {
        let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_owned());
        let out = Command::new(rustc)
            .args(["--print", "target-libdir"])
            .stderr(Stdio::null())
            .output()
            .ok()?;
        Some(String::from_utf8(out.stdout).ok()?.trim().to_owned())
    });
    executable
        .parent()
        .map(Utf8Path::to_string)
        .into_iter()
        .chain(toolchain_lib.clone())
        .chain(
            std::env::var("LD_LIBRARY_PATH")
                .ok()
                .filter(|p| !p.is_empty()),
        )
        .join(":")
}

fn list(exe: &Executable, args: &TestArgs, ignored: bool) -> Result<Vec<String>> {
    let mut cmd = Command::new(&exe.path);
    cmd.arg("--list")
        .current_dir(&exe.cwd)
        .env("LD_LIBRARY_PATH", library_path(&exe.path));
    // Options that change which tests exist in the list; others (--test-threads) do not matter.
    let mut harness = args.harness.iter();
    while let Some(a) = harness.next() {
        if a == "--skip" {
            cmd.arg(a);
            if let Some(v) = harness.next() {
                cmd.arg(v);
            }
        } else if a == "--exact" {
            cmd.arg(a);
        }
    }
    if ignored {
        cmd.arg("--ignored");
    }
    cmd.args(&args.filters);
    let out = cmd
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .with_context(|| format!("list tests of {}", exe.path))?;
    if !out.status.success() {
        bail!("{} --list failed: {:?}", exe.path, out.status);
    }
    Ok(parse_test_list(&String::from_utf8_lossy(&out.stdout)))
}

#[cfg(test)]
mod test {
    use pretty_assertions::assert_eq;

    use super::*;

    fn strings(s: &[&str]) -> Vec<String> {
        s.iter().map(|&s| s.to_owned()).collect()
    }

    #[test]
    fn test_args_split_cargo_options_filters_and_harness_options() {
        let parsed = TestArgs::parse(&strings(&[
            "--lib",
            "-p",
            "x",
            "loot::golden",
            "--",
            "--skip",
            "slow",
            "--test-threads",
            "2",
            "other",
            "--include-ignored",
        ]));
        assert_eq!(
            parsed,
            TestArgs {
                cargo: strings(&["--lib", "-p", "x"]),
                filters: strings(&["loot::golden", "other"]),
                harness: strings(&["--skip", "slow", "--test-threads", "2", "--include-ignored"]),
            }
        );
        assert!(parsed.selects_targets());
    }

    #[test]
    fn without_target_options_doc_tests_run_too() {
        assert!(!TestArgs::parse(&strings(&["--features=x", "name"])).selects_targets());
        assert!(TestArgs::parse(&strings(&["--test=it"])).selects_targets());
    }

    #[test]
    fn list_output_gives_test_names_only() {
        let list = "a::b: test\nc: benchmark\nd::e: test\n\n2 tests, 1 benchmark\n";
        assert_eq!(parse_test_list(list), strings(&["a::b", "d::e"]));
    }

    #[test]
    fn executables_come_from_test_profile_artifacts() {
        let json = r#"{"reason":"compiler-artifact","profile":{"test":false},"executable":null,"manifest_path":"/w/Cargo.toml","target":{"name":"dep"}}
{"reason":"compiler-artifact","profile":{"test":true},"executable":"/w/target/debug/deps/x-1","manifest_path":"/w/x/Cargo.toml","target":{"name":"x","kind":["lib"]}}
{"reason":"build-finished","success":true}"#;
        assert_eq!(
            parse_executables(json),
            vec![Executable {
                path: "/w/target/debug/deps/x-1".into(),
                target: "lib:x".into(),
                cwd: "/w/x".into(),
            }]
        );
    }
}
