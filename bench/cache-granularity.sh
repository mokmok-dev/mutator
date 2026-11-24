#!/usr/bin/env bash
# What a cache finer than "per package" could save, and where it would be wrong.
#
# Builds a three-crate workspace (app -> util -> math), warms the verdict cache on it,
# then applies one edit at a time to a fresh copy of that warmed tree. Each scenario
# runs twice from the same starting point, once with `--cache` and once without. The
# uncached run is the ground truth, so a cached run that disagrees with it fails the
# script with a diff.
#
# The scenarios are the cases a cache key has to separate:
#   one-function  a source edit inside one function of the root package
#   one-test      a test file of the root package
#   sibling-file  a module of the root package that no mutant lives in
#   dependency    a function of a crate the root package depends on
#   dep-test      a test file of a crate the root package depends on
#
# `reused` counts verdicts the cache served. `flipped` counts mutants present on both
# sides whose verdict the edit changed, and `moved by` names them. A key that leaves
# out a file named in `moved by` serves a stale verdict, so those two columns are the
# soundness boundary for any key finer than the package:
#
#   one-test and sibling-file both flip verdicts of mutants in files the edit never
#   touched, so no key built from a mutant and its own file is sound. one-function
#   flips none *in this fixture*, which is not a proof that a function-level key is
#   sound: a mutant's verdict depends on every value its tests compute.
#
# Usage: bench/cache-granularity.sh   (from `nix develop ./dev`, or with MUTATOR=<path>)
set -euo pipefail

repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
mutator=${MUTATOR:-$repo/target/debug/mutator}
jobs=${JOBS:-4}

[[ -x $mutator ]] || {
    echo "build the binary first: nix develop ./dev --command cargo build" >&2
    exit 1
}
command -v jq >/dev/null || {
    echo "jq is required to read the JSON report" >&2
    exit 1
}

work=$(mktemp -d "${TMPDIR:-/tmp}/mutator-cache-bench.XXXXXX")
trap 'rm -rf "$work"' EXIT
out=$work/out
base=$work/base
mkdir -p "$out" "$base"

write_fixture() {
    cd "$base"
    git init -q
    git config user.email bench@example.invalid
    git config user.name bench
    mkdir -p math/src util/src util/tests app/src app/tests
    cat >Cargo.toml <<'EOF'
[workspace]
members = ["app", "math", "util"]
resolver = "3"

[workspace.package]
edition = "2024"
version = "0.1.0"

[workspace.dependencies]
math = { path = "math" }
util = { path = "util" }
EOF
    echo '/target' >.gitignore
    cat >math/Cargo.toml <<'EOF'
[package]
name = "math"
edition.workspace = true
version.workspace = true
EOF
    cat >math/src/lib.rs <<'EOF'
//! Bounds the other crates derive their values from.

pub fn twice(value: i32) -> i32 {
    value * 2
}

pub fn threshold() -> i32 {
    1
}

#[cfg(test)]
mod tests {
    #[test]
    fn twice_doubles() {
        assert_eq!(super::twice(3), 6);
    }
}
EOF
    cat >util/Cargo.toml <<'EOF'
[package]
name = "util"
edition.workspace = true
version.workspace = true

[dependencies]
math.workspace = true
EOF
    cat >util/src/lib.rs <<'EOF'
//! Values derived from `math` for the app crate.

pub fn threshold() -> i32 {
    math::threshold()
}

pub fn score(value: i32) -> i32 {
    math::twice(value)
}
EOF
    cat >util/tests/scores.rs <<'EOF'
#[test]
fn the_threshold_reaches_the_app() {
    assert!(util::threshold() > 0);
}

#[test]
fn a_score_is_twice_its_value() {
    assert_eq!(util::score(3), 6);
}
EOF
    cat >app/Cargo.toml <<'EOF'
[package]
name = "app"
edition.workspace = true
version.workspace = true

[dependencies]
util.workspace = true
EOF
    cat >app/src/bound.rs <<'EOF'
//! The bound one test compares against. No mutant lives in this file.

pub fn k() -> i32 {
    1
}
EOF
    cat >app/src/lib.rs <<'EOF'
//! The root package's values.

pub mod bound;

pub fn route(kind: u8) -> i32 {
    if kind == 0 { alpha() } else { 0 }
}

pub fn alpha() -> i32 {
    7
}

pub fn beta() -> i32 {
    9
}

pub fn g() -> i32 {
    2
}

pub fn label(value: i32) -> i32 {
    if value > util::threshold() { 1 } else { 0 }
}
EOF
    cat >app/tests/behavior.rs <<'EOF'
#[test]
fn route_zero_is_alpha() {
    assert_eq!(app::route(0), 7);
}

#[test]
fn route_one_is_zero() {
    assert_eq!(app::route(1), 0);
}

#[test]
fn alpha_is_seven() {
    assert_eq!(app::alpha(), 7);
}

#[test]
fn beta_is_nine() {
    assert_eq!(app::beta(), 9);
}

#[test]
fn g_exceeds_its_bound() {
    assert!(app::g() > app::bound::k());
}

#[test]
fn label_is_zero_at_the_threshold() {
    assert_eq!(app::label(1), 0);
}
EOF
    # `Cargo.lock` is a fingerprint input, so it must exist before the first run.
    cargo metadata --format-version 1 >/dev/null
    git add -A
    git commit -qm base
}

