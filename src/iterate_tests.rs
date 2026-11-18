// SPDX-License-Identifier: GPL-3.0-or-later

//! Narrow an `--iterate` re-run in schema mode to the tests that changed.
//!
//! A mutant the previous run MISSED already passed every test that reaches it. When only
//! test-only code changed since (the usual case: a golden widened by appended scenes and
//! re-blessed), the code under test and every unchanged test are the same, so running the
//! unchanged tests again cannot change the verdict: only the tests defined in the changed test
//! modules and tests that did not exist then need to run. Anything else that changed (code
//! that is not test-only, a removed file, a data file read by non-test code, other test
//! arguments) makes the re-run a full one, as before.
//!
//! The previous run's state is in `mutants.out.old` (where `--iterate` runs rotate it): the
//! file hashes of the tree, the test arguments and the unit keys, written by every schema run.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Context as _;
use camino::{Utf8Path, Utf8PathBuf};
use ignore::WalkBuilder;
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::Result;
use crate::probe::fnv64;

/// The state file every schema run writes into its output directory.
pub const STATE_JSON: &str = "iterate-state.json";

/// What a later `--iterate` run compares itself with.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct State {
    /// The test arguments, as one string: other arguments select other tests.
    pub args: String,
    /// FNV-1a hash of every source file, by tree-relative path with `/` separators.
    pub tree: BTreeMap<String, u64>,
    /// Keys of every test unit of the baseline.
    pub units: BTreeSet<String>,
    /// Names of the mutants held by the schema, in switch-id order (id 1 first).
    #[serde(default)]
    pub mutants: Vec<String>,
    /// Each unit's baseline: its time, the schema mutants it reached (indexes into `mutants`)
    /// and whether it was flaky. A later `--iterate` run reuses it for unchanged units.
    #[serde(default)]
    pub unit_runs: BTreeMap<String, UnitRun>,
    /// Wall time of the whole baseline, seconds: left-out mutants' timeout comes from it.
    #[serde(default)]
    pub wall_secs: f64,
}

/// One unit's recorded baseline.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct UnitRun {
    pub secs: f64,
    pub reach: Vec<u32>,
    pub flaky: bool,
}

/// The previous baseline of units an `--iterate` run need not run again: unchanged tests on
/// unchanged code pass again and evaluate the same guards, so their reach (mapped from the
/// previous run's switch ids to this run's by mutant name) is the same.
pub struct Reuse {
    by_unit: BTreeMap<String, UnitRun>,
    old_names: Vec<String>,
    id_of: BTreeMap<String, u32>,
    pub wall_secs: f64,
}

impl Reuse {
    /// `None` unless the plan narrows and every mutant now in the schema had its reach
    /// recorded by the previous run (a mutant new to the schema has no recorded reach).
    pub fn new(plan: &Plan, old: &State, active: &[(u32, String)]) -> Option<Reuse> {
        if !matches!(plan, Plan::Changed { .. }) || old.unit_runs.is_empty() {
            return None;
        }
        let known: BTreeSet<&String> = old.mutants.iter().collect();
        if !active.iter().all(|(_, name)| known.contains(name)) {
            return None;
        }
        Some(Reuse {
            by_unit: old.unit_runs.clone(),
            old_names: old.mutants.clone(),
            id_of: active
                .iter()
                .map(|(id, name)| (name.clone(), *id))
                .collect(),
            wall_secs: old.wall_secs,
        })
    }

    /// The recorded run of an unchanged unit (`rerun` = the plan says it changed), with its
    /// reach in this run's switch ids.
    pub fn get(&self, key: &str, rerun: bool) -> Option<(f64, BTreeSet<u32>, bool)> {
        if rerun {
            return None;
        }
        let run = self.by_unit.get(key)?;
        let reach = run
            .reach
            .iter()
            .filter_map(|&i| self.old_names.get(i as usize))
            .filter_map(|name| self.id_of.get(name).copied())
            .collect();
        Some((run.secs, reach, run.flaky))
    }
}

