// SPDX-License-Identifier: GPL-3.0-or-later

//! `kvasir-test budget`: keep the test suite fast as it grows.
//!
//! Reads the test times both modes record ([`crate::durations`]) and fails when a test takes
//! longer than the budget and is not on the allow list, or when an allowed slow test got slower
//! than its recorded ceiling. Allowed tests that now fit the budget are reported so the list
//! only shrinks (a ratchet). Every later run keeps the check: nothing here is a one-off.

use std::process::Command;

use anyhow::Context as _;
use camino::{Utf8Path, Utf8PathBuf};
use serde::Deserialize;

use crate::Result;
use crate::durations::{self, Durations};
use crate::exit_code::ExitCode;

#[derive(clap::Args, Debug)]
#[command(
    about = "Check recorded test times against a budget",
    after_help = "Times come from the last `kvasir-test mutants` baseline or `kvasir-test test` \
                  run of each test (TARGET/kvasir-test/test-durations.json). Exit 2 if a test \
                  over --max is not allowed, or an allowed test exceeds its max x --slack."
)]
pub struct BudgetArgs {
    /// Longest time, in seconds, any test not on the allow list may take.
    #[arg(long, default_value_t = 10.0)]
    max: f64,
    /// TOML allow list: `[[slow]] test = "<test path or key>", max = <seconds>, reason = ".."`.
    #[arg(long)]
    allow: Option<Utf8PathBuf>,
    /// Tolerance on an allowed test's own max (host load varies between runs).
    #[arg(long, default_value_t = 1.5)]
    slack: f64,
    /// Target directory holding the times (default: the cargo workspace's).
    #[arg(long)]
    target_dir: Option<Utf8PathBuf>,
}

/// One allowed slow test.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Slow {
    /// A test name (`module::test`) or a full key (`package/kind:target/module::test`).
    pub test: String,
    /// Its ceiling in seconds.
    pub max: f64,
    /// Why it may be slow (and what would make it fast).
    pub reason: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct AllowList {
    #[serde(default)]
    slow: Vec<Slow>,
}

/// What the check found, in report order.
#[derive(Debug, PartialEq)]
pub enum Finding {
    /// Over the budget and not allowed.
    Over { key: String, seconds: f64 },
    /// Allowed, but slower than its ceiling x slack.
    OverAllowed { key: String, seconds: f64, max: f64 },
    /// Allowed and within its ceiling.
    Allowed {
        key: String,
        seconds: f64,
        max: f64,
        reason: String,
    },
    /// Allowed, but now within the budget: remove it from the list.
    NowFast { key: String, seconds: f64 },
    /// An allow-list entry matching no recorded test.
    Stale { test: String },
}

impl Finding {
    pub fn fails(&self) -> bool {
        matches!(self, Finding::Over { .. } | Finding::OverAllowed { .. })
    }
}

fn matches(slow: &Slow, key: &str) -> bool {
    key == slow.test || key.ends_with(&format!("/{}", slow.test))
}

/// Judge every recorded time.
pub fn check(times: &Durations, allow: &[Slow], max: f64, slack: f64) -> Vec<Finding> {
    let mut findings = Vec::new();
    let mut by_time: Vec<(&String, f64)> = times.iter().map(|(k, &s)| (k, s)).collect();
    by_time.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    for (key, seconds) in by_time {
        let key = key.clone();
        match allow.iter().find(|s| matches(s, &key)) {
            None if seconds > max => findings.push(Finding::Over { key, seconds }),
            None => {}
            Some(s) if seconds > s.max * slack => findings.push(Finding::OverAllowed {
                key,
                seconds,
                max: s.max,
            }),
            Some(_) if seconds <= max => findings.push(Finding::NowFast { key, seconds }),
            Some(s) => findings.push(Finding::Allowed {
                key,
                seconds,
                max: s.max,
                reason: s.reason.clone(),
            }),
        }
    }
    for s in allow {
        if !times.keys().any(|k| matches(s, k)) {
            findings.push(Finding::Stale {
                test: s.test.clone(),
            });
        }
    }
    findings
}

fn workspace_target_dir() -> Result<Utf8PathBuf> {
    let out = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned()))
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .output()
        .context("run cargo metadata")?;
    anyhow::ensure!(
        out.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let meta: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    let dir = meta["target_directory"]
        .as_str()
        .context("cargo metadata has no target_directory")?;
    Ok(Utf8PathBuf::from(dir))
}