# Run, print the elapsed seconds, leave the JSON report at `$1`.
timed_json() {
    local report=$1
    shift
    local seconds
    TIMEFORMAT=%3R
    seconds=$({ time "$@" >"$report" 2>"$report.stderr"; } 2>&1)
    printf '%s\n' "$seconds"
}

# Seconds per run, alternating the two sides so drift favours neither.
# Leaves one timing per line in `$3`, cached first.
rounds=${ROUNDS:-5}
timings() {
    local cached_tree=$1 plain_tree=$2 into=$3
    : >"$into.cached"
    : >"$into.plain"
    local round
    for ((round = 1; round <= rounds; round++)); do
        timed_json "$out/timing.json" "$mutator" --root "$cached_tree" --all --cache --jobs "$jobs" --reporter json >>"$into.cached"
        timed_json "$out/timing.json" "$mutator" --root "$plain_tree" --all --jobs "$jobs" --reporter json >>"$into.plain"
    done
}

median() { sort -n | awk '{ v[NR] = $1 } END { print v[int((NR + 1) / 2)] }'; }
spread() { sort -n | awk 'NR == 1 { low = $1 } { high = $1 } END { print low "-" high }'; }

# One `id<TAB>status` line per mutant, sorted by id.
# Byte order, the order `join` requires, rather than the caller's locale.
rows() { jq -r '.rows[] | [.id, .status] | @tsv' "$1" | LC_ALL=C sort; }
reused() { jq -r '.reused.caught + .reused.missed' "$1"; }
total() { jq -r '.rows | length' "$1"; }

# `$1` label, `$2` edit function, applied inside a copied tree.
scenario() {
    local name=$1 edit=$2
    local cached_tree=$work/$name-cached
    local plain_tree=$work/$name-plain
    cp -a "$base" "$cached_tree"
    cp -a "$base" "$plain_tree"
    (cd "$cached_tree" && "$edit")
    (cd "$plain_tree" && "$edit")

    local cached=$out/$name-cached.json
    local reality=$out/$name-uncached.json
    "$mutator" --root "$cached_tree" --all --cache --jobs "$jobs" --reporter json >"$cached" 2>"$cached.stderr"
    "$mutator" --root "$plain_tree" --all --jobs "$jobs" --reporter json >"$reality" 2>"$reality.stderr"
    timings "$cached_tree" "$plain_tree" "$out/$name.time"

    rows "$reality" >"$out/$name-reality.tsv"
    rows "$cached" >"$out/$name-cached.tsv"
    if ! diff -q "$out/$name-reality.tsv" "$out/$name-cached.tsv" >/dev/null; then
        echo "FAILED: the cached run of '$name' disagrees with an uncached run" >&2
        diff "$out/$name-reality.tsv" "$out/$name-cached.tsv" >&2 || true
        return 1
    fi

    # Join the base verdicts with the edited ones, so a verdict that flipped is kept
    # apart from a mutant the edit created or removed. `rows` sorts byte-wise, so
    # `join` must compare byte-wise too: the caller's locale ignores the punctuation
    # in an id such as `-1#0`, and refuses the input as unsorted.
    local joined flipped names added removed
    joined=$(LC_ALL=C join -t $'\t' -a 1 -a 2 -e NONE -o 0,1.2,2.2 \
        "$out/base.tsv" "$out/$name-reality.tsv")
    flipped=$(printf '%s\n' "$joined" |
        awk -F'\t' '$2 != "NONE" && $3 != "NONE" && $2 != $3' | wc -l)
    names=$(printf '%s\n' "$joined" |
        awk -F'\t' '$2 != "NONE" && $3 != "NONE" && $2 != $3 { print $1 }' | paste -sd' ' -)
    added=$(printf '%s\n' "$joined" | awk -F'\t' '$2 == "NONE"' | wc -l)
    removed=$(printf '%s\n' "$joined" | awk -F'\t' '$3 == "NONE"' | wc -l)

    printf '| %s | %s | %s of %s | %s (%s) | %s (%s) | %s | %s | %s | %s |\n' \
        "$name" "$(total "$cached")" "$(reused "$cached")" "$(total "$cached")" \
        "$(median <"$out/$name.time.cached")" "$(spread <"$out/$name.time.cached")" \
        "$(median <"$out/$name.time.plain")" "$(spread <"$out/$name.time.plain")" \
        "$flipped" "$added" "$removed" "${names:-none}"
}