/// Hash every file of the tree, skipping build output, mutation output and VCS directories.
pub fn tree_hashes(root: &Utf8Path) -> Result<BTreeMap<String, u64>> {
    let mut tree = BTreeMap::new();
    let walk = WalkBuilder::new(root)
        .hidden(false)
        .ignore(false)
        .require_git(true)
        .filter_entry(|entry| {
            let name = entry.file_name().to_string_lossy();
            !matches!(
                name.as_ref(),
                "target" | "mutants.out" | "mutants.out.old" | ".git" | ".hg" | ".jj"
            )
        })
        .build();
    for entry in walk {
        let entry = entry?;
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(root) else {
            continue;
        };
        let bytes = std::fs::read(entry.path())
            .with_context(|| format!("hash {}", entry.path().display()))?;
        let key = relative.to_string_lossy().replace('\\', "/");
        tree.insert(key, fnv64(&bytes));
    }
    Ok(tree)
}

/// How the previously missed mutants are re-run.
#[derive(Debug, PartialEq, Eq)]
pub enum Plan {
    /// Every reaching test, as without this module; the reason is logged.
    Full(String),
    /// Only tests that are new or defined in one of these test-only files (or below them).
    Changed {
        test_files: BTreeSet<String>,
        old_units: BTreeSet<String>,
        missed: BTreeSet<String>,
        /// Every file of the current tree, to find the file a test is defined in.
        files: BTreeSet<String>,
    },
}

/// Decide the plan from the previous state and the current tree. `read` returns the current
/// text of a tree-relative file.
pub fn plan(
    old: &State,
    missed: BTreeSet<String>,
    args: &str,
    tree: &BTreeMap<String, u64>,
    read: &dyn Fn(&str) -> Option<String>,
) -> Plan {
    if old.args != args {
        return Plan::Full("the test arguments changed".to_owned());
    }
    let changed: BTreeSet<&String> = old
        .tree
        .iter()
        .filter(|(path, hash)| tree.get(*path) != Some(hash))
        .map(|(path, _)| path)
        .chain(tree.keys().filter(|path| !old.tree.contains_key(*path)))
        .collect();
    let mut test_files = BTreeSet::new();
    for path in changed {
        if !tree.contains_key(path) {
            return Plan::Full(format!("{path} was removed"));
        }
        if is_rust(path) {
            if !is_test_only(path, tree, read) {
                return Plan::Full(format!("{path} is not test-only code"));
            }
            if let Some(user) = other_user(path, tree, read) {
                return Plan::Full(format!("{user} uses the changed test module {path}"));
            }
            test_files.insert(path.clone());
        } else {
            match data_readers(path, tree, read) {
                Ok(readers) => test_files.extend(readers),
                Err(reason) => return Plan::Full(reason),
            }
        }
    }
    Plan::Changed {
        test_files,
        old_units: old.units.clone(),
        missed,
        files: tree.keys().cloned().collect(),
    }
}

impl Plan {
    /// Whether a previously missed mutant must run this unit again. `package_dir` is the
    /// package's tree-relative directory (empty for the root package), `target` and `name`
    /// the unit's libtest target (`lib:crate`) and test path.
    pub fn reruns(&self, key: &str, package_dir: &str, target: &str, name: &str) -> bool {
        let Plan::Changed {
            test_files,
            old_units,
            files,
            ..
        } = self
        else {
            return true;
        };
        if !old_units.contains(key) || !target.starts_with("lib:") {
            // A new test, or a test outside the library (whose files this does not map).
            return true;
        }
        let Some(file) = unit_file(package_dir, name, files) else {
            return true;
        };
        test_files
            .iter()
            .any(|changed| file == *changed || file.starts_with(&module_dir(changed)))
    }

    /// Whether this mutant was missed by the previous run.
    pub fn was_missed(&self, name: &str) -> bool {
        matches!(self, Plan::Changed { missed, .. } if missed.contains(name))
    }
}

/// The file holding a library test: the longest prefix of its module path that names a file.
fn unit_file(package_dir: &str, name: &str, files: &BTreeSet<String>) -> Option<String> {
    let src = join(package_dir, "src");
    let modules: Vec<&str> = name.split("::").collect();
    let modules = &modules[..modules.len().saturating_sub(1)];
    for n in (1..=modules.len()).rev() {
        let base = format!("{src}/{}", modules[..n].join("/"));
        for candidate in [format!("{base}.rs"), format!("{base}/mod.rs")] {
            if files.contains(&candidate) {
                return Some(candidate);
            }
        }
    }
    ["lib.rs", "main.rs"]
        .iter()
        .map(|f| format!("{src}/{f}"))
        .find(|p| files.contains(p))
}

fn is_rust(path: &str) -> bool {
    Utf8Path::new(path).extension() == Some("rs")
}

