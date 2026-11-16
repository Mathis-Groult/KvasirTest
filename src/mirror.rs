// Copyright 2026 Mathis Groult
// KvasirTest: see docs/decisions/0002-test-mode-reach-map.md.

//! The mirror tree of `test` mode: an instrumented copy of the workspace that builds into the
//! workspace's own target directory. A file is only rewritten when its content changes, so the
//! mirror's builds stay incremental and the user's sources are never touched.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;

use anyhow::Context as _;
use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::Result;
use crate::probe::{analyse, fnv64, instrument};
use crate::reach_map::FileEntry;

/// The result of one sync.
#[derive(Debug, Default)]
pub struct Synced {
    /// Instrumented files by tree-relative path.
    pub files: BTreeMap<String, FileEntry>,
    /// Hash of every other file (paths and contents).
    pub other_hash: u64,
    /// Function key of each probe id.
    pub probes: BTreeMap<u64, String>,
    /// How many mirror files were (re)written.
    pub written: usize,
    /// How many source files were read (the others were unchanged since the last sync).
    pub read: usize,
}

/// What the last sync learned about each source file, so unchanged files are not read again.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SyncCache {
    files: BTreeMap<String, Cached>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Cached {
    size: u64,
    modified: (u64, u32),
    /// Length of the file written to the mirror.
    mirror_len: u64,
    kind: CachedKind,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum CachedKind {
    Copied { hash: u64 },
    Probed { entry: FileEntry },
}

/// The probe id of a function: a hash of its key, so ids do not move when functions are added
/// elsewhere (moving ids would rewrite every later file and rebuild unrelated crates).
pub fn probe_id(key: &str) -> u64 {
    fnv64(key.as_bytes())
}

/// Copy `src` to `mirror`, instrumenting the `.rs` files under `instrumented` (tree-relative
/// package directories). `skip` lists tree-relative directories not to copy (the target dir).
/// `.git` is linked, not copied (build scripts read it). Files whose size and modification time
/// match `cache` are not read again.
pub fn sync(
    src: &Utf8Path,
    mirror: &Utf8Path,
    instrumented: &[Utf8PathBuf],
    skip: &[Utf8PathBuf],
    cache: &mut SyncCache,
) -> Result<Synced> {
    let mut paths = Vec::new();
    walk(src, Utf8Path::new(""), skip, &mut paths)?;
    paths.sort();
    let mut synced = Synced::default();
    let mut other: Vec<(String, u64)> = Vec::new();
    let mut next_cache = BTreeMap::new();
    for rel in &paths {
        let key = rel.to_string();
        let from = src.join(rel);
        let to = mirror.join(rel);
        let meta = fs::metadata(&from).with_context(|| format!("stat {from}"))?;
        let modified = meta
            .modified()?
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| (d.as_secs(), d.subsec_nanos()))
            .unwrap_or_default();
        let probed = rel.extension() == Some("rs")
            && instrumented
                .iter()
                .any(|dir| dir.as_str().is_empty() || rel.starts_with(dir));
        let unchanged = cache.files.get(&key).filter(|c| {
            c.size == meta.len()
                && c.modified == modified
                && matches!(c.kind, CachedKind::Probed { .. }) == probed
                && fs::metadata(&to).is_ok_and(|m| m.len() == c.mirror_len)
        });
        let cached = if let Some(c) = unchanged {
            c.clone()
        } else {
            synced.read += 1;
            let bytes = fs::read(&from).with_context(|| format!("read {from}"))?;
            let (content, kind) = convert(rel, bytes, probed);
            if fs::read(&to).ok().as_deref() != Some(content.as_slice()) {
                if let Some(parent) = to.parent() {
                    fs::create_dir_all(parent).with_context(|| format!("create {parent}"))?;
                }
                fs::write(&to, &content).with_context(|| format!("write {to}"))?;
                fs::set_permissions(&to, meta.permissions()).ok();
                synced.written += 1;
            }
            Cached {
                size: meta.len(),
                modified,
                mirror_len: content.len() as u64,
                kind,
            }
        };
        match &cached.kind {
            CachedKind::Copied { hash } => other.push((key.clone(), *hash)),
            CachedKind::Probed { entry } => {
                synced
                    .probes
                    .extend(entry.fns.keys().map(|k| (probe_id(k), k.clone())));
                synced.files.insert(key.clone(), entry.clone());
            }
        }
        next_cache.insert(key, cached);
    }
    cache.files = next_cache;
    remove_stale(mirror, &paths.iter().collect())?;
    link_git(src, mirror)?;
    let mut listing = String::new();
    for (path, hash) in &other {
        listing.push_str(path);
        listing.push('\0');
        listing.push_str(&hash.to_string());
        listing.push('\n');
    }
    synced.other_hash = fnv64(listing.as_bytes());
    Ok(synced)
}