edit_function() {
    cat >app/src/lib.rs <<'EOF'
//! The root package's values.

pub mod bound;

pub fn route(kind: u8) -> i32 {
    if kind == 0 { 7 } else { 0 }
}

pub fn alpha() -> i32 {
    7
}

pub fn beta() -> i32 {
    9
}

pub fn g() -> i32 {
    2
}

pub fn label(value: i32) -> i32 {
    if value > util::threshold() { 1 } else { 0 }
}
EOF
}

edit_test() {
    cat >app/tests/behavior.rs <<'EOF'
#[test]
fn route_zero_is_alpha() {
    assert_eq!(app::route(0), 7);
}

#[test]
fn route_one_is_zero() {
    assert_eq!(app::route(1), 0);
}

#[test]
fn alpha_is_seven() {
    assert_eq!(app::alpha(), 7);
}

#[test]
fn g_exceeds_its_bound() {
    assert!(app::g() > app::bound::k());
}

#[test]
fn label_is_zero_at_the_threshold() {
    assert_eq!(app::label(1), 0);
}
EOF
}

edit_sibling() {
    cat >app/src/bound.rs <<'EOF'
//! The bound one test compares against. No mutant lives in this file.

pub fn k() -> i32 {
    -5
}
EOF
}

edit_dependency() {
    cat >math/src/lib.rs <<'EOF'
//! Bounds the other crates derive their values from.

pub fn twice(value: i32) -> i32 {
    value * 2
}

pub fn threshold() -> i32 {
    2
}

#[cfg(test)]
mod tests {
    #[test]
    fn twice_doubles() {
        assert_eq!(super::twice(3), 6);
    }
}
EOF
}

edit_dependency_test() {
    cat >util/tests/scores.rs <<'EOF'
#[test]
fn the_threshold_reaches_the_app() {
    assert!(util::threshold() > 0);
}
EOF
}

write_fixture

cold=$(timed_json "$out/base.json" "$mutator" --root "$base" --all --cache --jobs "$jobs" --reporter json)
warm=$(timed_json "$out/base-warm.json" "$mutator" --root "$base" --all --cache --jobs "$jobs" --reporter json)
rows "$out/base.json" >"$out/base.tsv"

printf 'base: %s mutants, %ss cold, %ss warm, %s of %s verdicts reused\n\n' \
    "$(total "$out/base.json")" "$cold" "$warm" \
    "$(reused "$out/base-warm.json")" "$(total "$out/base-warm.json")"
printf 'Times are seconds, median of %s alternating runs, with the range in brackets.\n\n' "$rounds"
printf '| change | mutants | reused | cached | uncached | flipped | added | removed | moved by |\n'
printf '|---|---|---|---|---|---|---|---|---|\n'
scenario one-function edit_function
scenario one-test edit_test
scenario sibling-file edit_sibling
scenario dependency edit_dependency
scenario dep-test edit_dependency_test