fn join(dir: &str, rest: &str) -> String {
    if dir.is_empty() {
        rest.to_owned()
    } else {
        format!("{dir}/{rest}")
    }
}

/// `a/b/x.rs` -> `a/b/x/`, `a/b/mod.rs` -> `a/b/`: where the module's children live.
fn module_dir(file: &str) -> String {
    let path = Utf8Path::new(file);
    let parent = path
        .parent()
        .map_or(String::new(), |p| p.as_str().to_owned());
    match path.file_stem() {
        Some("mod" | "lib" | "main") => format!("{parent}/"),
        Some(stem) => format!("{}/", join(&parent, stem)),
        None => format!("{file}/"),
    }
}

/// A file only compiled for tests: under a package's `tests/`, or a module declared with
/// `#[cfg(test)]` (directly or through a test-only ancestor).
fn is_test_only(
    path: &str,
    tree: &BTreeMap<String, u64>,
    read: &dyn Fn(&str) -> Option<String>,
) -> bool {
    let p = Utf8Path::new(path);
    if p.components().any(|c| c.as_str() == "tests") && !p.components().any(|c| c.as_str() == "src")
    {
        return true;
    }
    let Some((name, parents)) = declaring_files(p) else {
        return false;
    };
    for parent in parents.iter().filter(|f| tree.contains_key(f.as_str())) {
        let Some(text) = read(parent.as_str()) else {
            continue;
        };
        match declaration(&text, &name) {
            Some(true) => return true,
            Some(false) => return is_test_only(parent.as_str(), tree, read),
            None => {}
        }
    }
    false
}

/// The module name a file defines and the files that may declare it.
fn declaring_files(p: &Utf8Path) -> Option<(String, Vec<Utf8PathBuf>)> {
    let stem = p.file_stem()?;
    let dir = p.parent()?;
    let (name, outer) = if stem == "mod" {
        (dir.file_name()?.to_owned(), dir.parent()?)
    } else if matches!(stem, "lib" | "main") && dir.file_name() == Some("src") {
        return None;
    } else {
        (stem.to_owned(), dir)
    };
    let mut parents = vec![outer.join("mod.rs")];
    if outer.file_name() == Some("src") {
        parents.push(outer.join("lib.rs"));
        parents.push(outer.join("main.rs"));
    } else {
        parents.push(Utf8PathBuf::from(format!("{outer}.rs")));
    }
    Some((name, parents))
}

/// `Some(true)` if `text` declares `mod name;` under `#[cfg(test)]`, `Some(false)` if it
/// declares it otherwise, `None` if not at all.
fn declaration(text: &str, name: &str) -> Option<bool> {
    let re = Regex::new(&format!(
        r"(?m)((?:^[ \t]*#\[[^\n]*\][ \t]*\n)*)^[ \t]*(?:pub(?:\([^)]*\))?[ \t]+)?mod[ \t]+{}[ \t]*;",
        regex::escape(name)
    ))
    .expect("valid regex");
    let attrs = re.captures(text)?.get(1).map_or("", |m| m.as_str());
    Some(attrs.contains("#[cfg(test)]") || attrs.contains("#[cfg(all(test"))
}

/// Another Rust file of the same package that names the changed module (`name::`): its tests
/// may call into the changed code, so they changed too.
fn other_user(
    path: &str,
    tree: &BTreeMap<String, u64>,
    read: &dyn Fn(&str) -> Option<String>,
) -> Option<String> {
    let p = Utf8Path::new(path);
    let name = match p.file_stem()? {
        "mod" => p.parent()?.file_name()?,
        stem => stem,
    };
    // A whole path segment: `golden::` must not match `script_golden::`.
    let needle =
        Regex::new(&format!(r"(?:^|[^A-Za-z0-9_]){}::", regex::escape(name))).expect("valid regex");
    let package = package_root(path);
    tree.keys()
        .filter(|f| f.as_str() != path && is_rust(f) && f.starts_with(&package))
        .find(|f| read(f).is_some_and(|text| needle.is_match(&text)))
        .cloned()
}

