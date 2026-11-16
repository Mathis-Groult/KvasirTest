// Copyright 2026 Mathis Groult
// SPDX-License-Identifier: GPL-3.0-or-later (KvasirTest; see NOTICE)

//! `kvasir-test compare OLD NEW`: compare the verdicts of two mutation runs across a refactor.
//!
//! When a function is split, its statements move verbatim into new functions: names and lines
//! change, so outcomes cannot be compared by name. Every run writes `mutants.out/sources.json`
//! (mutant name -> its mutated statement, see [`crate::excerpt`]); this command pairs the mutants
//! of two runs by that text and lists every pair whose verdict changed, every old mutant with no
//! counterpart (the code it mutated no longer exists as it was), and the new ones. Whole-body
//! replacements are keyed by their function, so they are counted apart.

use std::collections::BTreeMap;

use anyhow::{Context as _, bail};
use camino::{Utf8Path, Utf8PathBuf};

use crate::Result;
use crate::exit_code::ExitCode;

/// File every run writes: mutant name -> mutated source text.
pub const SOURCES_JSON: &str = "sources.json";

#[derive(Debug, clap::Args)]
#[command(
    about = "Compare two mutation runs across a refactor, pairing mutants by their mutated source",
    after_help = "OLD and NEW are output directories of `kvasir-test mutants` (the `-o` directory or \
                  its `mutants.out`). Exit 2 if a mutant caught before is not caught now, or a \
                  viable old mutant has no counterpart."
)]
pub struct CompareArgs {
    /// The run on the code before the refactor.
    old: Utf8PathBuf,
    /// The run on the code after it.
    new: Utf8PathBuf,
}

/// One run: per mutant name, its verdict and its source key (if it has one).
struct Run {
    mutants: Vec<(String, String, Option<String>)>,
}

impl Run {
    fn load(dir: &Utf8Path) -> Result<Run> {
        let dir = if dir.join("mutants.out").is_dir() {
            dir.join("mutants.out")
        } else {
            dir.to_owned()
        };
        let read = |name: &str| {
            std::fs::read_to_string(dir.join(name)).with_context(|| format!("read {dir}/{name}"))
        };
        let outcomes: serde_json::Value = serde_json::from_str(&read("outcomes.json")?)?;
        let Ok(sources) = read(SOURCES_JSON) else {
            bail!("{dir} has no {SOURCES_JSON}: rerun it with this version of kvasir-test");
        };
        let sources: BTreeMap<String, String> = serde_json::from_str(&sources)?;
        let mutants = outcomes["outcomes"]
            .as_array()
            .context("outcomes.json has no outcomes")?
            .iter()
            .filter_map(|o| {
                let name = o["scenario"]["Mutant"]["name"].as_str()?.to_owned();
                let summary = o["summary"].as_str()?.to_owned();
                let key = sources.get(&name).cloned();
                Some((name, summary, key))
            })
            .collect();
        Ok(Run { mutants })
    }

    fn by_key(&self) -> BTreeMap<&str, Vec<(&str, &str)>> {
        let mut map: BTreeMap<&str, Vec<(&str, &str)>> = BTreeMap::new();
        for (name, summary, key) in &self.mutants {
            if let Some(key) = key {
                map.entry(key).or_default().push((name, summary));
            }
        }
        map
    }

    fn unkeyed(&self) -> usize {
        self.mutants.iter().filter(|m| m.2.is_none()).count()
    }
}

/// The comparison of two runs.
#[derive(Debug, Default, PartialEq, Eq)]
struct Report {
    same: usize,
    /// (old name, old verdict, new name, new verdict)
    changed: Vec<(String, String, String, String)>,
    /// (old name, verdict)
    gone: Vec<(String, String)>,
    /// (new name, verdict)
    added: Vec<(String, String)>,
}

fn compare(old: &Run, new: &Run) -> Report {
    let (old_keys, new_keys) = (old.by_key(), new.by_key());
    let mut report = Report::default();
    for (key, olds) in &old_keys {
        let news = new_keys.get(key).map_or(&[][..], Vec::as_slice);
        // Same text more than once (repeated statements): pair them in order.
        for (k, (old_name, old_verdict)) in olds.iter().enumerate() {
            match news.get(k) {
                Some((_, new_verdict)) if new_verdict == old_verdict => report.same += 1,
                Some((new_name, new_verdict)) => report.changed.push((
                    (*old_name).to_owned(),
                    (*old_verdict).to_owned(),
                    (*new_name).to_owned(),
                    (*new_verdict).to_owned(),
                )),
                None => report
                    .gone
                    .push(((*old_name).to_owned(), (*old_verdict).to_owned())),
            }
        }
        for (new_name, new_verdict) in news.iter().skip(olds.len()) {
            report
                .added
                .push(((*new_name).to_owned(), (*new_verdict).to_owned()));
        }
    }
    for (key, news) in &new_keys {
        if !old_keys.contains_key(key) {
            report
                .added
                .extend(news.iter().map(|(n, v)| ((*n).to_owned(), (*v).to_owned())));
        }
    }
    report
}

