// SPDX-License-Identifier: GPL-3.0-or-later

//! Recorded test times: every unit's time from the last run that ran it, kept under the target
//! directory by both modes (`mutants` baseline and `test`).
//!
//! The times order the next baseline (longest first) and feed `kvasir-test budget`. They are a
//! cache: a missing or unreadable file only means no ordering and nothing to check.

use std::collections::BTreeMap;
use std::fs;

use camino::{Utf8Path, Utf8PathBuf};
use tracing::warn;

/// The file, relative to the target directory.
pub const FILE: &str = "kvasir-test/test-durations.json";

/// Seconds per unit, by [`crate::schema_runner::Unit::key`].
pub type Durations = BTreeMap<String, f64>;

/// The recorded times file of a target directory.
pub fn path(target_dir: &Utf8Path) -> Utf8PathBuf {
    target_dir.join(FILE)
}

/// Recorded times (empty if none or unreadable).
pub fn load(path: &Utf8Path) -> Durations {
    fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Record `ran` over the earlier times (other units keep theirs). Failures are logged, not
/// returned: nothing a run reports depends on this file.
pub fn record(path: &Utf8Path, ran: impl IntoIterator<Item = (String, f64)>) {
    let mut all = load(path);
    all.extend(ran);
    let written = serde_json::to_string(&all)
        .map_err(std::io::Error::from)
        .and_then(|text| {
            if let Some(dir) = path.parent() {
                fs::create_dir_all(dir)?;
            }
            let tmp = path.with_extension("json.tmp");
            fs::write(&tmp, text)?;
            fs::rename(&tmp, path)
        });
    if let Err(err) = written {
        warn!(?err, %path, "could not save test times");
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn record_merges_and_load_tolerates_garbage() {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::try_from(dir.path().join("k/d.json")).unwrap();
        assert!(load(&path).is_empty());
        record(&path, [("a".to_owned(), 1.0), ("b".to_owned(), 2.0)]);
        record(&path, [("b".to_owned(), 3.0)]);
        assert_eq!(
            load(&path),
            [("a".to_owned(), 1.0), ("b".to_owned(), 3.0)].into()
        );
        fs::write(&path, "not json").unwrap();
        assert!(load(&path).is_empty());
    }
}