/// Test-only Rust files of the package that name a changed data file; an error when none does
/// or when non-test code also does (it might read the file at run time).
fn data_readers(
    path: &str,
    tree: &BTreeMap<String, u64>,
    read: &dyn Fn(&str) -> Option<String>,
) -> std::result::Result<BTreeSet<String>, String> {
    let file_name = Utf8Path::new(path).file_name().unwrap_or(path);
    let package = package_root(path);
    let mut readers = BTreeSet::new();
    for f in tree
        .keys()
        .filter(|f| is_rust(f) && f.starts_with(&package))
    {
        if read(f).is_some_and(|text| text.contains(file_name)) {
            if !is_test_only(f, tree, read) {
                return Err(format!("{f} (not test-only) names the changed file {path}"));
            }
            readers.insert(f.clone());
        }
    }
    if readers.is_empty() {
        return Err(format!("no test module names the changed file {path}"));
    }
    Ok(readers)
}

/// The package directory a file belongs to: the part before `/src/` or `/tests/`, or the
/// file's own directory.
fn package_root(path: &str) -> String {
    for marker in ["/src/", "/tests/"] {
        if let Some(i) = path.find(marker) {
            return path[..=i].to_owned();
        }
    }
    for marker in ["src/", "tests/"] {
        if path.starts_with(marker) {
            return String::new();
        }
    }
    Utf8Path::new(path)
        .parent()
        .map_or(String::new(), |p| format!("{p}/"))
}

#[cfg(test)]
mod test {
    use super::*;

    const LIB: &str = "#[cfg(test)]\nmod golden;\npub mod world;\n";
    const WORLD: &str =
        "pub mod writes;\n#[cfg(test)]\n#[allow(dead_code)]\npub(crate) mod flush_golden;\n";