pub fn run(args: &CompareArgs) -> Result<ExitCode> {
    let old = Run::load(&args.old)?;
    let new = Run::load(&args.new)?;
    let report = compare(&old, &new);
    println!(
        "Paired by mutated source: {} same verdict, {} changed; {} old without counterpart, {} new.",
        report.same,
        report.changed.len(),
        report.gone.len(),
        report.added.len()
    );
    println!(
        "Whole-body replacements, keyed by function and not paired: {} old, {} new.",
        old.unkeyed(),
        new.unkeyed()
    );
    for (old_name, old_verdict, new_name, new_verdict) in &report.changed {
        println!("CHANGED {old_verdict} -> {new_verdict}: {new_name}\n        was {old_name}");
    }
    for (name, verdict) in &report.gone {
        println!("GONE    ({verdict}) {name}");
    }
    for (name, verdict) in &report.added {
        if verdict != "CaughtMutant" {
            println!("NEW     ({verdict}) {name}");
        }
    }
    let worse = report
        .changed
        .iter()
        .any(|(_, old, _, new)| old == "CaughtMutant" && new != "CaughtMutant");
    // An unviable mutant without counterpart tested nothing; others mean code changed shape.
    let gone = report.gone.iter().any(|(_, verdict)| verdict != "Unviable");
    Ok(if worse || gone {
        ExitCode::FoundProblems
    } else {
        ExitCode::Success
    })
}

#[cfg(test)]
mod test {
    use super::*;

    fn run(mutants: &[(&str, &str, Option<&str>)]) -> Run {
        Run {
            mutants: mutants
                .iter()
                .map(|(n, v, k)| ((*n).to_owned(), (*v).to_owned(), k.map(str::to_owned)))
                .collect(),
        }
    }

    #[test]
    fn mutants_pair_by_source_whatever_their_names() {
        let old = run(&[
            (
                "a.rs:10:5: replace + with - in big",
                "CaughtMutant",
                Some("x = (a - b);"),
            ),
            (
                "a.rs:11:5: replace < with == in big",
                "CaughtMutant",
                Some("if (a == b) {"),
            ),
            (
                "a.rs:12:5: delete s.go(); in big",
                "MissedMutant",
                Some("s.go2();"),
            ),
            ("a.rs:9:1: replace big with ()", "MissedMutant", None),
        ]);
        let new = run(&[
            (
                "a.rs:40:5: replace + with - in part1",
                "CaughtMutant",
                Some("x = (a - b);"),
            ),
            (
                "a.rs:50:5: replace < with == in part2",
                "MissedMutant",
                Some("if (a == b) {"),
            ),
            (
                "a.rs:60:5: replace * with / in part2",
                "MissedMutant",
                Some("y = (c / d);"),
            ),
        ]);
        let report = compare(&old, &new);
        assert_eq!(report.same, 1);
        assert_eq!(
            report.changed,
            [(
                "a.rs:11:5: replace < with == in big".to_owned(),
                "CaughtMutant".to_owned(),
                "a.rs:50:5: replace < with == in part2".to_owned(),
                "MissedMutant".to_owned()
            )]
        );
        assert_eq!(report.gone.len(), 1);
        assert_eq!(
            report.added,
            [(
                "a.rs:60:5: replace * with / in part2".to_owned(),
                "MissedMutant".to_owned()
            )]
        );
        assert_eq!((old.unkeyed(), new.unkeyed()), (1, 0));
    }

    #[test]
    fn repeated_statements_pair_in_order() {
        let old = run(&[
            ("1", "CaughtMutant", Some("k")),
            ("2", "CaughtMutant", Some("k")),
        ]);
        let new = run(&[("3", "CaughtMutant", Some("k"))]);
        let report = compare(&old, &new);
        assert_eq!(report.same, 1);
        assert_eq!(report.gone, [("2".to_owned(), "CaughtMutant".to_owned())]);
    }
}
