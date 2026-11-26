# mutator

Diff-driven mutation testing for Rust, built on [cargo-mutants](https://mutants.rs).

A pull request runs only the mutants its diff can affect, and fails when one survives. The rest of the project's mutants stay out of the run, so a check finishes in seconds instead of running a whole-codebase mutation sweep.

## Why

`cargo-mutants` finds untested code by changing it and rerunning the tests. On a large workspace a full run is slow enough that it cannot sit on every pull request. Most of that time is spent regrowing verdicts for code the pull request never touched.

`mutator` keeps `cargo-mutants` as the mutation engine and adds the parts that make it usable per change:

- **Diff selection.** A mutant is in scope when the function that encloses it overlaps a line the diff changed. A deleted line counts as touching the line it collapsed onto.
- **Anything a function span cannot represent widens the scope.** A changed test, build script or manifest selects every mutant of the package that owns it, and so does a source change no function span covers: a line outside every mutant's function, a signature, type, trait, constant, macro, import, or attribute. So does a deletion or a rename. A changed path no package owns, such as the workspace `Cargo.lock` or `.cargo/config.toml`, selects the whole workspace, because nothing proves which mutants it can observe. `--list` prints the reason on stderr.
- **Selective is not exhaustive.** A run proves the mutants its change can reach. It is not a full-workspace sweep, and its score describes the selection.
- **Exact selection.** The selected mutants reach `cargo-mutants` as one anchored regex, so a run tests those mutants and no neighbours. Verdicts come back from `mutants.out/outcomes.json` rather than from matching console text.
- **Stable identity.** A mutant's id is `file::function::genre::replacement#ordinal`. Line numbers are not part of it, so editing above a mutant does not invalidate its accepted-equivalent entry.
- **Accepted equivalents.** `mutator accept <id> --reason "…"` records a mutant no input can observe in `mutator.accepted.json`, which you commit. The entry leaves the score and the gate, and the report lists entries whose mutant is gone.
- **A content-addressed cache.** `--cache` stores a verdict against a SHA-256 of everything that decides it, including the local packages it depends on, so an unchanged closure costs a hash instead of a test run.
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

From a checkout, `nix build` and `nix run . -- --version` do the same. The published flake depends on nixpkgs alone, so it carries the package and the module and nothing else. Development tooling lives in [`dev/`](dev/flake.nix): `nix develop ./dev` gives you the toolchain, `cargo-mutants`, and `cargo-nextest`.

Supported systems are `x86_64-linux`, `aarch64-linux`, and `aarch64-darwin`. Nixpkgs 26.11 dropped `x86_64-darwin`, so Intel Macs are not covered.

### As a flake-parts module

`flakeModules.default` configures a whole project from `perSystem`. Import it and set
`perSystem.mutator`:

```nix
{
  inputs.mutator.url = "github:mokmok-dev/mutator";
  inputs.mutator.inputs.nixpkgs.follows = "nixpkgs";

  outputs = inputs: inputs.flake-parts.lib.mkFlake { inherit inputs; } {
    systems = [ "x86_64-linux" "aarch64-darwin" ];
    imports = [ inputs.mutator.flakeModules.default ];
    perSystem.mutator = {
      enable = true;
      check = true;
    };
  };
}
```

| option | default | effect |
|---|---|---|
| `mutator.enable` | `false` | adds `packages.mutator`, `packages.default`, and `apps.mutator` |
| `mutator.check` | `true` | adds `checks.mutator`, so `nix flake check` runs the mutation gate |
| `mutator.source` | `self` | the project to test; its `Cargo.lock` must be committed |
| `mutator.args` | `[ "--all" "--fail-on-survived" ]` | arguments for the check |
| `mutator.jobs` | `4` | `--jobs` for the check |
| `mutator.package` | the input's binary | override the binary |

With this, `nix flake check` runs the same gate as CI, fully offline and reproducible. The
check copies the source, so `cargo-mutants` can mutate it, and it runs `--all`: a sandbox
has no `git` history to diff against.

The tradeoff is cost. `checks.mutator` runs every mutant with a cold cache, so it takes
minutes on a large crate. Set `check = false` and keep the `--cache` CI job when you want
the gate to stay inside a few seconds.

`mutator` calls the `cargo` and `cargo-mutants` on your `PATH` rather than bundling them, so the version in your devShell is the one that runs.

### Development

`nix develop ./dev` gives you the toolchain, `cargo-mutants`, `cargo-nextest`, and the linters. `nix flake check ./dev` runs every check, one per linter, so a failure names the tool that failed. The formatter lives there too, and `cd dev && nix fmt` formats the whole repository.

The dev shell uses nixpkgs' rustc, the same one `nix build` compiles the package with, so the shell and the published binary are the same compiler. `dev/flake.nix` follows the root flake's nixpkgs, and `checks.nixpkgs-aligned` fails when the two lock files disagree. After bumping the root, run `nix flake update --flake ./dev`.

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

# for CI: annotate survivors and write the job summary
mutator --since main --fail-on-survived --reporter github --summary "$GITHUB_STEP_SUMMARY"

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
| `--summary <FILE>` | write the GitHub Actions job summary as Markdown |
| `--list` | print the selected names, and the selection reason on stderr when it widened |
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

A diff that changed something a function span cannot carry prints the reason the scope is wider than the functions it touched:

```
mutator: 270 of 309 mutants in scope (diff since main)
selection: 1 test file changed, so every mutant of mutator is in scope
  ...
```

The text report never claims more than the selection proves. A score from a diff-scoped run measures the change, not the repository.

`--reporter json` emits one document with a stable field order:

```json
{"schemaVersion":1,"scope":"diff since main",
 "selection":{"reason":"Packages","packages":["mutator"],"files":["tests/cli.rs"]},
 "counts":{"caught":1,"missed":1,"timeout":0,"unviable":0,"accepted":0,"not_run":0},
 "detected":1,"decided":2,"scoreBp":5000,
 "rows":[{"id":"src/lib.rs::parse::FnValue::0#0","name":"…","file":"src/lib.rs",
          "line":3,"function":"parse","status":"Caught"}],
 "staleAccepted":[],"reused":{"caught":0,"missed":0},
 "totalMutants":309,"scoreScope":"Selected"}
```

`selection.reason` is `All`, `Touched`, `Packages`, or `Workspace`. `status` is one of `Caught`, `Missed`, `Timeout`, `Unviable`, `Accepted`, `NotRun`. `--reporter github` prints `::warning` annotations for survivors and `::notice` for stale accepted entries.

`totalMutants` is every mutant in the project and `scoreScope` says which set `scoreBp` describes: `Repository` when the run selected them all, `Selected` otherwise. A diff's score is never labelled as the project's. New fields are appended and existing ones keep their name, type, and order, so `schemaVersion` stays `1` and a consumer written against it keeps reading the same values.

`--summary <FILE>` writes the job summary GitHub renders on a pull request: the gate result and its reason, the counts, the score with the set it covers, the cache hits and misses, the elapsed time, and each survivor. Write it to `$GITHUB_STEP_SUMMARY` in a workflow step.

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

`--cache` stores each verdict under `target/mutator-cache/<package>/<fingerprint>.json`. The fingerprint covers every file a package's tests can observe: everything under `src/`, `tests/`, `benches/` and `examples/` of the package and of every local package it depends on, their `Cargo.toml` and `build.rs`, the workspace `Cargo.toml`, `Cargo.lock`, `.cargo/mutants.toml`, `.cargo/config`, `.cargo/config.toml`, `.config/nextest.toml`, `rust-toolchain`, and `rust-toolchain.toml`, plus the `cargo-mutants` and `mutator` versions. Files are hashed by their path relative to the project root, so moving the checkout does not invalidate the cache.

The local dependency closure is the point. `cargo-mutants` runs `cargo test --package=<pkg>`, which builds and links every path dependency of that package, so a change to a dependency's implementation can change the dependent package's verdicts while its own files stay byte-for-byte identical. A registry dependency is not local: its contents cannot change without `Cargo.lock` changing, which the fingerprint already covers.

That is deliberately the whole closure: `cargo-mutants` rebuilds and reruns a package's tests for each mutant, so any file in it can change a verdict. The cost is that editing one test re-runs every mutant of that package and its dependents. The benefit is that reuse needs no coverage data and can be wrong in only one direction: it decides nothing the tests would not have decided.

### Why the key stops at the package

A finer key was prototyped and rejected on measurement, not on taste. `bench/cache-granularity.sh` warms the cache on a three-crate workspace, then edits one function, one test, one module with no mutants, one dependency, and one dependency's test file, each from the same base. Every run is compared against an uncached run of the same tree, and the script fails if the verdicts differ.

| change | reused | verdicts flipped in files the edit never touched |
|---|---|---|
| one function in the root package | 13 of 34 | 0 |
| one test file of the root package | 13 of 34 | 3, all in `app/src/lib.rs` |
| a module of the root package with no mutants | 13 of 36 | 3, all in `app/src/lib.rs` |
| one function of a dependency | 0 of 35 | 3, all in `app/src/lib.rs` |
| one test file of a dependency | 7 of 34 | 3, all in `util/src/lib.rs` |

The last four rows are the reason nothing finer ships. A key built from a mutant and its own file would have reused every stale verdict in them, because a verdict depends on every value the tests compute, and those values live in other files. Two edges make it worse than a coverage tool would suggest. A `#[test]` lives in the package's own `tests/`, so no test-level key can separate "the mutant changed" from "the assertion changed". And a dependency's `tests/` file is still reachable as data: a library may `include!` it, which flips a dependent's verdict with no source file of that dependent changing.

The one-function row is where a finer key would pay, and the fixture cannot prove it would be sound. The edit flipped no verdict there, but that is one change to one function. Nothing in a function edit stops it from flipping a verdict for a mutant in a different function: a test that calls both functions computes its assertion from both, so editing one moves the other's verdict. A per-function key would need coverage evidence that no test observes the edited function together with a mutant's function, and where that evidence is missing it would fall back to the package anyway. That evidence is the coverage data this cache is designed to avoid.

What the prototype did establish is the shape of the trade. The cheapest sound key is the package, and the warm-run floor on this crate sits in `cargo-mutants`' unviable recompiles, not in fingerprinting.

A reused kill is reported as `KILLED` and counted exactly like a fresh one. A stored survivor still runs, since a survivor means "no test observed this mutant" and a new test can change that without touching the package's source. Nothing is read or written without the flag.

When a package's inputs cannot be modelled, the run decides it again instead of reusing a verdict, and says so on stderr. That covers a package whose dependencies `cargo metadata` does not resolve. A changed test or manifest that no package owns, such as a workspace-level `tests/` directory or `Cargo.lock`, turns reuse off for the whole run, since it cannot tell which stores the file could change.

`.cargo/mutants.toml` can widen a run in ways a fingerprint cannot follow. `test_workspace = true` makes every mutant test the whole workspace, so the key being present puts every local package in every closure. Other keys still apply, and `additional_cargo_args` or `additional_cargo_test_args` naming files outside the closure are not fingerprinted.

Some inputs cannot be fingerprinted at all, so a verdict is never claimed to be fresh against them: the installed compiler and standard library, when no `rust-toolchain.toml` pins them; environment variables a test or build script reads; files a test or build script opens by an absolute path or from outside the closure; and anything outside the tree. The cache is a speedup, not a substitute for a cold run on a new toolchain.

Measured on this crate: 2m48s cold, 10.8s warm, 270 of 270 verdicts reused, identical score. The warm run's floor is not fingerprinting. The three subprocess calls a warm run makes to find its work (`cargo mutants --list --json`, `cargo metadata`, `cargo mutants --version`) take 0.16s together, and hashing the closure is smaller still. A warm run takes 17.6s here, and 17.7s of a separate measurement is `cargo-mutants` rebuilding the 54 unviable mutants, which produce no compilable mutation to test but are re-emitted by every run and stored by none. A finer cache key would not touch that floor.

The counts behind that, from `mutator --all --cache` on this crate: 398 mutants, 344 verdicts reused, 54 unviable. Timing `cargo mutants` over exactly those 54 names alone takes 17.7s, so the warm run is that recompile plus about 0.2s of bookkeeping.

The benchmark is `bench/cache-granularity.sh`, and it fails if a cached run ever disagrees with an uncached run of the same tree.

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

A pull request job that runs the diff, annotates survivors, and writes the step summary:

```yaml
- uses: actions/checkout@v4
  with:
    fetch-depth: 0            # --since needs the base commit
- run: nix develop ./dev --command cargo build
- run: |
    nix develop ./dev --command ./target/debug/mutator \
      --since "origin/${{ github.base_ref }}" \
      --cache --fail-on-survived \
      --reporter github --summary "$GITHUB_STEP_SUMMARY"
```

`fetch-depth: 0` matters: `--since origin/main` resolves the base commit locally. The step summary shows the gate result and its reason, the counts, the score with the set it covers, the cache hits and misses, and the elapsed time, so a reviewer reads the outcome without opening the log. A failed run names the survivors on the diff and ends with `gate: failed: <reason>`.

`bench/cache-granularity.sh` records cold, warm, and partial-invalidation timings on a three-crate fixture and fails if a cached run disagrees with an uncached run of the same tree. The timing numbers vary too much between runners to gate on, so CI records them in the step summary and asserts only the agreement.

## This repository's own gate

`cargo mutants` on this crate reports **0 missed**: 309 mutants, 270 caught, 39 unviable. Unviable means the mutant did not compile, which is `cargo-mutants`' own verdict and outside the score.

The gate runs on every push to `main` and every pull request, and the `diff` job runs `mutator --since --cache` on pull request diffs. The parsers and selectors carry proptest properties alongside their example tests: diff text round-trips through the parser, line intersection agrees with per-line membership, ids survive a uniform line shift, verdicts refuse a mismatched tool version, and the accepted store round-trips for adversarial keys.

## Limits

Selective mutation testing is not equivalent to a full-workspace run. The guarantee runs one way: every path a diff touches that a function span cannot represent widens the selection until the mutants it can reach are measured. Nothing here proves the mutants outside the selection are killed.

- Selection follows function spans inside function bodies. A change to a function pulls in every mutant of that function, which is intentional but coarser than a line-level filter when a function is large.
- A source line outside every mutant's function, or a declaration inside one, widens to the whole package. The declaration test is a keyword scan, so it errs towards a wider run: a false positive costs time, while a missed caller would go unmeasured.
- A test, source, build-script, or manifest change widens the selection to whole packages, so a one-line edit in a large crate runs all of that crate's mutants. A path the owning package cannot attribute, such as a workspace-level lockfile or `tests/` directory, selects every mutant. `--all` is the explicit override for a wider sweep.
- Code generated into `OUT_DIR` by a build script, macro output, and `include!` of a file outside the crate are not modelled. A change to their generator is caught, a change to a file they read at build time is not.
- The score covers mutants this run selected. It is a measurement of the change, not of the repository.
- Reuse is per package. A workspace with one large crate gets less from `--cache` than one split into small crates.