    fn files() -> BTreeMap<String, String> {
        [
            ("p/src/lib.rs", LIB),
            ("p/src/golden.rs", "fn a() {}"),
            ("p/src/world.rs", WORLD),
            (
                "p/src/world/writes.rs",
                "pub fn f() {}\n#[cfg(test)]\nmod tests {}\n",
            ),
            (
                "p/src/world/flush_golden.rs",
                "mod helpers;\nconst F: &str = \"flush.txt\";",
            ),
            ("p/src/world/flush_golden/helpers.rs", "fn h() {}"),
            ("p/tests/golden/flush.txt", "a\n"),
            ("p/Cargo.toml", "[package]"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
    }

    fn state(files: &BTreeMap<String, String>) -> State {
        State {
            args: "--lib".to_owned(),
            tree: files
                .iter()
                .map(|(k, v)| (k.clone(), fnv64(v.as_bytes())))
                .collect(),
            units: [
                "p/lib:p/world::flush_golden::flush",
                "p/lib:p/world::flush_golden::helpers::h",
                "p/lib:p/world::writes::tests::t",
                "p/lib:p/golden::g",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
            ..State::default()
        }
    }

    fn plan_after(edit: &[(&str, &str)]) -> Plan {
        let old_files = files();
        let old = state(&old_files);
        let mut new_files = old_files;
        for (path, text) in edit {
            if text.is_empty() {
                new_files.remove(*path);
            } else {
                new_files.insert((*path).to_owned(), (*text).to_owned());
            }
        }
        let tree = state(&new_files).tree;
        let read = |p: &str| new_files.get(p).cloned();
        plan(
            &old,
            BTreeSet::from(["m".to_owned()]),
            "--lib",
            &tree,
            &read,
        )
    }

    fn reruns(plan: &Plan, name: &str) -> bool {
        plan.reruns(&format!("p/lib:p/{name}"), "p", "lib:p", name)
    }

    #[test]
    fn a_widened_golden_reruns_only_its_own_tests_and_its_children() {
        let plan = plan_after(&[
            (
                "p/src/world/flush_golden.rs",
                "mod helpers;\nconst F: &str = \"flush.txt\"; // more",
            ),
            ("p/tests/golden/flush.txt", "a\nb\n"),
        ]);
        let Plan::Changed { test_files, .. } = &plan else {
            panic!("expected a narrowed plan, got {plan:?}");
        };
        assert_eq!(
            test_files.iter().collect::<Vec<_>>(),
            ["p/src/world/flush_golden.rs"]
        );
        assert!(reruns(&plan, "world::flush_golden::flush"));
        assert!(reruns(&plan, "world::flush_golden::helpers::h"));
        assert!(!reruns(&plan, "world::writes::tests::t"));
        assert!(!reruns(&plan, "golden::g"));
        // A test that did not exist in the previous run always runs.
        assert!(reruns(&plan, "world::writes::tests::new_one"));
        assert!(plan.was_missed("m"));
        assert!(!plan.was_missed("other"));
    }

    #[test]
    fn nothing_changed_reruns_no_old_test() {
        let plan = plan_after(&[]);
        assert!(!reruns(&plan, "world::flush_golden::flush"));
    }

    #[test]
    fn any_change_outside_test_only_code_reruns_everything() {
        for (edit, why) in [
            (
                ("p/src/world/writes.rs", "pub fn f() { 1; }"),
                "not test-only",
            ),
            (("p/src/world.rs", "pub mod writes;\n"), "not test-only"),
            (("p/Cargo.toml", "[package] x"), "no test module names"),
            (("p/src/golden.rs", ""), "was removed"),
        ] {
            match plan_after(&[edit]) {
                Plan::Full(reason) => assert!(reason.contains(why), "{edit:?}: {reason}"),
                other @ Plan::Changed { .. } => {
                    panic!("{edit:?}: expected a full re-run, got {other:?}")
                }
            }
        }
    }

    #[test]
    fn a_test_module_used_from_another_file_reruns_everything() {
        let plan = plan_after(&[
            ("p/src/golden.rs", "fn a() { 2; }"),
            (
                "p/src/world/writes.rs",
                "pub fn f() {}\n#[cfg(test)]\nmod tests { use crate::golden::a; }\n",
            ),
        ]);
        assert!(matches!(plan, Plan::Full(_)), "{plan:?}");
    }

    #[test]
    fn a_module_named_like_the_end_of_another_is_not_its_user() {
        let plan = plan_after(&[
            ("p/src/golden.rs", "fn a() { 2; }"),
            (
                "p/src/world/writes.rs",
                "pub fn f() {}\n#[cfg(test)]\nmod tests { use crate::script_golden::a; }\n",
            ),
        ]);
        // writes.rs changed outside test-only code, so the plan is full, but not because of
        // `script_golden::`: check the reason.
        assert!(
            matches!(&plan, Plan::Full(r) if r.contains("not test-only")),
            "{plan:?}"
        );
        let files = files();
        let tree = state(&files).tree;
        let mut with_user = files.clone();
        with_user.insert(
            "p/src/world/flush_golden.rs".into(),
            "fn x() { crate::script_golden::a(); }".into(),
        );
        let read = |p: &str| with_user.get(p).cloned();
        assert_eq!(other_user("p/src/golden.rs", &tree, &read), None);
    }

    #[test]
    fn other_test_arguments_rerun_everything() {
        let files = files();
        let old = state(&files);
        let read = |p: &str| files.get(p).cloned();
        let plan = plan(&old, BTreeSet::new(), "--lib -- golden", &old.tree, &read);
        assert!(matches!(plan, Plan::Full(r) if r.contains("arguments")));
    }

    #[test]
    fn reuse_maps_reach_by_name_and_needs_every_mutant_known() {
        let mut old = state(&files());
        old.mutants = vec!["a".into(), "b".into(), "c".into()];
        old.unit_runs.insert(
            "u".into(),
            UnitRun {
                secs: 2.0,
                reach: vec![0, 2],
                flaky: false,
            },
        );
        let plan = plan_after(&[]);
        // This run holds c (id 1) and a (id 2); b was caught last time and is gone.
        let reuse = Reuse::new(&plan, &old, &[(1, "c".into()), (2, "a".into())]).unwrap();
        assert_eq!(
            reuse.get("u", false),
            Some((2.0, BTreeSet::from([1, 2]), false))
        );
        assert_eq!(reuse.get("u", true), None);
        assert_eq!(reuse.get("other", false), None);
        // A mutant the previous schema did not hold: no reuse at all.
        assert!(Reuse::new(&plan, &old, &[(1, "new".into())]).is_none());
        assert!(Reuse::new(&Plan::Full(String::new()), &old, &[(1, "a".into())]).is_none());
    }

    #[test]
    fn declarations_are_read_with_their_attributes() {
        assert_eq!(declaration(WORLD, "flush_golden"), Some(true));
        assert_eq!(declaration(WORLD, "writes"), Some(false));
        assert_eq!(declaration(WORLD, "other"), None);
        assert_eq!(
            declaration("#[cfg(all(test, unix))]\nmod x;", "x"),
            Some(true)
        );
        assert_eq!(module_dir("a/b/x.rs"), "a/b/x/");
        assert_eq!(module_dir("a/b/mod.rs"), "a/b/");
        assert_eq!(package_root("crates/p/src/a.rs"), "crates/p/");
        assert_eq!(package_root("src/a.rs"), "");
    }
}
