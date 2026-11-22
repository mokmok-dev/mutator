# mutator

Diff-driven mutation testing for Rust, built on [cargo-mutants](https://mutants.rs).

A pull request runs only the mutants its diff can affect. The rest of the project's mutants stay out of the run, so a check finishes in seconds instead of running a whole-codebase mutation sweep.

## Why

`cargo-mutants` finds untested code by changing it and rerunning the tests. On a large workspace a full run is slow enough that it cannot sit on every pull request. Most of that time is spent regrowing verdicts for code the pull request never touched.

`mutator` keeps `cargo-mutants` as the mutation engine and adds the part that makes it usable per change:

- **Diff selection.** A mutant is in scope when the function that encloses it overlaps a line the diff changed. A deleted line counts as touching the line it collapsed onto.
- **Exact selection.** The selected mutants reach `cargo-mutants` as one anchored regex, so a run tests those mutants and no neighbours. Verdicts come back from `mutants.out/outcomes.json` rather than from matching console text.
- **Stable identity.** A mutant's id is `file::function::genre::replacement#ordinal`. Line numbers are not part of it, so editing above a mutant does not invalidate its accepted-equivalent entry.
- **Accepted equivalents.** `mutator accept <id> --reason "…"` records a mutant no input can observe in `mutator.accepted.json`, which you commit. The entry leaves the score and the gate, and the report lists entries whose mutant is gone.
- **`--cache`.** A mutant's verdict is stored against a SHA-256 of every source and test file in its package plus the `cargo-mutants` and `mutator` versions. An unchanged package is decided from the store, so a CI job can hand its cache directory to the next run.
- **One gate.** `--fail-on-survived` and `--threshold` decide the exit code. Unrun mutants always fail: a run that could not decide a mutant has proved nothing.

Inspired by [mizchi/mutator](https://github.com/mizchi/mutator), which does the same job for TypeScript on top of Vitest, Jest, and Bun.

## Install

The repository builds with Nix. From a checkout:

```sh
nix develop --command cargo build --release
# target/release/mutator
```

## Use

```sh
# every mutant in the project
mutator

# only the mutants this branch's diff can affect
mutator --since main --fail-on-survived

# only the mutants a saved diff can affect
git diff -U0 main > change.diff
mutator --diff change.diff --fail-on-survived

# what would run, without running it
mutator --since main --list

# for CI: annotate survivors on the diff
mutator --since main --fail-on-survived --reporter github

# reuse verdicts for packages that have not changed
mutator --since main --cache --fail-on-survived
```

`--since <rev>` diffs against `git merge-base <rev> HEAD`, so a long-lived branch sees its own changes and not the base's.

Reporters are repeatable: `--reporter text`, `--reporter json` (one stable document), `--reporter github` (`::warning` annotations).

### Exit codes

| code | meaning |
|---|---|
| 0 | every mutant in scope was killed, accepted, unviable, or timed out |
| 1 | usage error, unreadable project, or an uninterpretable `cargo-mutants` failure |
| 2 | the gate failed: a survivor, an undecided mutant, or a score under `--threshold` |
| 4 | the tests fail without any mutant, so no verdict means anything |

## Accepted equivalents

```sh
mutator --since main --reporter json | jq '.rows[] | select(.status == "Missed")'
mutator accept 'src/parse.rs::number::BinaryOperator::+#0' \
  --reason "the operands are unsigned and bounded, so wrapping never changes the sum"
```

The id comes from the report. An accepted entry is reported as `ACCEPTED` and stays out of the score.

## The cache

`--cache` stores each verdict under `target/mutator-cache/<package>/<fingerprint>.json`. The fingerprint covers every `.rs` file under the package's `src/`, `tests/`, `benches/` and `examples/`, its `Cargo.toml`, the workspace `Cargo.toml` and `Cargo.lock`, plus the `cargo-mutants` and `mutator` versions.

That is deliberately the whole package: `cargo-mutants` rebuilds and reruns a package's tests for each mutant, so any file in the package can change a verdict. The cost of that choice is that editing one test re-runs every mutant of that package. The benefit is that reuse needs no coverage data and can be wrong in only one direction: it decides nothing the tests would not have decided.

A reused kill is reported as `KILLED` and counted exactly like a fresh one. A stored survivor still runs, since a survivor means "no test observed this mutant" and a new test can change that without touching the package's source. Nothing is read or written without the flag.

```sh
mutator --cache                 # 272 verdicts reused in 11s, after a 2m50s cold run on this crate
```

For CI, cache the directory and let it restore across runs:

```yaml
- uses: actions/cache@v4
  with:
    path: target/mutator-cache
    key: mutator-cache-${{ github.run_id }}
    restore-keys: mutator-cache-
```

## This repository's own gate

`cargo mutants` on this crate reports **0 missed**: 310 mutants, 272 caught, 38 unviable. The unviable ones do not compile, which is `cargo-mutants`' own verdict and outside the score. CI reruns that check with `--cache` on every push to `main` and every pull request, and the `diff` job runs `mutator --since --cache` on pull request diffs.

The parsers and selectors carry proptest properties alongside their example tests: diff text round-trips through the parser, line intersection agrees with per-line membership, ids survive a uniform line shift, and the accepted store round-trips for adversarial keys.

## Limits

- Selection follows function spans. A change to a function pulls in every mutant of that function, which is intentional but coarser than a line-level filter when a function is large.
- A diff that only edits a test file selects nothing, since no mutant moved. Rerun with `--all` after changing tests.
- The score covers mutants this run selected. It is a measurement of the change, not of the repository.
