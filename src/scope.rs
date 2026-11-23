//! What a changed path is, and the mutants a diff re-evaluates because of it.
//!
//! A diff that only touches tests selects no mutant by function span, yet a new
//! test can kill a mutant whose function never moved. Those paths therefore put
//! whole packages back in scope, and a path no package owns puts the workspace in
//! scope, since nothing proves which mutants it can observe.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::diff::Diff;
use crate::mutant::Mutant;

/// What a changed path can change about a mutant's verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Rust code a mutant can live in.
    Production,
    /// Code the run compiles as a test: a `tests/` file or a `tests` module.
    Test,
    /// A manifest or lockfile, which changes what a package builds and runs.
    Manifest,
    /// A path no test run reads.
    Other,
}

impl Kind {
    /// Classify a project-relative path.
    #[must_use]
    pub fn of(path: &str) -> Self {
        let name = path.rsplit('/').next().unwrap_or(path);
        if matches!(name, "Cargo.toml" | "Cargo.lock") {
            Self::Manifest
        } else if name == "tests.rs" || path.split('/').any(|part| part == "tests") {
            Self::Test
        } else if Path::new(name)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("rs"))
        {
            Self::Production
        } else {
            Self::Other
        }
    }

    /// Whether a change to this path puts whole packages back in scope.
    #[must_use]
    const fn widens(self) -> bool {
        matches!(self, Self::Test | Self::Manifest)
    }
}

/// Why a run's scope is wider than the functions its diff touched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "reason")]
pub enum Reason {
    /// Every mutant, because no selector was given.
    All,
    /// Only the mutants whose functions the diff touches.
    Touched,
    /// Every mutant of these packages, because a test or manifest in one changed.
    Packages {
        packages: Vec<String>,
        files: Vec<String>,
    },
    /// Every mutant, because a changed test or manifest belongs to no single package.
    Workspace { files: Vec<String> },
}

impl Reason {
    /// Whether a stored verdict may be reused.
    ///
    /// A changed path no package owns belongs to no store, so the run cannot tell
    /// which verdicts it could have changed. Every selected mutant then runs again
    /// rather than trusting a store the change may have invalidated.
    #[must_use]
    pub const fn reuses_verdicts(&self) -> bool {
        !matches!(self, Self::Workspace { .. })
    }

    /// The report line explaining a widened scope, absent when the diff decided alone.
    #[must_use]
    pub fn describe(&self) -> Option<String> {
        match self {
            Self::All | Self::Touched => None,
            Self::Packages { packages, files } => Some(format!(
                "selection: {} changed, so every mutant of {} is in scope",
                counting(files),
                packages.join(", ")
            )),
            Self::Workspace { files } => Some(format!(
                "selection: every mutant is in scope because {} {} no package",
                files.join(", "),
                plural(files.len(), "belongs to", "belong to")
            )),
        }
    }
}

/// What a run selected: the label a report prints and why the scope is that wide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    pub label: String,
    pub reason: Reason,
}

impl Scope {
    /// Every mutant in the project.
    #[must_use]
    pub fn all() -> Self {
        Self {
            label: "all mutants".to_owned(),
            reason: Reason::All,
        }
    }

    /// The mutants the diff touches, labelled by where the diff came from.
    #[must_use]
    pub fn touched(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            reason: Reason::Touched,
        }
    }
}

/// Whether `diff` names a path whose change re-evaluates whole packages.
#[must_use]
pub fn widens(diff: &Diff) -> bool {
    diff.paths().any(|path| Kind::of(path).widens())
}

/// The mutants a changed test or manifest puts in scope, and why.
///
/// `packages` maps each workspace member to its directory relative to the project
/// root. A path inside no member, or inside a member with no mutants, puts every
/// mutant in scope: a test outside a package can still observe any mutant.
#[must_use]
pub fn widen(
    diff: &Diff,
    packages: &BTreeMap<String, PathBuf>,
    mutants: &[Mutant],
) -> (Reason, Vec<usize>) {
    let mut files = Vec::new();
    let mut names = BTreeSet::new();
    let mut unowned = Vec::new();
    for path in diff.paths() {
        let kind = Kind::of(path);
        if !kind.widens() {
            continue;
        }
        files.push(path.to_owned());
        if let Some(name) = owner(path, packages).filter(|name| has_mutants(mutants, name)) {
            names.insert(name.to_owned());
        } else {
            unowned.push(path.to_owned());
        }
    }
    if !unowned.is_empty() {
        return (
            Reason::Workspace { files: unowned },
            (0..mutants.len()).collect(),
        );
    }
    if names.is_empty() {
        return (Reason::Touched, Vec::new());
    }
    let selected = mutants
        .iter()
        .enumerate()
        .filter(|(_, mutant)| names.contains(&mutant.package))
        .map(|(index, _)| index)
        .collect();
    (
        Reason::Packages {
            packages: names.into_iter().collect(),
            files,
        },
        selected,
    )
}

