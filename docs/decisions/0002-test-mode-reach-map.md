# 0002 `test` mode: run only the tests a change can reach

Status: accepted (2026-11-15)

## Context

After an edit, `cargo test -p pumpkin-core` runs all 714 lib tests (129 s with the opt-level fix),
although a change to one function can only affect the tests that execute it. Runner v2 of the
mutation mode (ADR 0001) showed that a per-test reach recorded by probes in the code is exact: on
the loot batch it picked 1 of 714 tests for every mutant, with the same verdict as the full suite.

## Decision

`kvasir-test test [-p PKG].. [-- cargo test args]` keeps a **reach map** (test -> functions it
executed) and, after an edit, runs only the tests whose reach contains a changed function.

- **Probes, not coverage.** Every function body of the tested packages gets a probe statement that,
  when `KVASIR_TEST_REACH` names a file, appends the function's id once per process. No compiler
  flags change, so dependencies are not rebuilt (`-C instrument-coverage` would rebuild every crate).
- **A mirror tree** under `target/kvasir-test/tree` holds the instrumented copy; it builds into the
  workspace's own `target/` (registry dependencies are shared, only workspace crates get their own
  artifacts). The user's sources are never edited, and files are only rewritten when their content
  changes, so mirror builds stay incremental.
- **Every run records.** The selected tests run in the instrumented build with recording on, so their
  reach is refreshed by the same run that gives the verdict; tests not selected executed no changed
  code, so their reach cannot have changed. The map never goes stale.
- **What counts as changed** (hashes of token streams, so comments and formatting do not count):
  - a function item (signature, attributes and body): select the tests that reached it;
  - anything else in an instrumented file (types, consts, `use`, macros, impl headers), any other file
    of the workspace (other crates, `Cargo.toml`, `Cargo.lock`, data and golden files), or the
    toolchain: run everything (and re-record everything).
- Tests that are new or failed last time always run. A change with no reaching test still builds, so
  compile errors are reported.

## Why this is sound

A deterministic test that executes no changed function body and depends on no changed non-function
input runs the same code on the same data, so its verdict cannot change. New functions are only
reachable through changed callers; renamed or removed ones change their callers too. Runtime inputs
outside Rust code (golden files, assets) are covered by the "any other file" rule.

## Consequences

- One extra set of workspace-crate artifacts in `target/` (the instrumented build).
- Non-deterministic tests (time, randomness without a seed, network) can hide a reach; the same is
  true of any test selection. `--full` runs and re-records everything.
- `const fn` bodies cannot hold a probe: they are hashed with the rest of the file (any change to one
  runs everything).