fn load_allow(path: &Utf8Path) -> Result<Vec<Slow>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {path}"))?;
    Ok(toml::from_str::<AllowList>(&text)
        .with_context(|| format!("parse {path}"))?
        .slow)
}

pub fn run(args: &BudgetArgs) -> Result<ExitCode> {
    let target_dir = match &args.target_dir {
        Some(dir) => dir.clone(),
        None => workspace_target_dir()?,
    };
    let path = durations::path(&target_dir);
    let times = durations::load(&path);
    anyhow::ensure!(
        !times.is_empty(),
        "no recorded test times in {path}: run `kvasir-test test` or `kvasir-test mutants` first"
    );
    let allow = match &args.allow {
        Some(file) => load_allow(file)?,
        None => Vec::new(),
    };
    let findings = check(&times, &allow, args.max, args.slack);
    for f in &findings {
        match f {
            Finding::Over { key, seconds } => {
                println!(
                    "OVER     {seconds:7.1}s  {key}  (budget {:.0}s, not on the allow list)",
                    args.max
                );
            }
            Finding::OverAllowed { key, seconds, max } => {
                println!(
                    "OVER     {seconds:7.1}s  {key}  (allowed {max:.0}s x {})",
                    args.slack
                );
            }
            Finding::Allowed {
                key,
                seconds,
                max,
                reason,
            } => {
                println!("allowed  {seconds:7.1}s  {key}  (ceiling {max:.0}s: {reason})");
            }
            Finding::NowFast { key, seconds } => {
                println!(
                    "FAST     {seconds:7.1}s  {key}  (now within budget: remove it from the allow list)"
                );
            }
            Finding::Stale { test } => {
                println!("stale    allow-list entry matches no recorded test: {test}");
            }
        }
    }
    let total: f64 = times.values().sum();
    println!(
        "{} tests, {total:.0}s in total; {} over budget",
        times.len(),
        findings.iter().filter(|f| f.fails()).count()
    );
    Ok(if findings.iter().any(Finding::fails) {
        ExitCode::FoundProblems
    } else {
        ExitCode::Success
    })
}

#[cfg(test)]
mod test {
    use super::*;

    fn slow(test: &str, max: f64) -> Slow {
        Slow {
            test: test.to_owned(),
            max,
            reason: "r".to_owned(),
        }
    }

    #[test]
    fn judges_each_time_against_budget_and_allow_list() {
        let times: Durations = [
            ("p/lib:p/a::fast", 0.5),
            ("p/lib:p/a::new_slow", 30.0),
            ("p/lib:p/a::known", 60.0),
            ("p/lib:p/a::regressed", 200.0),
            ("p/lib:p/a::fixed", 3.0),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect();
        let allow = [
            slow("a::known", 90.0),
            slow("p/lib:p/a::regressed", 90.0),
            slow("a::fixed", 90.0),
            slow("a::gone", 5.0),
        ];
        let found = check(&times, &allow, 10.0, 1.5);
        assert_eq!(
            found,
            [
                Finding::OverAllowed {
                    key: "p/lib:p/a::regressed".into(),
                    seconds: 200.0,
                    max: 90.0
                },
                Finding::Allowed {
                    key: "p/lib:p/a::known".into(),
                    seconds: 60.0,
                    max: 90.0,
                    reason: "r".into()
                },
                Finding::Over {
                    key: "p/lib:p/a::new_slow".into(),
                    seconds: 30.0
                },
                Finding::NowFast {
                    key: "p/lib:p/a::fixed".into(),
                    seconds: 3.0
                },
                Finding::Stale {
                    test: "a::gone".into()
                },
            ]
        );
        assert_eq!(found.iter().filter(|f| f.fails()).count(), 2);
    }

    #[test]
    fn a_test_name_does_not_match_a_longer_name() {
        assert!(!matches(&slow("a::x", 1.0), "p/lib:p/a::x_y"));
        assert!(!matches(&slow("x", 1.0), "p/lib:p/a::x"));
        assert!(matches(&slow("a::x", 1.0), "p/lib:p/a::x"));
    }

    #[test]
    fn allow_list_parses_and_rejects_unknown_fields() {
        let ok: AllowList =
            toml::from_str("[[slow]]\ntest = \"a::b\"\nmax = 95.0\nreason = \"r\"\n").unwrap();
        assert_eq!(ok.slow.len(), 1);
        assert!(
            toml::from_str::<AllowList>(
                "[[slow]]\ntest = \"a\"\nmax = 1.0\nreason = \"r\"\nx = 1\n"
            )
            .is_err()
        );
    }
}