/// The innermost package whose directory contains `path`.
fn owner<'a>(path: &str, packages: &'a BTreeMap<String, PathBuf>) -> Option<&'a str> {
    packages
        .iter()
        .filter(|(_, dir)| Path::new(path).starts_with(dir))
        .max_by_key(|(_, dir)| dir.components().count())
        .map(|(name, _)| name.as_str())
}

fn has_mutants(mutants: &[Mutant], package: &str) -> bool {
    mutants.iter().any(|mutant| mutant.package == package)
}

/// `1 test file`, `2 test files and 1 manifest`, from the paths that widened a scope.
fn counting(files: &[String]) -> String {
    let manifests = files
        .iter()
        .filter(|file| Kind::of(file) == Kind::Manifest)
        .count();
    let tests = files.len().saturating_sub(manifests);
    match (tests, manifests) {
        (tests, 0) => quantity(tests, "test file"),
        (0, manifests) => quantity(manifests, "manifest"),
        (tests, manifests) => format!(
            "{} and {}",
            quantity(tests, "test file"),
            quantity(manifests, "manifest")
        ),
    }
}

fn quantity(count: usize, noun: &str) -> String {
    format!("{count} {noun}{}", if count == 1 { "" } else { "s" })
}

const fn plural<'a>(count: usize, one: &'a str, many: &'a str) -> &'a str {
    if count == 1 { one } else { many }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{Pos, Span};
    use crate::mutant::Function;
    use proptest::prelude::*;

    fn mutant(name: &str, package: &str, file: &str) -> Mutant {
        Mutant {
            name: name.to_owned(),
            file: file.to_owned(),
            package: package.to_owned(),
            genre: "FnValue".to_owned(),
            replacement: "0".to_owned(),
            span: Span::new(Pos::new(3, 5), Pos::new(3, 9)),
            function: Some(Function {
                name: "f".to_owned(),
                span: Span::new(Pos::new(3, 1), Pos::new(7, 2)),
            }),
        }
    }

    fn fixture() -> Vec<Mutant> {
        vec![
            mutant("a", "app", "crates/app/src/lib.rs"),
            mutant("b", "core", "crates/core/src/lib.rs"),
        ]
    }

    /// One crate at the project root, whose manifest sits at `Cargo.toml`.
    fn root_fixture() -> Vec<Mutant> {
        vec![
            mutant("a", "root", "src/lib.rs"),
            mutant("b", "root", "src/parse.rs"),
        ]
    }

    fn packages(entries: &[(&str, &str)]) -> BTreeMap<String, PathBuf> {
        entries
            .iter()
            .map(|(name, dir)| ((*name).to_owned(), PathBuf::from(dir)))
            .collect()
    }

    fn changed(path: &str) -> Diff {
        Diff::parse(&format!(
            "diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -1,1 +1,1 @@\n-a\n+b\n"
        ))
        .unwrap()
    }

    fn renamed(from: &str, to: &str) -> Diff {
        Diff::parse(&format!(
            "diff --git a/{from} b/{to}\nsimilarity index 100%\nrename from {from}\nrename to {to}\n"
        ))
        .unwrap()
    }

    fn deleted(path: &str) -> Diff {
        Diff::parse(&format!(
            "diff --git a/{path} b/{path}\n--- a/{path}\n+++ /dev/null\n@@ -1,1 +0,0 @@\n-x\n"
        ))
        .unwrap()
    }

    #[test]
    fn classifies_every_kind_of_path() {
        for (path, kind) in [
            ("src/lib.rs", Kind::Production),
            ("crates/app/src/main.rs", Kind::Production),
            ("tests/cli.rs", Kind::Test),
            ("crates/app/tests/integration.rs", Kind::Test),
            ("src/tests.rs", Kind::Test),
            ("Cargo.toml", Kind::Manifest),
            ("crates/app/Cargo.toml", Kind::Manifest),
            ("Cargo.lock", Kind::Manifest),
            ("README.md", Kind::Other),
            ("nix/module.nix", Kind::Other),
        ] {
            assert_eq!(Kind::of(path), kind, "{path}");
        }
    }

    #[test]
    fn a_changed_test_selects_its_packages_mutants() {
        let diff = changed("crates/app/tests/t.rs");
        let (reason, selected) = widen(
            &diff,
            &packages(&[("app", "crates/app"), ("core", "crates/core")]),
            &fixture(),
        );
        assert_eq!(selected, vec![0]);
        assert_eq!(
            reason,
            Reason::Packages {
                packages: vec!["app".to_owned()],
                files: vec!["crates/app/tests/t.rs".to_owned()],
            }
        );
        assert_eq!(
            reason.describe().as_deref(),
            Some("selection: 1 test file changed, so every mutant of app is in scope")
        );
    }

    #[test]
    fn a_deleted_test_selects_its_packages_mutants() {
        let (_, selected) = widen(
            &deleted("crates/core/tests/t.rs"),
            &packages(&[("app", "crates/app"), ("core", "crates/core")]),
            &fixture(),
        );
        assert_eq!(selected, vec![1]);
    }

    #[test]
    fn a_renamed_test_selects_the_packages_it_left() {
        let (reason, selected) = widen(
            &renamed("crates/core/tests/a.rs", "crates/core/tests/b.rs"),
            &packages(&[("app", "crates/app"), ("core", "crates/core")]),
            &fixture(),
        );
        assert_eq!(selected, vec![1]);
        assert_eq!(
            reason,
            Reason::Packages {
                packages: vec!["core".to_owned()],
                files: vec![
                    "crates/core/tests/a.rs".to_owned(),
                    "crates/core/tests/b.rs".to_owned(),
                ],
            }
        );
    }

    #[test]
    fn the_innermost_package_owns_a_path() {
        let (_, selected) = widen(
            &changed("crates/app/tests/t.rs"),
            &packages(&[("workspace", ""), ("app", "crates/app")]),
            &fixture(),
        );
        assert_eq!(selected, vec![0], "only app's mutant");
    }

    #[test]
    fn a_root_package_owns_its_tests() {
        let (reason, selected) = widen(
            &changed("tests/t.rs"),
            &packages(&[("root", "")]),
            &root_fixture(),
        );
        assert_eq!(selected, vec![0, 1]);
        assert!(matches!(reason, Reason::Packages { .. }));
    }

    #[test]
    fn a_test_outside_every_package_selects_everything() {
        let diff = changed("tests/integration.rs");
        let (reason, selected) = widen(&diff, &packages(&[("app", "crates/app")]), &fixture());
        assert_eq!(selected, vec![0, 1]);
        assert_eq!(
            reason,
            Reason::Workspace {
                files: vec!["tests/integration.rs".to_owned()],
            }
        );
        assert_eq!(
            reason.describe().as_deref(),
            Some(
                "selection: every mutant is in scope because tests/integration.rs belongs to no package"
            )
        );
    }

    #[test]
    fn a_test_in_a_package_without_mutants_selects_everything() {
        let (reason, selected) = widen(
            &changed("fixtures/tests/t.rs"),
            &packages(&[("fixtures", "fixtures"), ("app", "crates/app")]),
            &fixture(),
        );
        assert_eq!(selected, vec![0, 1]);
        assert!(matches!(reason, Reason::Workspace { .. }));
    }

    #[test]
    fn a_member_manifest_selects_its_package() {
        let (reason, selected) = widen(
            &changed("crates/app/Cargo.toml"),
            &packages(&[("app", "crates/app"), ("core", "crates/core")]),
            &fixture(),
        );
        assert_eq!(selected, vec![0]);
        assert_eq!(
            reason.describe().as_deref(),
            Some("selection: 1 manifest changed, so every mutant of app is in scope")
        );
    }

    #[test]
    fn the_lockfile_selects_everything() {
        let (reason, selected) = widen(
            &changed("Cargo.lock"),
            &packages(&[("app", "crates/app"), ("core", "crates/core")]),
            &fixture(),
        );
        assert_eq!(selected, vec![0, 1]);
        assert_eq!(
            reason,
            Reason::Workspace {
                files: vec!["Cargo.lock".to_owned()],
            }
        );
    }

    #[test]
    fn a_workspace_root_manifest_selects_everything() {
        let (reason, selected) = widen(
            &changed("Cargo.toml"),
            &packages(&[("app", "crates/app"), ("core", "crates/core")]),
            &fixture(),
        );
        assert_eq!(selected, vec![0, 1]);
        assert!(matches!(reason, Reason::Workspace { .. }));
    }

    #[test]
    fn a_single_packages_root_manifest_is_its_own() {
        let (reason, selected) = widen(
            &changed("Cargo.toml"),
            &packages(&[("root", "")]),
            &root_fixture(),
        );
        assert_eq!(selected, vec![0, 1]);
        assert_eq!(
            reason,
            Reason::Packages {
                packages: vec!["root".to_owned()],
                files: vec!["Cargo.toml".to_owned()],
            }
        );
    }

    #[test]
    fn tests_and_manifests_are_counted_together() {
        let diff = Diff::parse(
            "diff --git a/tests/a.rs b/tests/a.rs\n--- a/tests/a.rs\n+++ b/tests/a.rs\n@@ -1,1 +1,1 @@\n-a\n+b\n\
             diff --git a/tests/b.rs b/tests/b.rs\n--- a/tests/b.rs\n+++ b/tests/b.rs\n@@ -1,1 +1,1 @@\n-a\n+b\n\
             diff --git a/Cargo.lock b/Cargo.lock\n--- a/Cargo.lock\n+++ b/Cargo.lock\n@@ -1,1 +1,1 @@\n-a\n+b\n",
        )
        .unwrap();
        let (reason, _) = widen(
            &diff,
            &packages(&[("app", "crates/app"), ("core", "crates/core")]),
            &fixture(),
        );
        assert_eq!(
            reason,
            Reason::Workspace {
                files: vec![
                    "Cargo.lock".to_owned(),
                    "tests/a.rs".to_owned(),
                    "tests/b.rs".to_owned()
                ],
            }
        );
        assert_eq!(
            counting(&[
                "tests/a.rs".to_owned(),
                "tests/b.rs".to_owned(),
                "Cargo.lock".to_owned()
            ]),
            "2 test files and 1 manifest"
        );
    }

    #[test]
    fn a_single_packages_lockfile_stays_inside_that_package() {
        let (reason, selected) = widen(
            &changed("Cargo.lock"),
            &packages(&[("root", "")]),
            &root_fixture(),
        );
        assert_eq!(selected, vec![0, 1]);
        assert_eq!(
            reason,
            Reason::Packages {
                packages: vec!["root".to_owned()],
                files: vec!["Cargo.lock".to_owned()],
            }
        );
    }

    #[test]
    fn a_production_change_widens_nothing() {
        let diff = changed("crates/app/src/lib.rs");
        assert!(!widens(&diff));
        assert_eq!(
            widen(&diff, &packages(&[("app", "crates/app")]), &fixture()),
            (Reason::Touched, Vec::new())
        );
    }

    #[test]
    fn one_test_path_beside_production_code_still_widens() {
        let diff = Diff::parse(
            "diff --git a/crates/app/src/lib.rs b/crates/app/src/lib.rs\n--- a/crates/app/src/lib.rs\n+++ b/crates/app/src/lib.rs\n@@ -3,1 +3,1 @@\n-a\n+b\n\
             diff --git a/crates/core/tests/t.rs b/crates/core/tests/t.rs\n--- a/crates/core/tests/t.rs\n+++ b/crates/core/tests/t.rs\n@@ -1,1 +1,1 @@\n-a\n+b\n",
        )
        .unwrap();
        assert!(widens(&diff));
        let (_, selected) = widen(
            &diff,
            &packages(&[("app", "crates/app"), ("core", "crates/core")]),
            &fixture(),
        );
        assert_eq!(selected, vec![1]);
    }

    #[test]
    fn a_diff_of_other_inputs_selects_nothing() {
        let diff = changed("README.md");
        assert!(!widens(&diff));
        assert_eq!(
            widen(&diff, &packages(&[("app", "crates/app")]), &fixture()),
            (Reason::Touched, Vec::new())
        );
    }

    #[test]
    fn no_paths_is_no_widening() {
        let diff = Diff::parse("diff --git a/x.rs b/x.rs\n").unwrap();
        assert!(!widens(&diff));
        assert_eq!(
            widen(&diff, &packages(&[]), &fixture()),
            (Reason::Touched, Vec::new())
        );
    }

    fn test_path() -> impl Strategy<Value = String> {
        (
            prop::option::of("[a-z][a-z0-9_]{0,5}/"),
            "[a-z][a-z0-9_]{0,7}",
        )
            .prop_map(|(dir, name)| format!("{}tests/{name}.rs", dir.unwrap_or_default()))
    }

    proptest! {
        #[test]
        fn any_test_path_selects_the_mutants_of_its_package(path in test_path()) {
            let diff = changed(&path);
            prop_assert_eq!(Kind::of(&path), Kind::Test);
            prop_assert!(widens(&diff));
            let (reason, selected) = widen(&diff, &packages(&[("root", "")]), &root_fixture());
            prop_assert_eq!(selected, vec![0, 1]);
            assert!(matches!(reason, Reason::Packages { .. }));
        }

        #[test]
        fn a_deleted_test_path_selects_the_same_mutants(path in test_path()) {
            let (_, selected) = widen(&deleted(&path), &packages(&[("root", "")]), &root_fixture());
            prop_assert_eq!(selected, vec![0, 1]);
        }

        #[test]
        fn a_renamed_test_path_selects_the_same_mutants(from in test_path(), to in test_path()) {
            prop_assume!(from != to);
            let (_, selected) =
                widen(&renamed(&from, &to), &packages(&[("root", "")]), &root_fixture());
            prop_assert_eq!(selected, vec![0, 1]);
        }
    }
}
