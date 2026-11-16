# KvasirTest

Fast Rust testing, in two modes:

- **`kvasir-test mutants`**: mutation testing (do your tests catch bugs?) built on
  [cargo-mutants](https://github.com/sourcefrog/cargo-mutants) by Martin Pool, with **mutant
  schemata**: every mutant of a run is compiled into one build behind a runtime switch, and each
  mutant runs only on the tests that actually execute it.
- **`kvasir-test test`**: after an edit, build once and run only the tests that can reach the
  changed functions, in parallel.

Both modes rest on one idea: probes compiled into your code record which tests execute which
functions (or mutants). A test that never executes a piece of code cannot observe a change in it,
so the selection is exact, not a heuristic.

## Measured

On a 284,000-line crate with 714 tests (the KvasirMC Minecraft server engine, debug builds, a
6-core VPS):

| | Plain tools | KvasirTest |
|---|---|---|
| 88 mutants of one file, mutation testing | 10,001 s (cargo-mutants, one build per mutant) | **381 s**, the same verdict on every mutant |
| Tests after editing one function | 124 s (all 714 tests) | **7 s** (1 test, the one that reaches it) |
| Run with nothing changed | 124 s | **5 s** |

The 10,001 s run also had slower test builds (the crate's opt-level was raised in between; with
that change alone the plain run would take an estimated 60-80 min). On cargo-mutants' own test
crates the schema mode gives the same verdict (caught / missed / unviable / timeout) on every
mutant of 29 crates; it is up to 12x faster on crates with tens of mutants, and about even (or
slower: one crate took 9.9 s instead of 7.3 s) on crates with a handful of mutants, where
recording each test's reach costs more than it saves.

## Install

```sh
cargo install --locked --git https://github.com/Mathis-Groult/KvasirTest
```

Linux (and other Unix) only: the runners use process groups and symlinks.

## `kvasir-test mutants`

```sh
kvasir-test mutants -p my-crate --file src/parser.rs -- --lib
```

Options, filters and output (`mutants.out/`) are those of cargo-mutants 27.1; see its
[manual](https://mutants.rs/). What changes:

- All mutants of the run are written into a **schema** of each mutated file, compiled once.
  Mutants a runtime switch cannot hold (const contexts, let-chains, proc-macro and
  build-dependency crates, type errors) are found up front or by the compiler and run the
  classic way, one build each, so every mutant still gets a verdict.
- The baseline runs every test in its own process and records which mutants each test reaches.
  A mutant runs only its reaching tests, fastest first, and stops at the first failure; a mutant
  no test reaches is reported missed without running.
- Mutants run in parallel: CPUs minus 2 by default (`--schema-jobs N`).
- `--classic` (or `KVASIR_TEST_CLASSIC=1`) gives cargo-mutants' one-build-per-mutant mode.
- `--custom-mutants spec.toml` adds hand-written mutants to the same build, with the same reach
  selection and verdicts. Each is a textual edit; `old` must occur exactly once in the file:

  ```toml
  [[mutant]]
  name = "swap_xy"                  # unique; shown in results as `custom swap_xy: ...`
  file = "src/display.rs"           # relative to the workspace root
  old = "Vec3(t.x, t.y, t.z)"
  new = "Vec3(t.y, t.x, t.z)"
  ```

  The switch goes around the smallest enclosing expression or statement list, so the schema runs
  exactly the edited code. Custom mutants are not subject to `--file`/`--re`/`--exclude`.
- `--extra-mutants KIND,..` adds mutation kinds cargo-mutants does not generate (off by default,
  so existing counts stay put): `delete-call` (delete a call statement), `literal` (integers
  ±1, floats to 0.0, bools flipped, strings emptied), `drop-operand` (`a && b` -> `a`, -> `b`),
  `swap-args` (adjacent call arguments of the same shape: `v(t.x, t.y)` -> `v(t.y, t.x)`),
  `enum-variant` (`Shape::Tall` -> another `Shape::` variant the same file uses), or `all`.
  They are generated only where a runtime switch fits. A swap the compiler rejects inside its
  own variant is reported unviable at once, without a build of its own.

Design: [docs/decisions/0001-mutant-schemata.md](docs/decisions/0001-mutant-schemata.md).

## `kvasir-test test`

```sh
kvasir-test test -p my-crate -- --lib
```

- The first run builds an instrumented copy of the workspace (`target/kvasir-test/tree`, built
  into your own `target/`), runs every test once and records which functions each one executes.
- Later runs compare function hashes (comments and formatting do not count) and run only the
  tests that reached a changed function, plus new tests and those that failed last time. Every
  run re-records what it ran, so the record stays exact.
- Any change outside function bodies (types, constants, manifests, data or golden files, the
  toolchain) runs everything. `--full` forces it; `--plan` shows what would run.

Your source tree is never edited. Design:
[docs/decisions/0002-test-mode-reach-map.md](docs/decisions/0002-test-mode-reach-map.md).

## Limits

- A test whose behaviour depends on time, unseeded randomness or the network can hide what it
  reaches; that is true of any test selection.
- Doc tests run as one unit per package.
- The first `test` run and the first build of a schema cost a full build.

## Licence

KvasirTest is free software under the **GNU GPL, version 3 or later** ([LICENSE](LICENSE)).
It is a fork of cargo-mutants 27.1.0, copyright Martin Pool, used under the MIT licence
([LICENSE-MIT](LICENSE-MIT)); that notice applies to the code taken from cargo-mutants. See
[NOTICE](NOTICE). cargo-mutants' own README is kept in
[docs/upstream-README.md](docs/upstream-README.md).