/// The mirror content of a source file and what to remember about it.
fn convert(rel: &Utf8Path, bytes: Vec<u8>, probed: bool) -> (Vec<u8>, CachedKind) {
    if probed && let Ok(code) = std::str::from_utf8(&bytes) {
        match analyse(rel.as_str(), code) {
            Ok(file) => {
                let ids: Vec<u64> = file.fns.iter().map(|f| probe_id(&f.key)).collect();
                let entry = FileEntry {
                    outside_hash: file.outside_hash,
                    fns: file.fns.iter().map(|f| (f.key.clone(), f.hash)).collect(),
                };
                return (
                    instrument(code, &file.fns, &ids).into_bytes(),
                    CachedKind::Probed { entry },
                );
            }
            Err(err) => debug!(%rel, %err, "not parsable: copied as is"),
        }
    }
    let hash = fnv64(&bytes);
    (bytes, CachedKind::Copied { hash })
}

/// Make `mirror/.git` point at the source's `.git`, so build scripts that read it (or tell cargo
/// to watch `.git/HEAD`) see the real repository.
fn link_git(src: &Utf8Path, mirror: &Utf8Path) -> Result<()> {
    let real = src.join(".git");
    let link = mirror.join(".git");
    if real.exists() && fs::read_link(&link).ok().as_deref() != Some(real.as_std_path()) {
        fs::remove_file(&link).ok();
        std::os::unix::fs::symlink(&real, &link).with_context(|| format!("link {link}"))?;
    }
    Ok(())
}

fn walk(
    root: &Utf8Path,
    rel: &Utf8Path,
    skip: &[Utf8PathBuf],
    out: &mut Vec<Utf8PathBuf>,
) -> Result<()> {
    let dir = root.join(rel);
    for entry in fs::read_dir(&dir).with_context(|| format!("list {dir}"))? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            warn!(?name, "skipping a non-UTF-8 file name");
            continue;
        };
        let child = rel.join(name);
        let kind = entry.file_type()?;
        if kind.is_dir() {
            if name == ".git" || skip.contains(&child) {
                continue;
            }
            walk(root, &child, skip, out)?;
        } else if kind.is_file() || (kind.is_symlink() && root.join(&child).is_file()) {
            out.push(child);
        }
    }
    Ok(())
}

/// Remove mirror files that no longer exist in the source tree.
fn remove_stale(mirror: &Utf8Path, keep: &BTreeSet<&Utf8PathBuf>) -> Result<()> {
    if !mirror.exists() {
        return Ok(());
    }
    let mut present = Vec::new();
    walk(mirror, Utf8Path::new(""), &[], &mut present)?;
    for rel in present.iter().filter(|p| !keep.contains(p)) {
        fs::remove_file(mirror.join(rel)).ok();
    }
    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;

    fn tree(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (path, content) in files {
            let p = dir.path().join(path);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, content).unwrap();
        }
        dir
    }

    fn utf8(p: &std::path::Path) -> &Utf8Path {
        Utf8Path::from_path(p).unwrap()
    }

    #[test]
    fn instruments_package_sources_copies_the_rest_and_only_rewrites_changes() {
        let src = tree(&[
            ("Cargo.toml", "[workspace]"),
            ("a/src/lib.rs", "pub fn f() {}\n"),
            ("b/src/lib.rs", "pub fn g() {}\n"),
            ("target/debug/x", "built"),
        ]);
        let mirror = tempfile::tempdir().unwrap();
        let (src_p, mirror_p) = (utf8(src.path()), utf8(mirror.path()));
        let skip = [Utf8PathBuf::from("target")];
        let mut cache = SyncCache::default();
        let first = sync(src_p, mirror_p, &["a".into()], &skip, &mut cache).unwrap();
        assert_eq!(
            first.probes.values().collect::<Vec<_>>(),
            ["a/src/lib.rs::f"]
        );
        assert_eq!(first.written, 3);
        let a = fs::read_to_string(mirror_p.join("a/src/lib.rs")).unwrap();
        assert!(a.contains("KVASIR_TEST_PROBE"));
        assert_eq!(
            fs::read_to_string(mirror_p.join("b/src/lib.rs")).unwrap(),
            "pub fn g() {}\n"
        );
        assert!(!mirror_p.join("target").exists());

        let again = sync(src_p, mirror_p, &["a".into()], &skip, &mut cache).unwrap();
        assert_eq!(again.written, 0);
        assert_eq!(again.read, 0);
        assert_eq!(again.files, first.files);
        assert_eq!(again.other_hash, first.other_hash);

        fs::write(src_p.join("b/src/lib.rs"), "pub fn g() { 1; }\n").unwrap();
        fs::remove_file(src_p.join("Cargo.toml")).unwrap();
        let changed = sync(src_p, mirror_p, &["a".into()], &skip, &mut cache).unwrap();
        assert_eq!(changed.written, 1);
        assert_eq!(changed.read, 1);
        assert_ne!(changed.other_hash, first.other_hash);
        assert!(!mirror_p.join("Cargo.toml").exists());
    }
}
