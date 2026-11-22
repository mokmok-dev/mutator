# mutator

Diff-driven mutation testing for Rust, built on [cargo-mutants](https://mutants.rs).

A pull request runs only the mutants its diff can affect, and fails when one survives. The rest of the project's mutants stay out of the run, so a check finishes in seconds instead of running a whole-codebase mutation sweep.

## Why

`cargo-mutants` finds untested code by changing it and rerunning the tests. On a large workspace a full run is slow enough that it cannot sit on every pull request. Most of that time is spent regrowing verdicts for code the pull request never touched.

`mutator` keeps `cargo-mutants` as the mutation engine and adds the parts that make it usable per change:

- **Diff selection.** A mutant is in scope when the function that encloses it overlaps a line the diff changed. A deleted line counts as touching the line it collapsed onto.
- **Exact selection.** The selected mutants reach `cargo-mutants` as one anchored regex, so a run tests those mutants and no neighbours. Verdicts come back from `mutants.out/outcomes.json` rather than from matching console text.
- **Stable identity.** A mutant's id is `file::function::genre::replacement#ordinal`. Line numbers are not part of it, so editing above a mutant does not invalidate its accepted-equivalent entry.
- **Accepted equivalents.** `mutator accept <id> --reason "…"` records a mutant no input can observe in `mutator.accepted.json`, which you commit. The entry leaves the score and the gate, and the report lists entries whose mutant is gone.
- **A content-addressed cache.** `--cache` stores a verdict against a SHA-256 of everything that decides it, so an unchanged package costs a hash instead of a test run.
- **One gate.** `--fail-on-survived` and `--threshold` decide the exit code. Unrun mutants always fail: a run that could not decide a mutant has proved nothing.

Inspired by [mizchi/mutator](https://github.com/mizchi/mutator), which does the same job for TypeScript on top of Vitest, Jest, and Bun.

## Install

```sh
nix run github:mokmok-dev/mutator -- --since main
```

As a flake input:

```nix
{
  inputs.mutator.url = "github:mokmok-dev/mutator";
  inputs.mutator.inputs.nixpkgs.follows = "nixpkgs";
}
```

```nix
# packages.${system}.default, or in a devShell
packages = [ inputs.mutator.packages.${system}.default ];
```

From a checkout, `nix build` and `nix run . -- --version` do the same. `nix develop` gives you the toolchain, `cargo-mutants`, and `cargo-nextest`.

Supported systems are `x86_64-linux`, `aarch64-linux`, and `aarch64-darwin`. Nixpkgs 26.11 dropped `x86_64-darwin`, so Intel Macs are not covered.

`mutator` calls the `cargo` and `cargo-mutants` on your `PATH` rather than bundling them, so the version in your devShell is the one that runs.

### Agent skills

The repository ships skills under [`skills/`](skills/), so an agent can close the gaps this tool finds:

```sh
npx skills add mokmok-dev/mutator
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

`--since <rev>` diffs against `git merge-base <rev> HEAD`, so a long-lived branch sees its own changes and not the base's. `--all` forces a full sweep when a selector is also present.

`--jobs` and `--timeout` pass straight through to `cargo-mutants` as `-j` and `-t`. `--manifest-path` selects a workspace member. `--verbose` echoes each command on stderr, keeping stdout parseable.

### Options

| option | effect |
|---|---|
| `--root <DIR>` | project root, default `.` |
| `--since <REV>` / `--diff <FILE>` / `--all` | which mutants to consider; mutually exclusive |
| `--fail-on-survived` | exit 2 when any mutant survived |
| `--threshold <PERCENT>` | exit 2 below this score; `85`, `85.5`, and `85.50%` all parse |
| `--reporter text\|json\|github` | repeatable |
| `--cache` | reuse verdicts for unchanged packages |
| `--list` | print the selected names, run nothing |
| `--jobs` / `--timeout` / `--manifest-path` | passed to `cargo-mutants` |
| `--verbose` | echo each command on stderr |

### Reports

The text report names the scope, one row per selected mutant, the score, and a cache line when `--cache` reused anything:

```
mutator: 2 of 309 mutants in scope (diff since main)
  src/diff.rs:88  parse  KILLED
  src/diff.rs:140  render  SURVIVED
score: 50.00% (1 detected, 1 missed, 0 accepted, 0 unviable, 0 not run)
cache: 1 killed verdict reused, 0 survivors decided again
```

`--reporter json` emits one document with a stable field order:

```json
{"schemaVersion":1,"scope":"diff since main",
 "counts":{"caught":1,"missed":1,"timeout":0,"unviable":0,"accepted":0,"not_run":0},
 "detected":1,"decided":2,"scoreBp":5000,
 "rows":[{"id":"src/lib.rs::parse::FnValue::0#0","name":"…","file":"src/lib.rs",
          "line":3,"function":"parse","status":"Caught"}],
 "staleAccepted":[],"reused":{"caught":0,"missed":0}}
```

`status` is one of `Caught`, `Missed`, `Timeout`, `Unviable`, `Accepted`, `NotRun`. `--reporter github` prints `::warning` annotations for survivors and `::notice` for stale accepted entries.

### Exit codes

| code | meaning |
|---|---|
| 0 | every mutant in scope was killed, accepted, unviable, or timed out |
| 1 | usage error, unreadable project, or an uninterpretable `cargo-mutants` failure |
| 2 | the gate failed: a survivor, an undecided mutant, or a score under `--threshold` |
| 4 | the tests fail without any mutant, so no verdict means anything |

## Accepted equivalents

```sh
mutator --since main --reporter json | jq -r '.rows[] | select(.status=="Missed") | .id'
mutator accept 'src/parse.rs::number::BinaryOperator::+#0' \
  --reason "the operands are unsigned and bounded, so wrapping never changes the sum"
```

The id comes from the report. An accepted entry is reported as `ACCEPTED`, stays out of the score, and leaves `--fail-on-survived` alone. `staleAccepted` lists entries whose mutant no longer exists, so a refactor that deletes the code does not leave dead entries behind.

## The cache

`--cache` stores each verdict under `target/mutator-cache/<package>/<fingerprint>.json`. The fingerprint covers every `.rs` file under the package's `src/`, `tests/`, `benches/` and `examples/`, its `Cargo.toml`, the workspace `Cargo.toml` and `Cargo.lock`, plus the `cargo-mutants` and `mutator` versions.

That is deliberately the whole package: `cargo-mutants` rebuilds and reruns a package's tests for each mutant, so any file in the package can change a verdict. The cost is that editing one test re-runs every mutant of that package. The benefit is that reuse needs no coverage data and can be wrong in only one direction: it decides nothing the tests would not have decided.

A reused kill is reported as `KILLED` and counted exactly like a fresh one. A stored survivor still runs, since a survivor means "no test observed this mutant" and a new test can change that without touching the package's source. Nothing is read or written without the flag.

Measured on this crate: 2m48s cold, 10.8s warm, 270 of 270 verdicts reused, identical score.

For CI, cache the directory and let it restore across runs:

```yaml
- uses: actions/cache@v4
  with:
    path: target/mutator-cache
    key: mutator-cache-${{ github.run_id }}
    restore-keys: mutator-cache-
```

Do not let a run that saves the cache get cancelled by a newer push. In this repository `cancel-in-progress` is `true` only for pull requests, because a push run is the one that writes the verdicts.

## CI

A pull request job that runs the diff and annotates survivors:

```yaml
- uses: actions/checkout@v4
  with:
    fetch-depth: 0            # --since needs the base commit
- run: nix develop --command cargo build
- run: |
    nix develop --command ./target/debug/mutator \
      --since "origin/${{ github.base_ref }}" \
      --cache --fail-on-survived --reporter github
```

`fetch-depth: 0` matters: `--since origin/main` resolves the base commit locally.

## This repository's own gate

`cargo mutants` on this crate reports **0 missed**: 309 mutants, 270 caught, 39 unviable. Unviable means the mutant did not compile, which is `cargo-mutants`' own verdict and outside the score.

The gate runs on every push to `main` and every pull request, and the `diff` job runs `mutator --since --cache` on pull request diffs. The parsers and selectors carry proptest properties alongside their example tests: diff text round-trips through the parser, line intersection agrees with per-line membership, ids survive a uniform line shift, verdicts refuse a mismatched tool version, and the accepted store round-trips for adversarial keys.

## Limits

- Selection follows function spans. A change to a function pulls in every mutant of that function, which is intentional but coarser than a line-level filter when a function is large.
- A diff that only edits a test file selects nothing, since no mutant moved. Rerun with `--all` after changing tests.
- The score covers mutants this run selected. It is a measurement of the change, not of the repository.
- Reuse is per package. A workspace with one large crate gets less from `--cache` than one split into small crates.
