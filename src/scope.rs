//! What a changed path is, and the mutants a diff re-evaluates because of it.
//!
//! Function-span overlap only stands for a change inside a function body. What it
//! cannot represent widens the scope instead:
//!
//! * a test, which can kill a mutant whose function never moved;
//! * a manifest, lockfile, build script, or shared build configuration, which
//!   decides what every package compiles and runs;
//! * a source change outside the functions the mutants live in, such as a
//!   signature, type, constant, macro, import, deletion, or rename;
//! * a path no package owns, which widens to the whole workspace, because nothing
//!   proves which mutants it can observe.
//!
//! The guarantee is therefore one-directional: a selected run tests the mutants a
//! change can reach, plus the packages it falls back to. It does not prove the
//! repository's mutants are all killed, and its score describes the selection.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::diff::{Diff, Span};
use crate::mutant::Mutant;

/// What a changed path can change about a mutant's verdict.
///
/// The order is the order a report counts them in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    /// Rust code a mutant can live in.
    Production,
    /// Code the run compiles as a test: a `tests/` file or a `tests` module.
    Test,
    /// A manifest or lockfile, which changes what a package builds and runs.
    Manifest,
    /// A build script, which decides what its package compiles.
    BuildScript,
    /// Shared build configuration, which decides how every package compiles and runs.
    Config,
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
        } else if name == "build.rs" {
            Self::BuildScript
        } else if crate::cache::SHARED.contains(&path) {
            Self::Config
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

    /// The singular and plural nouns a report uses for a changed path of this kind.
    const fn nouns(self) -> (&'static str, &'static str) {
        match self {
            Self::Production => ("source file", "source files"),
            Self::Test => ("test file", "test files"),
            Self::Manifest => ("manifest", "manifests"),
            Self::BuildScript => ("build script", "build scripts"),
            Self::Config => ("configuration", "configurations"),
            Self::Other => ("file", "files"),
        }
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
    /// Every mutant of these packages, because a change in one falls outside a function span.
    Packages {
        packages: Vec<String>,
        files: Vec<String>,
    },
    /// Every mutant, because a changed path that widened the scope belongs to no single package.
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

/// Whether `diff` changes anything function-span overlap cannot represent.
///
/// When it does not, the touched functions are the whole selection and no package
/// lookup is needed. When it does, [`select`] widens the scope instead.
#[must_use]
pub fn widens(diff: &Diff, mutants: &[Mutant]) -> bool {
    diff.paths()
        .any(|path| kind_widens(Kind::of(path), path, diff, mutants))
}

/// The mutants a diff puts in scope, and why the scope is wider than its function spans.
///
/// `packages` maps each workspace member to its directory relative to the project
/// root. Every mutant a hunk touches stays in scope. On top of that, a changed path
/// that [`widens`] selects every mutant of the package that owns it. A widening path
/// inside no member, or inside a member with no mutants, puts every mutant in scope:
/// nothing proves which mutants such a change can observe.
#[must_use]
pub fn select(
    diff: &Diff,
    packages: &BTreeMap<String, PathBuf>,
    mutants: &[Mutant],
) -> (Reason, Vec<usize>) {
    let mut files = Vec::new();
    let mut names = BTreeSet::new();
    let mut unowned = Vec::new();
    for path in diff.paths() {
        if !kind_widens(Kind::of(path), path, diff, mutants) {
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
    let reason = if names.is_empty() {
        Reason::Touched
    } else {
        Reason::Packages {
            packages: names.iter().cloned().collect(),
            files,
        }
    };
    let selected = mutants
        .iter()
        .enumerate()
        .filter(|(_, mutant)| {
            names.contains(&mutant.package) || diff.touching(&mutant.file, mutant.enclosing_span())
        })
        .map(|(index, _)| index)
        .collect();
    (reason, selected)
}

/// Whether a changed path of this kind puts whole packages back in scope.
fn kind_widens(kind: Kind, path: &str, diff: &Diff, mutants: &[Mutant]) -> bool {
    match kind {
        Kind::Test | Kind::Manifest | Kind::BuildScript | Kind::Config => true,
        Kind::Other => false,
        Kind::Production => source_widens(path, diff, mutants),
    }
}

/// Whether a changed source file says something no function span can carry.
///
/// A changed line inside a mutant's enclosing function is exactly what span
/// selection stands for. A changed line outside every such span has no function to
/// attribute it to, whether it sits at module scope or in a function `cargo-mutants`
/// does not mutate. A declaration changes what callers compile against even when it
/// sits inside a span, and a file removed or renamed away leaves no new-side line.
fn source_widens(path: &str, diff: &Diff, mutants: &[Mutant]) -> bool {
    if diff.moved(path) {
        return true;
    }
    let spans: Vec<Span> = mutants
        .iter()
        .filter(|mutant| mutant.file == path)
        .map(Mutant::enclosing_span)
        .collect();
    diff.changed(path)
        .any(|(line, text)| !spans.iter().any(|span| covers(*span, line)) || declares(text))
}

/// Whether `text` starts an item whose change can move behaviour outside any
/// function body: a signature, type, trait, constant, macro, import, or attribute.
///
/// Only the item's own keyword counts, so a statement that merely mentions one
/// (`let w = async { … }`) stays inside span selection. The test is still
/// deliberately generous: a false positive costs a wider run, while a false
/// negative would leave a caller unmeasured.
fn declares(text: &str) -> bool {
    if text.starts_with('#') {
        return true;
    }
    let mut rest = text;
    if let Some(after) = rest.strip_prefix("pub") {
        match after.chars().next() {
            // A visibility split from its item by a newline changes that item.
            None => return true,
            Some(' ') => rest = after.trim_start(),
            Some('(') => {
                // `pub(crate)`, `pub(super)`, `pub(in path)`: the item follows the group.
                let Some((_, tail)) = after.split_once(')') else {
                    return true;
                };
                rest = tail.trim_start();
            }
            // `pub` inside a longer identifier, such as `publish`.
            Some(_) => return false,
        }
    }
    rest.split_whitespace()
        .next()
        .is_some_and(|token| DECLARATIONS.contains(&token))
}

/// The keywords a declaration starts with, after any `pub` visibility.
const DECLARATIONS: [&str; 15] = [
    "async",
    "const",
    "enum",
    "extern",
    "fn",
    "impl",
    "macro_rules!",
    "mod",
    "static",
    "struct",
    "trait",
    "type",
    "union",
    "unsafe",
    "use",
];

const fn covers(span: Span, line: u32) -> bool {
    let (start, end) = span.lines();
    start <= line && line <= end
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
    let mut counts = BTreeMap::new();
    for file in files {
        *counts.entry(Kind::of(file)).or_insert(0_usize) += 1;
    }
    let parts: Vec<String> = counts
        .into_iter()
        .map(|(kind, count)| {
            let (one, many) = kind.nouns();
            format!("{count} {}", plural(count, one, many))
        })
        .collect();
    match parts.as_slice() {
        [] => String::new(),
        [only] => only.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

const fn plural<'a>(count: usize, one: &'a str, many: &'a str) -> &'a str {
    if count == 1 { one } else { many }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::Pos;
    use crate::mutant::Function;
    use proptest::prelude::*;

    /// A mutant whose enclosing function spans lines 3 to 7.
    fn mutant(name: &str, package: &str, file: &str) -> Mutant {
        Mutant {
            name: name.to_owned(),
            file: file.to_owned(),
            package: package.to_owned(),
            genre: "FnValue".to_owned(),
            replacement: "0".to_owned(),
            span: Span::new(Pos::new(5, 5), Pos::new(5, 9)),
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

    fn two_packages() -> BTreeMap<String, PathBuf> {
        packages(&[("app", "crates/app"), ("core", "crates/core")])
    }

    /// One changed line at `line`, replacing the text `from` with `to`.
    fn edit(path: &str, line: u32, from: &str, to: &str) -> Diff {
        Diff::parse(&format!(
            "diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -{line},1 +{line},1 @@\n-{from}\n+{to}\n"
        ))
        .unwrap()
    }

    fn changed_at(path: &str, line: u32) -> Diff {
        edit(path, line, "a", "b")
    }

    fn changed(path: &str) -> Diff {
        changed_at(path, 1)
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
            ("crates/app/src/tests.rs", Kind::Test),
            ("tests/cli.rs", Kind::Test),
            ("crates/app/tests/integration.rs", Kind::Test),
            ("src/tests.rs", Kind::Test),
            ("Cargo.toml", Kind::Manifest),
            ("crates/app/Cargo.toml", Kind::Manifest),
            ("Cargo.lock", Kind::Manifest),
            ("build.rs", Kind::BuildScript),
            ("crates/app/build.rs", Kind::BuildScript),
            (".cargo/config.toml", Kind::Config),
            (".cargo/mutants.toml", Kind::Config),
            (".config/nextest.toml", Kind::Config),
            ("rust-toolchain.toml", Kind::Config),
            ("README.md", Kind::Other),
            ("nix/module.nix", Kind::Other),
        ] {
            assert_eq!(Kind::of(path), kind, "{path}");
        }
    }

    #[test]
    fn a_changed_test_selects_its_packages_mutants() {
        let (reason, selected) = select(
            &changed("crates/app/tests/t.rs"),
            &two_packages(),
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
        let (_, selected) = select(
            &deleted("crates/core/tests/t.rs"),
            &two_packages(),
            &fixture(),
        );
        assert_eq!(selected, vec![1]);
    }

    #[test]
    fn a_renamed_test_selects_the_packages_it_left() {
        let (reason, selected) = select(
            &renamed("crates/core/tests/a.rs", "crates/core/tests/b.rs"),
            &two_packages(),
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
        let (_, selected) = select(
            &changed("crates/app/tests/t.rs"),
            &packages(&[("workspace", ""), ("app", "crates/app")]),
            &fixture(),
        );
        assert_eq!(selected, vec![0], "only app's mutant");
    }

    #[test]
    fn a_root_package_owns_its_tests() {
        let (reason, selected) = select(
            &changed("tests/t.rs"),
            &packages(&[("root", "")]),
            &root_fixture(),
        );
        assert_eq!(selected, vec![0, 1]);
        assert!(matches!(reason, Reason::Packages { .. }));
    }

    #[test]
    fn a_test_outside_every_package_selects_everything() {
        let (reason, selected) = select(
            &changed("tests/integration.rs"),
            &packages(&[("app", "crates/app")]),
            &fixture(),
        );
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
        let (reason, selected) = select(
            &changed("fixtures/tests/t.rs"),
            &packages(&[("fixtures", "fixtures"), ("app", "crates/app")]),
            &fixture(),
        );
        assert_eq!(selected, vec![0, 1]);
        assert!(matches!(reason, Reason::Workspace { .. }));
    }

    #[test]
    fn a_member_manifest_selects_its_package() {
        let (reason, selected) = select(
            &changed("crates/app/Cargo.toml"),
            &two_packages(),
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
        let (reason, selected) = select(&changed("Cargo.lock"), &two_packages(), &fixture());
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
        let (reason, selected) = select(&changed("Cargo.toml"), &two_packages(), &fixture());
        assert_eq!(selected, vec![0, 1]);
        assert!(matches!(reason, Reason::Workspace { .. }));
    }

    #[test]
    fn a_single_packages_root_manifest_is_its_own() {
        let (reason, selected) = select(
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
    fn a_single_packages_lockfile_stays_inside_that_package() {
        let (reason, selected) = select(
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
    fn a_build_script_selects_its_package() {
        let (reason, selected) =
            select(&changed("crates/app/build.rs"), &two_packages(), &fixture());
        assert_eq!(selected, vec![0]);
        assert_eq!(
            reason.describe().as_deref(),
            Some("selection: 1 build script changed, so every mutant of app is in scope")
        );
    }

    #[test]
    fn shared_configuration_selects_everything() {
        let (reason, selected) =
            select(&changed(".cargo/config.toml"), &two_packages(), &fixture());
        assert_eq!(selected, vec![0, 1]);
        assert_eq!(
            reason,
            Reason::Workspace {
                files: vec![".cargo/config.toml".to_owned()],
            }
        );
    }

    #[test]
    fn a_body_change_inside_a_function_selects_by_span() {
        let diff = changed_at("crates/app/src/lib.rs", 5);
        assert_eq!(
            select(&diff, &two_packages(), &fixture()),
            (Reason::Touched, vec![0])
        );
    }

    #[test]
    fn a_change_outside_every_function_selects_the_package() {
        let diff = changed_at("crates/app/src/lib.rs", 1);
        let (reason, selected) = select(&diff, &two_packages(), &fixture());
        assert_eq!(selected, vec![0]);
        assert_eq!(
            reason.describe().as_deref(),
            Some("selection: 1 source file changed, so every mutant of app is in scope")
        );
    }

    #[test]
    fn a_signature_change_selects_the_package() {
        let diff = edit(
            "crates/app/src/lib.rs",
            3,
            "pub fn f(x: u8)",
            "pub fn f(x: u16)",
        );
        let (reason, selected) = select(&diff, &two_packages(), &fixture());
        assert_eq!(selected, vec![0]);
        assert!(matches!(reason, Reason::Packages { .. }));
    }

    #[test]
    fn only_a_leading_keyword_declares_an_item() {
        for line in [
            "pub(crate) fn f() {}",
            "pub(in crate::a) struct S;",
            "pub const N: u8 = 1;",
            "pub",
            "#[cfg(feature = \"x\")]",
            "use std::io;",
            "impl Trait for S {}",
            "macro_rules! m { () => {} }",
            "const N: u8 = 1;",
            "type T = u8;",
            "unsafe fn f() {}",
            "async fn f() {}",
        ] {
            assert!(declares(line), "{line}");
        }
        for line in [
            "let x = 1;",
            "self.0",
            "}}",
            "return f();",
            "// a comment",
            "let block = async { run() };",
            "publish();",
            "x = unsafe_op(a);",
        ] {
            assert!(!declares(line), "{line}");
        }
    }

    #[test]
    fn a_deleted_source_file_selects_its_package() {
        let (reason, selected) = select(
            &deleted("crates/app/src/lib.rs"),
            &two_packages(),
            &fixture(),
        );
        assert_eq!(selected, vec![0]);
        assert!(matches!(reason, Reason::Packages { packages, .. } if packages == ["app"]));
    }

    #[test]
    fn a_renamed_source_file_selects_both_packages() {
        let (reason, selected) = select(
            &renamed("crates/app/src/lib.rs", "crates/core/src/lib.rs"),
            &two_packages(),
            &fixture(),
        );
        assert_eq!(selected, vec![0, 1]);
        assert_eq!(
            reason,
            Reason::Packages {
                packages: vec!["app".to_owned(), "core".to_owned()],
                files: vec![
                    "crates/app/src/lib.rs".to_owned(),
                    "crates/core/src/lib.rs".to_owned(),
                ],
            }
        );
    }

    #[test]
    fn a_source_change_in_a_package_without_mutants_selects_everything() {
        let (reason, selected) = select(
            &changed_at("crates/app/src/lib.rs", 1),
            &packages(&[("app", "crates/app"), ("core", "crates/core")]),
            &[mutant("b", "core", "crates/core/src/lib.rs")],
        );
        assert_eq!(selected, vec![0]);
        assert!(matches!(reason, Reason::Workspace { .. }));
    }

    #[test]
    fn a_body_change_beside_a_test_selects_both() {
        let diff = Diff::parse(
            "diff --git a/crates/app/src/lib.rs b/crates/app/src/lib.rs\n--- a/crates/app/src/lib.rs\n+++ b/crates/app/src/lib.rs\n@@ -5,1 +5,1 @@\n-a\n+b\n\
             diff --git a/crates/core/tests/t.rs b/crates/core/tests/t.rs\n--- a/crates/core/tests/t.rs\n+++ b/crates/core/tests/t.rs\n@@ -1,1 +1,1 @@\n-a\n+b\n",
        )
        .unwrap();
        let (_, selected) = select(&diff, &two_packages(), &fixture());
        assert_eq!(selected, vec![0, 1]);
    }

    #[test]
    fn a_docs_change_selects_nothing() {
        assert_eq!(
            select(
                &changed("README.md"),
                &packages(&[("app", "crates/app")]),
                &fixture()
            ),
            (Reason::Touched, Vec::new())
        );
    }

    #[test]
    fn no_paths_selects_nothing() {
        let diff = Diff::parse("diff --git a/x.rs b/x.rs\n").unwrap();
        assert_eq!(
            select(&diff, &packages(&[]), &fixture()),
            (Reason::Touched, Vec::new())
        );
    }

    #[test]
    fn a_mode_only_change_widens_nothing() {
        // `git diff -U0` names the path but moves no line, so no mutant's code changed.
        let diff =
            Diff::parse("diff --git a/src/lib.rs b/src/lib.rs\nold mode 100644\nnew mode 100755\n")
                .unwrap();
        assert!(!widens(&diff, &root_fixture()));
        assert_eq!(
            select(&diff, &packages(&[("root", "")]), &root_fixture()),
            (Reason::Touched, Vec::new())
        );
    }

    #[test]
    fn the_count_names_each_kind() {
        assert_eq!(
            counting(&[
                "crates/app/src/lib.rs".to_owned(),
                "tests/a.rs".to_owned(),
                "tests/b.rs".to_owned(),
                "Cargo.lock".to_owned(),
                "build.rs".to_owned(),
                ".cargo/config.toml".to_owned(),
                "README.md".to_owned(),
            ]),
            "1 source file, 2 test files, 1 manifest, 1 build script, 1 configuration and 1 file"
        );
        assert_eq!(counting(&[]), "");
    }

    fn test_path() -> impl Strategy<Value = String> {
        (
            prop::option::of("[a-z][a-z0-9_]{0,5}/"),
            "[a-z][a-z0-9_]{0,7}",
        )
            .prop_map(|(dir, name)| format!("{}tests/{name}.rs", dir.unwrap_or_default()))
    }

    fn declaration() -> impl Strategy<Value = String> {
        (
            prop::option::of(prop_oneof![Just("pub "), Just("pub(crate) ")]),
            prop::sample::select(DECLARATIONS.to_vec()),
            "[a-z][a-z0-9_]{0,5}",
        )
            .prop_map(|(visibility, keyword, name)| {
                format!("{}{keyword} {name}", visibility.unwrap_or_default())
            })
    }

    proptest! {
        #[test]
        fn any_test_path_selects_the_mutants_of_its_package(path in test_path()) {
            let diff = changed(&path);
            prop_assert_eq!(Kind::of(&path), Kind::Test);
            let (reason, selected) = select(&diff, &packages(&[("root", "")]), &root_fixture());
            prop_assert_eq!(selected, vec![0, 1]);
            assert!(matches!(reason, Reason::Packages { .. }));
        }

        #[test]
        fn a_deleted_test_path_selects_the_same_mutants(path in test_path()) {
            let (_, selected) = select(&deleted(&path), &packages(&[("root", "")]), &root_fixture());
            prop_assert_eq!(selected, vec![0, 1]);
        }

        #[test]
        fn a_renamed_test_path_selects_the_same_mutants(from in test_path(), to in test_path()) {
            prop_assume!(from != to);
            let (_, selected) =
                select(&renamed(&from, &to), &packages(&[("root", "")]), &root_fixture());
            prop_assert_eq!(selected, vec![0, 1]);
        }

        /// Inside the enclosing function of the one mutant, a plain body edit is span selection.
        #[test]
        fn a_body_line_inside_a_function_needs_no_fallback(line in 3_u32..=7) {
            let diff = changed_at("src/lib.rs", line);
            let (reason, selected) = select(&diff, &packages(&[("root", "")]), &root_fixture());
            prop_assert_eq!(reason, Reason::Touched);
            prop_assert_eq!(selected, vec![0]);
        }

        /// Any other line of that file is outside every function span, so it falls back.
        #[test]
        fn a_line_outside_every_function_falls_back(line in prop::sample::select(vec![1_u32, 2, 8, 9, 40])) {
            let diff = changed_at("src/lib.rs", line);
            let (reason, selected) = select(&diff, &packages(&[("root", "")]), &root_fixture());
            prop_assert_eq!(selected, vec![0, 1]);
            assert!(matches!(reason, Reason::Packages { .. }));
        }

        /// A declaration widens even when the line it sits on is inside a function span.
        #[test]
        fn a_declaration_line_falls_back(text in declaration()) {
            let diff = edit("src/lib.rs", 5, "old", &text);
            let (reason, selected) = select(&diff, &packages(&[("root", "")]), &root_fixture());
            prop_assert_eq!(selected, vec![0, 1]);
            assert!(matches!(reason, Reason::Packages { .. }));
        }

        /// A deleted or renamed source file carries no line a span could hold.
        #[test]
        fn a_removed_source_path_falls_back(path in "[a-z][a-z0-9_]{0,7}/src/lib\\.rs") {
            for diff in [deleted(&path), renamed(&path, "src/moved.rs")] {
                let (_, selected) = select(&diff, &packages(&[("root", "")]), &root_fixture());
                prop_assert_eq!(selected, vec![0, 1]);
            }
        }
    }
}
