// Copyright 2026 Mathis Groult
// SPDX-License-Identifier: GPL-3.0-or-later (KvasirTest; see NOTICE)

//! Known equivalent mutants, listed in a checked-in file, so they are not reported as missed on
//! every run (and argued again by hand).
//!
//! The file is `.cargo/mutants-equivalent.toml` in the workspace, or `--equivalent FILE`:
//!
//! ```toml
//! [[equivalent]]
//! name = "src/display.rs: replace | with ^ in DisplayEntity::read_display_nbt"
//! reason = "(sky << 4) and (block & 0xF) never share a bit"
//! # line = 611   # only when the same name occurs more than once in the function
//! ```
//!
//! `name` is the mutant name as printed, with or without its `:line:col`; matching ignores the
//! location, so an entry survives edits that move the code. Listed mutants are not tested: they
//! are printed as `EQUIV` with the reason, written to `mutants.out/equivalent.txt`, and do not
//! count as missed or set the exit code. An entry that matches nothing in a run that covers its
//! file is reported, since the code it described has changed.

use std::collections::BTreeSet;

use anyhow::{Context as _, ensure};
use camino::{Utf8Path, Utf8PathBuf};
use serde::Deserialize;
use tracing::warn;

use crate::{Mutant, Result};

/// The default file, relative to the workspace root.
pub const DEFAULT_PATH: &str = ".cargo/mutants-equivalent.toml";

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    equivalent: Vec<Entry>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    name: String,
    reason: String,
    line: Option<usize>,
}

/// The equivalent-mutant list of a run.
#[derive(Debug, Default)]
pub struct Equivalents {
    entries: Vec<Entry>,
}

/// A mutant left out of the run as equivalent, with the reason given.
pub struct Equivalent {
    pub mutant: Mutant,
    pub reason: String,
}

impl Equivalents {
    /// Read `explicit`, or the default file under `root` if it exists (else an empty list).
    pub fn load(explicit: Option<&Utf8Path>, root: &Utf8Path) -> Result<Self> {
        let path: Utf8PathBuf = if let Some(p) = explicit {
            p.to_owned()
        } else {
            let p = root.join(DEFAULT_PATH);
            if !p.is_file() {
                return Ok(Equivalents::default());
            }
            p
        };
        let text =
            std::fs::read_to_string(&path).with_context(|| format!("read equivalents {path}"))?;
        Self::parse(&text).with_context(|| format!("parse equivalents {path}"))
    }

    fn parse(text: &str) -> Result<Self> {
        let file: File = toml::from_str(text)?;
        for e in &file.equivalent {
            ensure!(!e.reason.trim().is_empty(), "{:?} has no reason", e.name);
        }
        Ok(Equivalents {
            entries: file.equivalent,
        })
    }

    /// Split `mutants` into those to test and those listed as equivalent. `filtered` says whether
    /// name filters (`--re`, `--exclude-re`) may have dropped listed mutants on purpose.
    pub fn split(&self, mutants: Vec<Mutant>, filtered: bool) -> (Vec<Mutant>, Vec<Equivalent>) {
        if self.entries.is_empty() {
            return (mutants, Vec::new());
        }
        let files: BTreeSet<String> = mutants
            .iter()
            .map(|m| m.source_file.tree_relative_slashes())
            .collect();
        let mut used = vec![false; self.entries.len()];
        let mut keep = Vec::with_capacity(mutants.len());
        let mut equivalent = Vec::new();
        for mutant in mutants {
            let name = mutant.name(false);
            let hit = self.entries.iter().position(|e| {
                strip_location(&e.name) == name
                    && e.line.is_none_or(|l| l == mutant.span.start.line)
            });
            match hit {
                Some(i) => {
                    used[i] = true;
                    equivalent.push(Equivalent {
                        reason: self.entries[i].reason.clone(),
                        mutant,
                    });
                }
                None => keep.push(mutant),
            }
        }
        for (entry, used) in self.entries.iter().zip(used) {
            let file = strip_location(&entry.name)
                .split_once(": ")
                .map(|(f, _)| f.to_owned())
                .unwrap_or_default();
            if !used && !filtered && files.contains(&file) {
                warn!(
                    name = entry.name,
                    "equivalent entry matches no mutant of this run: the code changed; update or remove it"
                );
            }
        }
        (keep, equivalent)
    }
}

/// `file:12:5: change` -> `file: change`.
fn strip_location(name: &str) -> String {
    let Some((file, rest)) = name.split_once(':') else {
        return name.to_owned();
    };
    let mut parts = rest.splitn(3, ':');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(line), Some(col), Some(tail))
            if line.chars().all(|c| c.is_ascii_digit())
                && !line.is_empty()
                && col.chars().all(|c| c.is_ascii_digit())
                && !col.is_empty() =>
        {
            format!("{file}:{tail}")
        }
        _ => name.to_owned(),
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::Options;
    use crate::visit::mutate_source_str;

    const CODE: &str = "fn f(a: u8, b: u8) -> u8 {\n    (a << 4) | (b & 0xF)\n}\nfn g(x: u8) -> u8 {\n    x | 1\n}\n";

    #[test]
    fn locations_are_ignored_when_matching() {
        assert_eq!(
            strip_location("src/a.rs:611:35: replace | with ^ in f"),
            "src/a.rs: replace | with ^ in f"
        );
        assert_eq!(
            strip_location("src/a.rs: replace a: b"),
            "src/a.rs: replace a: b"
        );
    }

    #[test]
    fn listed_mutants_are_left_out_with_their_reason() {
        let eq = Equivalents::parse(
            r#"
[[equivalent]]
name = "src/main.rs:2:14: replace | with ^ in f"
reason = "disjoint bits"
"#,
        )
        .unwrap();
        let mutants = mutate_source_str(CODE, &Options::default()).unwrap();
        let n = mutants.len();
        let (keep, equivalent) = eq.split(mutants, false);
        assert_eq!(equivalent.len(), 1);
        assert_eq!(equivalent[0].reason, "disjoint bits");
        assert_eq!(
            equivalent[0].mutant.name(false),
            "src/main.rs: replace | with ^ in f"
        );
        assert_eq!(keep.len(), n - 1);
        assert!(
            keep.iter()
                .any(|m| m.name(false) == "src/main.rs: replace | with ^ in g")
        );
    }

    #[test]
    fn entries_need_a_reason_and_known_keys() {
        assert!(Equivalents::parse("[[equivalent]]\nname = \"x\"\nreason = \" \"\n").is_err());
        assert!(
            Equivalents::parse("[[equivalent]]\nname = \"x\"\nreason = \"r\"\nwhy = 1\n").is_err()
        );
    }

    #[test]
    fn line_picks_one_of_several_same_names() {
        let code = "fn f(a: u8) -> u8 {\n    let x = a | 1;\n    x | 2\n}\n";
        let eq = Equivalents::parse(
            "[[equivalent]]\nname = \"src/main.rs: replace | with ^ in f\"\nreason = \"r\"\nline = 3\n",
        )
        .unwrap();
        let (_, equivalent) =
            eq.split(mutate_source_str(code, &Options::default()).unwrap(), false);
        assert_eq!(equivalent.len(), 1);
        assert_eq!(equivalent[0].mutant.span.start.line, 3);
    }
}
