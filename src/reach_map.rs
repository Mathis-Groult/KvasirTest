// Copyright 2026 Mathis Groult
// KvasirTest: see docs/decisions/0002-test-mode-reach-map.md.

//! The reach map of `test` mode, and what a change selects from it.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// Format version of the stored map; a different version means a full run.
pub const MAP_VERSION: u32 = 1;

/// The functions of one instrumented source file.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct FileEntry {
    pub outside_hash: u64,
    /// Function key -> hash of the function item.
    pub fns: BTreeMap<String, u64>,
}

/// What one test did on its last run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TestEntry {
    /// Keys of the functions it executed.
    pub reach: BTreeSet<String>,
    pub seconds: f64,
    pub passed: bool,
}

/// The state of the tree a map was recorded against (or the tree now).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// Packages and arguments of the run: a map is only valid for the same scope.
    pub scope: String,
    /// Hash of every input that is not a probed function (other files, toolchain).
    pub env_hash: u64,
    /// Instrumented files by tree-relative path.
    pub files: BTreeMap<String, FileEntry>,
}

/// The stored reach map.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReachMap {
    pub version: u32,
    pub scope: String,
    pub env_hash: u64,
    pub files: BTreeMap<String, FileEntry>,
    /// Test key -> its last run.
    pub tests: BTreeMap<String, TestEntry>,
}

/// What a run has to do.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Plan {
    /// Run (and re-record) every test, for this reason.
    Full(String),
    /// Run the tests that reached these changed functions (possibly none).
    Changed(BTreeSet<String>),
}

/// Compare the tree now with the recorded map.
pub fn plan(map: Option<&ReachMap>, now: &Snapshot) -> Plan {
    let Some(map) = map else {
        return Plan::Full("no reach map yet".into());
    };
    if map.version != MAP_VERSION {
        return Plan::Full("reach map from another KvasirTest version".into());
    }
    if map.scope != now.scope {
        return Plan::Full("other packages or test arguments than the recorded run".into());
    }
    if map.env_hash != now.env_hash {
        return Plan::Full(
            "a file outside the tested crates' functions changed (other crate, manifest, data, toolchain)"
                .into(),
        );
    }
    let paths: BTreeSet<&String> = map.files.keys().chain(now.files.keys()).collect();
    let mut changed = BTreeSet::new();
    for path in paths {
        let (Some(old), Some(new)) = (map.files.get(path), now.files.get(path)) else {
            return Plan::Full(format!("{path} was added or removed"));
        };
        if old.outside_hash != new.outside_hash {
            return Plan::Full(format!("{path} changed outside its functions"));
        }
        for key in old.fns.keys().chain(new.fns.keys()) {
            if old.fns.get(key) != new.fns.get(key) {
                changed.insert(key.clone());
            }
        }
    }
    Plan::Changed(changed)
}

/// Tests to run for `changed` functions: those that reached one, those the map does not know
/// (new tests), and those that failed last time.
pub fn select<'a>(
    map: &ReachMap,
    changed: &BTreeSet<String>,
    tests: impl IntoIterator<Item = &'a String>,
) -> BTreeSet<String> {
    tests
        .into_iter()
        .filter(|t| match map.tests.get(*t) {
            None => true,
            Some(entry) => !entry.passed || !entry.reach.is_disjoint(changed),
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod test {
    use pretty_assertions::assert_eq;

    use super::*;

    fn file(outside: u64, fns: &[(&str, u64)]) -> FileEntry {
        FileEntry {
            outside_hash: outside,
            fns: fns.iter().map(|(k, h)| ((*k).to_owned(), *h)).collect(),
        }
    }

    fn snapshot(files: &[(&str, FileEntry)]) -> Snapshot {
        Snapshot {
            scope: "s".into(),
            env_hash: 1,
            files: files
                .iter()
                .map(|(p, f)| ((*p).to_owned(), f.clone()))
                .collect(),
        }
    }

    fn map_of(now: &Snapshot, tests: &[(&str, &[&str], bool)]) -> ReachMap {
        ReachMap {
            version: MAP_VERSION,
            scope: now.scope.clone(),
            env_hash: now.env_hash,
            files: now.files.clone(),
            tests: tests
                .iter()
                .map(|(t, reach, passed)| {
                    (
                        (*t).to_owned(),
                        TestEntry {
                            reach: reach.iter().map(|&r| r.to_owned()).collect(),
                            seconds: 1.0,
                            passed: *passed,
                        },
                    )
                })
                .collect(),
        }
    }

    fn keys(s: &[&str]) -> BTreeSet<String> {
        s.iter().map(|&s| s.to_owned()).collect()
    }

    #[test]
    fn changed_functions_select_the_tests_that_reached_them() {
        let before = snapshot(&[("a.rs", file(9, &[("a.rs::f", 1), ("a.rs::g", 2)]))]);
        let map = map_of(
            &before,
            &[
                ("t_f", &["a.rs::f"], true),
                ("t_g", &["a.rs::g"], true),
                ("t_both", &["a.rs::f", "a.rs::g"], true),
            ],
        );
        let after = snapshot(&[(
            "a.rs",
            file(9, &[("a.rs::f", 1), ("a.rs::g", 3), ("a.rs::new", 4)]),
        )]);
        let Plan::Changed(changed) = plan(Some(&map), &after) else {
            panic!("expected a selection");
        };
        assert_eq!(changed, keys(&["a.rs::g", "a.rs::new"]));
        let all = keys(&["t_f", "t_g", "t_both", "t_new"]);
        assert_eq!(
            select(&map, &changed, &all),
            keys(&["t_both", "t_g", "t_new"])
        );
    }

    #[test]
    fn failed_tests_always_run_again() {
        let now = snapshot(&[]);
        let map = map_of(&now, &[("t_failed", &[], false), ("t_ok", &[], true)]);
        let all = keys(&["t_failed", "t_ok"]);
        assert_eq!(select(&map, &BTreeSet::new(), &all), keys(&["t_failed"]));
    }

    #[test]
    fn anything_outside_functions_means_a_full_run() {
        let before = snapshot(&[("a.rs", file(9, &[("a.rs::f", 1)]))]);
        let map = map_of(&before, &[]);
        assert!(matches!(plan(None, &before), Plan::Full(_)));
        assert!(matches!(
            plan(
                Some(&map),
                &snapshot(&[("a.rs", file(8, &[("a.rs::f", 1)]))])
            ),
            Plan::Full(_)
        ));
        assert!(matches!(plan(Some(&map), &snapshot(&[])), Plan::Full(_)));
        let mut env_changed = before.clone();
        env_changed.env_hash = 2;
        assert!(matches!(plan(Some(&map), &env_changed), Plan::Full(_)));
        let mut other_scope = before.clone();
        other_scope.scope = "t".into();
        assert!(matches!(plan(Some(&map), &other_scope), Plan::Full(_)));
        assert_eq!(plan(Some(&map), &before), Plan::Changed(BTreeSet::new()));
    }
}
