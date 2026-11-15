# 0001 All mutants of a batch in one build (mutant schemata)

Status: accepted (2026-11-15)

## Context

cargo-mutants (the upstream of this fork) applies one mutant, rebuilds, runs the tests, reverts, and
repeats. On KvasirMC's engine (pumpkin-core, 284k lines in one crate) a mutant costs ~35 s of
rebuild and, before the opt-level fix, ~68 s of test: a batch of 88 mutants took ~2.5 h. The rebuild
is paid once per mutant although the mutants of a batch only differ in a few tokens.

## Decision

KvasirTest writes **one schema** per mutated file: every mutant is compiled in, behind a runtime
guard `KVASIR_TEST_MUTANT == <id>`. One build serves the whole batch; each mutant is then a run of
the already-built test executables with that variable set, several at once.

How each mutant genre becomes code (`src/schema.rs`):

| Genre | Site | Emitted |
|---|---|---|
| binary / unary operator, struct field deletion, match-guard replacement | the whole expression | `(if G(a) { variant a } else if G(b) { variant b } else { original })` |
| function body replacement | the function body | `if G(a) { return (value); }` before the original statements |
| match arm deletion | the arm's pattern | `pattern if !G(a) =>` (the arm is skipped, so the catch-all takes it) |

- Variant texts contain the *plain* original subexpressions (no nested guards): only one mutant is
  active per run, so when a variant branch runs, no inner mutant can be active. This keeps the schema
  linear in size instead of exponential in nesting depth.
- `G(id)` is self-contained (a block with its own `static OnceLock` reading the variable once), so no
  module has to be injected into the crate and nested `mod` blocks need no path.
- Mutants the schema cannot hold are found by compiling: each rustc error span is mapped back to the
  variant or guard it falls in, those mutants are removed and the schema is rebuilt until it
  compiles. Removed mutants run afterwards in the classic per-mutant mode, so nothing is lost
  (const contexts, `impl Trait` returns, inference that changes under `if`).

## Options considered

1. **Keep upstream's rebuild per mutant**: correct, simple, slow (build dominates after the opt fix).
2. **mutest-rs** (compile-once in the compiler): needs a pinned nightly; young research tool;
   different mutation set, so past ratchets would not compare.
3. **Fork cargo-mutants with schemata (chosen)**: same mutant set and output format as upstream, so
   results compare one to one with earlier runs; stable compiler; classic mode stays as fallback.

## Consequences

- Results must equal the classic mode per mutant. Acceptance check: the KvasirMC loot batch (88
  mutants) gives the same caught / missed / unviable per mutant as the classic run.
- Tests of one batch run in parallel: tests that share fixed ports or files must already be safe under
  `cargo test` (libtest runs tests in parallel threads), so this adds no new requirement in practice.
- Doc tests are not run by the schema runner (they are built by rustdoc per run); use the classic
  mode for packages whose only tests are doc tests.
- Upstream merges touch `visit.rs` (sites recorded beside each mutant) and `lab.rs` (mode switch);
  the schema code lives in its own modules to keep those merges small.
