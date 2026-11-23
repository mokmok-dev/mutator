//! Verdict reuse keyed by a digest of everything a package's tests can observe.
//!
//! `cargo-mutants` reruns `cargo test` for the package a mutant lives in, so a
//! verdict depends on that package's own files and on every local package it
//! depends on. The key is the content of all of them, plus the workspace files
//! and tool versions that decide what a run means: a change anywhere in the
//! closure invalidates the entry, and an unchanged closure hits.
//!
//! Inputs this cannot see, and therefore does not claim to cover: a file a test
//! or build script opens by a path outside the package directory, such as a
//! fixture in a sibling directory; environment variables; the installed compiler,
//! which moves without `rust-toolchain.toml` changing; and anything outside the
//! tree.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::exec::Exec;

pub const DIR: &str = "target/mutator-cache";

const VERSION: u32 = 1;

/// Bumped when the inputs a fingerprint covers change, so old stores stop matching.
const FINGERPRINT_VERSION: u32 = 2;

/// The `cargo-mutants` configuration, which can widen a run past the closure or
/// change the features it builds with.
const MUTANTS_CONFIG: &str = ".cargo/mutants.toml";

/// Workspace-level files whose content decides what a package's tests build and run.
///
/// A change to one also widens the selection to the whole workspace, since it
/// decides more than the package that happens to contain it.
pub(crate) const SHARED: [&str; 8] = [
    "Cargo.lock",
    "Cargo.toml",
    MUTANTS_CONFIG,
    ".cargo/config",
    ".cargo/config.toml",
    ".config/nextest.toml",
    "rust-toolchain",
    "rust-toolchain.toml",
];

/// One cached verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub package: String,
    /// `cargo-mutants` reported this mutant as caught, so the tests kill it.
    pub caught: bool,
}

/// The verdicts known to be valid for the current package contents.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Hits {
    entries: BTreeMap<String, Entry>,
}

impl Hits {
    /// Whether the tests were seen killing this mutant's name.
    #[must_use]
    pub fn killed(&self, name: &str) -> bool {
        self.entries.get(name).is_some_and(|entry| entry.caught)
    }

    /// Whether any verdict exists for this mutant, killed or not.
    #[must_use]
    pub fn seen(&self, name: &str) -> bool {
        self.entries.contains_key(name)
    }

    pub fn insert(&mut self, name: &str, entry: Entry) {
        self.entries.insert(name.to_owned(), entry);
    }

    #[must_use]
    pub const fn entries(&self) -> &BTreeMap<String, Entry> {
        &self.entries
    }

    /// Merge another hit set in, for the per-package files of one run.
    pub fn extend(&mut self, other: &Self) {
        for (name, entry) in &other.entries {
            self.entries.insert(name.clone(), entry.clone());
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// A file's project-relative path and content hash.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct FileHash {
    pub path: String,
    pub hash: [u8; 32],
}

/// The digest that decides whether a cached verdict still applies.
///
/// `sources` are every file of the package's dependency closure, plus the shared
/// configuration, each named by its project-relative path, along with the tool
/// versions that change what a verdict means.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Fingerprint([u8; 32]);

impl Fingerprint {
    #[must_use]
    pub fn compute(package: &str, sources: &[FileHash], cargo_mutants: &str, this: &str) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(format!(
            "v{FINGERPRINT_VERSION}\0{package}\0{cargo_mutants}\0{this}\0"
        ));
        for FileHash { path, hash } in sources {
            hasher.update(path.as_bytes());
            hasher.update([0]);
            hasher.update(hash);
        }
        Self(hasher.finalize().into())
    }

    #[must_use]
    pub fn to_hex(self) -> String {
        use std::fmt::Write as _;
        self.0
            .iter()
            .fold(String::with_capacity(64), |mut text, byte| {
                let _ = write!(text, "{byte:02x}");
                text
            })
    }
}

/// One package's verdicts, ready to store: whose they are and what they say.
pub type PackageVerdicts = (Fingerprint, Vec<(String, Entry)>);

/// A parsed store file: one package's valid verdicts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Store {
    version: u32,
    package: String,
    cargo_mutants: String,
    entries: BTreeMap<String, Entry>,
}

impl Store {
    #[must_use]
    pub fn empty(package: &str, cargo_mutants: &str) -> Self {
        Self {
            version: VERSION,
            package: package.to_owned(),
            cargo_mutants: cargo_mutants.to_owned(),
            entries: BTreeMap::new(),
        }
    }

    /// # Errors
    ///
    /// Returns when the text is not a version 1 store for this package.
    pub fn parse(text: &str) -> Result<Self> {
        let store: Self = serde_json::from_str(text).map_err(|source| Error::Cache { source })?;
        store.check_version()?;
        Ok(store)
    }

    /// # Errors
    ///
    /// Returns when the store cannot be serialized.
    pub fn render(&self) -> Result<String> {
        let mut text = serde_json::to_string_pretty(self).map_err(|source| Error::Serialize {
            what: "cache entry",
            source,
        })?;
        text.push('\n');
        Ok(text)
    }

    /// # Errors
    ///
    /// Returns when the store names another version, package, or `cargo-mutants`
    /// release, since its verdicts then came from a different tool.
    pub fn check(&self, package: &str, cargo_mutants: &str) -> Result<()> {
        self.check_version()?;
        if self.package != package {
            return Err(Error::CachePackage {
                found: self.package.clone(),
                expected: package.to_owned(),
            });
        }
        if self.cargo_mutants != cargo_mutants {
            return Err(Error::CacheTool {
                found: self.cargo_mutants.clone(),
                expected: cargo_mutants.to_owned(),
            });
        }
        Ok(())
    }

    const fn check_version(&self) -> Result<()> {
        if self.version != VERSION {
            return Err(Error::CacheVersion {
                version: self.version,
            });
        }
        Ok(())
    }

    #[must_use]
    pub const fn entries(&self) -> &BTreeMap<String, Entry> {
        &self.entries
    }

    pub const fn entries_mut(&mut self) -> &mut BTreeMap<String, Entry> {
        &mut self.entries
    }
}

/// Where a store file lives: one file per package, named by its fingerprint.
#[must_use]
pub fn path(root: &Path, package: &str, fingerprint: &Fingerprint) -> PathBuf {
    root.join(DIR)
        .join(sanitize(package))
        .join(format!("{}.json", fingerprint.to_hex()))
}

/// Package names are also used as directory names, so keep the safe characters.
fn sanitize(package: &str) -> String {
    package
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Load one package's valid verdicts, or nothing when there is no usable entry.
///
/// A store written by another `cargo-mutants` version or another package is
/// discarded rather than trusted.
#[must_use]
pub fn load(root: &Path, package: &str, fingerprint: &Fingerprint, cargo_mutants: &str) -> Hits {
    let file = path(root, package, fingerprint);
    let Ok(text) = std::fs::read_to_string(&file) else {
        return Hits::default();
    };
    let Ok(store) = Store::parse(&text) else {
        return Hits::default();
    };
    if store.check(package, cargo_mutants).is_err() {
        return Hits::default();
    }
    Hits {
        entries: store.entries,
    }
}

fn write(root: &Path, package: &str, fingerprint: &Fingerprint, store: &Store) -> Result<()> {
    let file = path(root, package, fingerprint);
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent).map_err(|source| Error::Create {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    std::fs::write(&file, store.render()?).map_err(|source| Error::Write { path: file, source })
}

/// The `cargo-mutants` version, read once per run from the list output.
///
/// # Errors
///
/// Returns when `cargo-mutants` cannot report its version.
pub fn cargo_mutants_version(exec: &dyn Exec, root: &Path) -> Result<String> {
    let output = exec.run("cargo", &version_args(), root)?;
    if !output.success() {
        return Err(Error::Version {
            stderr: output.stderr.trim().to_owned(),
        });
    }
    Ok(output.stdout.trim().to_owned())
}

fn version_args() -> Vec<String> {
    vec!["mutants".to_owned(), "--version".to_owned()]
}

/// This tool's version, which also fixes the verdict rules.
#[must_use]
pub const fn this_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Write the verdicts this run learned, per package.
///
/// # Errors
///
/// Returns when the cache directory cannot be written.
pub fn save_all(
    root: &Path,
    packages: &BTreeMap<String, PackageVerdicts>,
    cargo_mutants: &str,
) -> Result<()> {
    for (package, (fingerprint, entries)) in packages {
        let mut store = Store::empty(package, cargo_mutants);
        for (name, entry) in entries {
            store.entries_mut().insert(name.clone(), entry.clone());
        }
        write(root, package, fingerprint, &store)?;
    }
    Ok(())
}

/// The files whose content decides a package's test outcome: the package's own
/// sources, tests, benches, examples, and manifest, then the same for every local
/// package it depends on, then the shared configuration at the workspace root.
#[must_use]
pub fn fingerprint_sources(package: &Package) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for dir in package.closure.values() {
        package_files(dir, &mut files);
    }
    for name in SHARED {
        let shared = package.workspace_root.join(name);
        if shared.is_file() {
            files.push(shared);
        }
    }
    files.sort();
    files.dedup();
    files
}

/// Every file of one package directory that a test run can observe.
fn package_files(dir: &Path, files: &mut Vec<PathBuf>) {
    for sub in ["src", "tests", "benches", "examples"] {
        collect(&dir.join(sub), files);
    }
    for name in ["Cargo.toml", "build.rs"] {
        let named = dir.join(name);
        if named.is_file() {
            files.push(named);
        }
    }
}

/// Every file under `dir`, recursively, so a build script's `include_str!` target counts.
fn collect(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, files);
        } else if path.is_file() {
            files.push(path);
        }
    }
}

/// `cargo`'s argv for the resolved workspace graph: which packages exist, where
/// they live, and which of them depend on which.
#[must_use]
pub fn metadata_args() -> Vec<String> {
    vec![
        "metadata".to_owned(),
        "--format-version".to_owned(),
        "1".to_owned(),
    ]
}

#[derive(Debug, Deserialize)]
struct Metadata {
    packages: Vec<MetadataPackage>,
    resolve: Option<Resolve>,
    workspace_root: String,
}

#[derive(Debug, Deserialize)]
struct MetadataPackage {
    /// The resolved id, which is what `resolve` refers to.
    id: String,
    name: String,
    manifest_path: String,
    /// Absent for a package cargo read from a path rather than a registry.
    source: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Resolve {
    nodes: Vec<ResolveNode>,
}

#[derive(Debug, Deserialize)]
struct ResolveNode {
    id: String,
    dependencies: Vec<String>,
}

/// One local package: where it lives and which local packages its tests reach.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    pub name: String,
    pub dir: PathBuf,
    /// The workspace root the shared configuration is read from.
    pub workspace_root: PathBuf,
    /// This package and every local package reachable from it, keyed by name.
    pub closure: BTreeMap<String, PathBuf>,
    /// Why some of the package's inputs cannot be fingerprinted, when they cannot.
    pub opaque: Option<String>,
}

/// Every local package, keyed by name, with its local dependency closure.
///
/// A path dependency is a package with no registry source, whether or not it is a
/// workspace member, so one outside the workspace still joins the closure of
/// whoever depends on it. A dependency with a registry source is not local: its
/// contents cannot change without `Cargo.lock` changing, which the fingerprint
/// already covers.
///
/// # Errors
///
/// Returns when `cargo metadata` fails or its output is not package records.
pub fn packages(root: &Path, exec: &dyn Exec) -> Result<BTreeMap<String, Package>> {
    let output = exec.run("cargo", &metadata_args(), root)?;
    if !output.success() {
        return Err(Error::Metadata {
            stderr: output.stderr.trim().to_owned(),
        });
    }
    let metadata: Metadata =
        serde_json::from_str(&output.stdout).map_err(|source| Error::MetadataJson { source })?;
    let local: BTreeMap<&str, &MetadataPackage> = metadata
        .packages
        .iter()
        .filter(|package| package.source.is_none())
        .map(|package| (package.id.as_str(), package))
        .collect();
    let mut packages = BTreeMap::new();
    let all: BTreeMap<String, PathBuf> = local
        .values()
        .filter_map(|package| {
            Path::new(&package.manifest_path)
                .parent()
                .map(|dir| (package.name.clone(), dir.to_path_buf()))
        })
        .collect();
    let whole = tests_whole_workspace(Path::new(&metadata.workspace_root));
    for package in metadata.packages.iter().filter(|p| p.source.is_none()) {
        let Some(dir) = Path::new(&package.manifest_path).parent() else {
            continue;
        };
        let closure = match &metadata.resolve {
            Some(resolve) if !whole => reachable(&package.id, resolve, &local),
            Some(_) => all.clone(),
            None => BTreeMap::from([(package.name.clone(), dir.to_path_buf())]),
        };
        packages.insert(
            package.name.clone(),
            Package {
                name: package.name.clone(),
                dir: dir.to_path_buf(),
                workspace_root: PathBuf::from(&metadata.workspace_root),
                closure,
                opaque: metadata
                    .resolve
                    .is_none()
                    .then(|| "cargo metadata reported no resolved dependency graph".to_owned()),
            },
        );
    }
    Ok(packages)
}

/// Whether `.cargo/mutants.toml` may make a run test more than the mutated package.
///
/// `cargo-mutants` otherwise tests only the mutated package, which is what lets a
/// fingerprint stop at the local dependency closure. `test_workspace` widens that
/// to every workspace test, so the key being present at all puts the whole local
/// set in every closure. Reading it shallowly avoids a TOML dependency, and it errs
/// toward a cache miss: a key set to `false` only costs a rerun.
fn tests_whole_workspace(root: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(root.join(MUTANTS_CONFIG)) else {
        return false;
    };
    text.lines()
        .map(|line| line.split('#').next().unwrap_or_default())
        .filter_map(|line| line.split_once('='))
        .any(|(key, _)| key.trim() == "test_workspace")
}

/// The directories of `id` and every local package its tests can reach.
fn reachable(
    id: &str,
    resolve: &Resolve,
    local: &BTreeMap<&str, &MetadataPackage>,
) -> BTreeMap<String, PathBuf> {
    let nodes: BTreeMap<&str, &ResolveNode> = resolve
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node))
        .collect();
    let mut seen = BTreeSet::new();
    let mut queue = vec![id];
    while let Some(id) = queue.pop() {
        if !seen.insert(id) {
            continue;
        }
        if let Some(node) = nodes.get(id) {
            queue.extend(node.dependencies.iter().map(String::as_str));
        }
    }
    seen.into_iter()
        .filter_map(|id| local.get(id))
        .filter_map(|package| {
            Path::new(&package.manifest_path)
                .parent()
                .map(|dir| (package.name.clone(), dir.to_path_buf()))
        })
        .collect()
}

/// Hash one package's fingerprint inputs.
///
/// # Errors
///
/// Returns when a file cannot be read.
pub fn fingerprint(
    root: &Path,
    package: &str,
    files: &[PathBuf],
    cargo_mutants: &str,
) -> Result<Fingerprint> {
    let mut hashes = Vec::with_capacity(files.len());
    for path in files {
        let content = std::fs::read(path).map_err(|source| Error::Read {
            path: path.clone(),
            source,
        })?;
        hashes.push(FileHash {
            path: relative(root, path),
            hash: Sha256::digest(&content).into(),
        });
    }
    Ok(Fingerprint::compute(
        package,
        &hashes,
        cargo_mutants,
        this_version(),
    ))
}

/// A path named relative to the project root, so a checkout move does not change it.
fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// The relative path cargo-mutants uses for a file, for reporting.
#[must_use]
pub fn strip_root<'a>(root: &Path, path: &'a Path) -> &'a Path {
    path.strip_prefix(root).unwrap_or(path)
}
#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn file(path: &str, byte: u8) -> FileHash {
        FileHash {
            path: path.to_owned(),
            hash: [byte; 32],
        }
    }

    #[test]
    fn a_store_round_trips() {
        let mut store = Store::empty("mutator", "27.1.0");
        store.entries_mut().insert(
            "a".to_owned(),
            Entry {
                package: "mutator".to_owned(),
                caught: true,
            },
        );
        assert_eq!(Store::parse(&store.render().unwrap()).unwrap(), store);
    }

    #[test]
    fn another_version_is_not_trusted() {
        assert!(
            Store::parse(r#"{"version":2,"package":"p","cargo_mutants":"1","entries":{}}"#)
                .is_err()
        );
    }

    #[test]
    fn a_store_from_another_package_or_tool_is_refused() {
        let store = Store::empty("mutator", "cargo-mutants 27.1.0");
        assert!(store.check("mutator", "cargo-mutants 27.1.0").is_ok());
        let wrong_package = store.check("other", "cargo-mutants 27.1.0").unwrap_err();
        assert!(
            wrong_package.to_string().contains("other"),
            "{wrong_package}"
        );
        let wrong_tool = store.check("mutator", "cargo-mutants 28.0.0").unwrap_err();
        assert!(wrong_tool.to_string().contains("28.0.0"), "{wrong_tool}");
    }

    #[test]
    fn the_version_helpers_report_what_the_run_compares() {
        assert_eq!(version_args(), ["mutants", "--version"]);
        assert_eq!(this_version(), env!("CARGO_PKG_VERSION"));
        assert_ne!(this_version(), "");
    }

    #[test]
    fn the_cargo_mutants_version_is_read_from_its_banner() {
        struct Banner(&'static str);
        impl Exec for Banner {
            fn run(
                &self,
                _program: &str,
                _args: &[String],
                _cwd: &Path,
            ) -> Result<crate::exec::Output> {
                Ok(crate::exec::Output {
                    code: 0,
                    stdout: self.0.to_owned(),
                    stderr: String::new(),
                })
            }
        }
        let version =
            cargo_mutants_version(&Banner("cargo-mutants 27.1.0\n"), Path::new(".")).unwrap();
        assert_eq!(version, "cargo-mutants 27.1.0");
    }

    #[test]
    fn a_failing_version_probe_is_an_error() {
        struct Broken;
        impl Exec for Broken {
            fn run(
                &self,
                _program: &str,
                _args: &[String],
                _cwd: &Path,
            ) -> Result<crate::exec::Output> {
                Ok(crate::exec::Output {
                    code: 1,
                    stdout: String::new(),
                    stderr: "not installed".to_owned(),
                })
            }
        }
        let err = cargo_mutants_version(&Broken, Path::new(".")).unwrap_err();
        assert!(err.to_string().contains("not installed"), "{err}");
    }

    #[test]
    fn a_missing_file_is_no_hits() {
        let root = std::env::temp_dir().join("mutator-cache-missing");
        let fingerprint = Fingerprint::compute("p", &[], "1", "1");
        let hits = load(&root, "p", &fingerprint, "1");
        assert_eq!(hits, Hits::default());
        assert_eq!(hits.len(), 0);
        assert!(hits.is_empty());
    }

    #[test]
    fn save_then_load_returns_the_entries() {
        let root = std::env::temp_dir().join("mutator-cache-save");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let fingerprint = Fingerprint::compute("p", &[file("src/lib.rs", 7)], "27.1.0", "1");
        let entry = Entry {
            package: "p".to_owned(),
            caught: true,
        };
        let mut packages = BTreeMap::new();
        packages.insert("p".to_owned(), (fingerprint, vec![("m".to_owned(), entry)]));
        save_all(&root, &packages, "27.1.0").unwrap();
        let hits = load(&root, "p", &fingerprint, "27.1.0");
        assert!(hits.killed("m"));
        assert_eq!(hits.len(), 1);
        assert!(!hits.is_empty());
        // A different cargo-mutants version invalidates the entry.
        assert!(load(&root, "p", &fingerprint, "28.0.0").is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_killed_verdict_is_the_only_hit() {
        let mut hits = Hits::default();
        hits.insert(
            "gone",
            Entry {
                package: "p".to_owned(),
                caught: false,
            },
        );
        hits.insert(
            "dead",
            Entry {
                package: "p".to_owned(),
                caught: true,
            },
        );
        assert!(hits.killed("dead"));
        assert!(!hits.killed("gone"));
        assert!(!hits.killed("unknown"));
        assert!(hits.seen("gone"));
        assert!(!hits.seen("unknown"));
        assert_eq!(hits.len(), 2);
        assert_eq!(hits.entries().len(), 2);
    }

    #[test]
    fn a_package_name_becomes_a_safe_directory() {
        assert_eq!(sanitize("serde_json"), "serde_json");
        assert_eq!(sanitize("a/b\\c d"), "a-b-c-d");
    }

    #[test]
    fn a_package_contributes_its_sources_and_manifests() {
        let dir = std::env::temp_dir().join("mutator-cache-sources");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("tests")).unwrap();
        std::fs::write(dir.join("src/lib.rs"), "").unwrap();
        std::fs::write(dir.join("tests/t.rs"), "").unwrap();
        std::fs::write(dir.join("build.rs"), "").unwrap();
        std::fs::write(dir.join("Cargo.toml"), "").unwrap();
        std::fs::write(dir.join("README.md"), "").unwrap();
        let package = Package {
            name: "p".to_owned(),
            dir: dir.clone(),
            workspace_root: dir.clone(),
            closure: BTreeMap::from([("p".to_owned(), dir.clone())]),
            opaque: None,
        };
        let files = fingerprint_sources(&package);
        let names: Vec<String> = files
            .iter()
            .map(|path| path.strip_prefix(&dir).unwrap().display().to_string())
            .collect();
        assert_eq!(
            names,
            vec!["Cargo.toml", "build.rs", "src/lib.rs", "tests/t.rs"],
            "the build script and manifest count, a README does not"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_shared_configuration_comes_from_the_workspace_root() {
        let root = std::env::temp_dir().join("mutator-cache-shared");
        let member = root.join("crates/app");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(member.join("src")).unwrap();
        for name in SHARED {
            let path = root.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
        }
        let package = Package {
            name: "p".to_owned(),
            dir: member.clone(),
            workspace_root: root.clone(),
            closure: BTreeMap::from([("p".to_owned(), member)]),
            opaque: None,
        };
        let files = fingerprint_sources(&package);
        for name in SHARED {
            assert!(
                files.contains(&root.join(name)),
                "{name} beside the member counts, not one beside the package"
            );
        }
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// `cargo metadata` output for one package, with no dependencies.
    fn metadata_one(package: &str, dir: &Path) -> String {
        let manifest = dir.join("Cargo.toml");
        format!(
            r#"{{"packages":[{{"id":"path+file://{}#0.1.0","name":"{package}",
                "source":null,"manifest_path":"{}"}}],
                "resolve":{{"nodes":[{{"id":"path+file://{}#0.1.0","dependencies":[]}}]}},
                "workspace_root":"{}"}}"#,
            dir.display(),
            manifest.display(),
            dir.display(),
            dir.display()
        )
    }

    /// Serves one fixed `cargo metadata` document.
    struct Fixed(String);

    impl Exec for Fixed {
        fn run(
            &self,
            _program: &str,
            _args: &[String],
            _cwd: &Path,
        ) -> Result<crate::exec::Output> {
            Ok(crate::exec::Output {
                code: 0,
                stdout: self.0.clone(),
                stderr: String::new(),
            })
        }
    }

    #[test]
    fn a_renamed_file_moves_the_fingerprint() {
        let root = std::env::temp_dir().join("mutator-cache-relative");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), "pub fn f() {}\n").unwrap();
        let before = fingerprint(&root, "p", &[root.join("src/a.rs")], "27.1.0").unwrap();

        std::fs::rename(root.join("src/a.rs"), root.join("src/b.rs")).unwrap();
        let after = fingerprint(&root, "p", &[root.join("src/b.rs")], "27.1.0").unwrap();
        assert_ne!(
            before, after,
            "the same content under another name is another file"
        );

        let moved = std::env::temp_dir().join("mutator-cache-relative-moved");
        let _ = std::fs::remove_dir_all(&moved);
        std::fs::create_dir_all(moved.join("src")).unwrap();
        std::fs::write(moved.join("src/b.rs"), "pub fn f() {}\n").unwrap();
        assert_eq!(
            after,
            fingerprint(&moved, "p", &[moved.join("src/b.rs")], "27.1.0").unwrap(),
            "moving the checkout must not invalidate the cache"
        );

        std::fs::remove_dir_all(&root).unwrap();
        std::fs::remove_dir_all(&moved).unwrap();
    }

    #[test]
    fn a_package_with_no_dependencies_closes_over_itself() {
        let root = std::env::temp_dir().join("mutator-cache-closure-self");
        let packages = packages(&root, &Fixed(metadata_one("p", &root))).unwrap();
        let package = &packages["p"];
        assert_eq!(package.closure, BTreeMap::from([("p".to_owned(), root)]));
        assert_eq!(package.opaque, None);
    }

    #[test]
    fn a_local_dependency_joins_the_closure() {
        let root = std::env::temp_dir().join("mutator-cache-closure-dep");
        let dep = root.join("dep");
        let metadata = format!(
            r#"{{"packages":[
                {{"id":"path+file://{root}#0.1.0","name":"p","source":null,
                  "manifest_path":"{root}/Cargo.toml"}},
                {{"id":"path+file://{dep}#0.1.0","name":"dep","source":null,
                  "manifest_path":"{dep}/Cargo.toml"}}],
               "resolve":{{"nodes":[
                {{"id":"path+file://{root}#0.1.0",
                  "dependencies":["path+file://{dep}#0.1.0"]}},
                {{"id":"path+file://{dep}#0.1.0","dependencies":[]}}]}},
               "workspace_root":"{root}"}}"#,
            root = root.display(),
            dep = dep.display()
        );
        let packages = packages(&root, &Fixed(metadata)).unwrap();
        assert_eq!(
            packages["p"].closure,
            BTreeMap::from([("p".to_owned(), root), ("dep".to_owned(), dep)]),
            "a path dependency's contents decide p's tests"
        );
    }

    #[test]
    fn a_dependency_of_a_dependency_joins_the_closure_too() {
        let root = std::env::temp_dir().join("mutator-cache-closure-deep");
        let mid = root.join("mid");
        let leaf = root.join("leaf");
        let metadata = format!(
            r#"{{"packages":[
                {{"id":"path+file://{root}#0.1.0","name":"p","source":null,
                  "manifest_path":"{root}/Cargo.toml"}},
                {{"id":"path+file://{mid}#0.1.0","name":"mid","source":null,
                  "manifest_path":"{mid}/Cargo.toml"}},
                {{"id":"path+file://{leaf}#0.1.0","name":"leaf","source":null,
                  "manifest_path":"{leaf}/Cargo.toml"}}],
               "resolve":{{"nodes":[
                {{"id":"path+file://{root}#0.1.0",
                  "dependencies":["path+file://{mid}#0.1.0"]}},
                {{"id":"path+file://{mid}#0.1.0",
                  "dependencies":["path+file://{leaf}#0.1.0"]}},
                {{"id":"path+file://{leaf}#0.1.0","dependencies":[]}}]}},
               "workspace_root":"{root}"}}"#,
            root = root.display(),
            mid = mid.display(),
            leaf = leaf.display()
        );
        let packages = packages(&root, &Fixed(metadata)).unwrap();
        assert_eq!(
            packages["p"].closure,
            BTreeMap::from([
                ("p".to_owned(), root.clone()),
                ("mid".to_owned(), mid.clone()),
                ("leaf".to_owned(), leaf),
            ]),
            "cargo links the whole chain, so a leaf edit changes p's verdicts"
        );
        assert_eq!(
            packages["mid"].closure,
            BTreeMap::from([
                ("mid".to_owned(), mid),
                ("leaf".to_owned(), root.join("leaf"))
            ]),
            "mid does not contain p, which depends on it"
        );
    }

    #[test]
    fn a_dependency_cycle_terminates() {
        let root = std::env::temp_dir().join("mutator-cache-closure-cycle");
        let other = root.join("other");
        let metadata = format!(
            r#"{{"packages":[
                {{"id":"path+file://{root}#0.1.0","name":"p","source":null,
                  "manifest_path":"{root}/Cargo.toml"}},
                {{"id":"path+file://{other}#0.1.0","name":"other","source":null,
                  "manifest_path":"{other}/Cargo.toml"}}],
               "resolve":{{"nodes":[
                {{"id":"path+file://{root}#0.1.0",
                  "dependencies":["path+file://{other}#0.1.0"]}},
                {{"id":"path+file://{other}#0.1.0",
                  "dependencies":["path+file://{root}#0.1.0"]}}]}},
               "workspace_root":"{root}"}}"#,
            root = root.display(),
            other = other.display()
        );
        let packages = packages(&root, &Fixed(metadata)).unwrap();
        assert_eq!(
            packages["p"].closure,
            BTreeMap::from([("p".to_owned(), root), ("other".to_owned(), other)]),
            "a cycle visits each package once"
        );
    }

    #[test]
    fn a_registry_dependency_stays_out_of_the_closure() {
        let root = std::env::temp_dir().join("mutator-cache-closure-remote");
        let metadata = format!(
            r#"{{"packages":[
                {{"id":"path+file://{root}#0.1.0","name":"p","source":null,
                  "manifest_path":"{root}/Cargo.toml"}},
                {{"id":"registry+https://github.com/rust-lang/crates.io-index#serde@1.0.0",
                  "name":"serde","source":"registry+https://github.com/rust-lang/crates.io-index",
                  "manifest_path":"/cache/serde/Cargo.toml"}}],
               "resolve":{{"nodes":[
                {{"id":"path+file://{root}#0.1.0",
                  "dependencies":["registry+https://github.com/rust-lang/crates.io-index#serde@1.0.0"]}},
                {{"id":"registry+https://github.com/rust-lang/crates.io-index#serde@1.0.0",
                  "dependencies":[]}}]}},
               "workspace_root":"{root}"}}"#,
            root = root.display()
        );
        let packages = packages(&root, &Fixed(metadata)).unwrap();
        assert_eq!(
            packages["p"].closure,
            BTreeMap::from([("p".to_owned(), root)]),
            "a registry package cannot change without Cargo.lock changing"
        );
        assert_eq!(packages.get("serde"), None, "only local packages are keyed");
    }

    #[test]
    fn a_missing_resolved_graph_makes_every_package_opaque() {
        let root = std::env::temp_dir().join("mutator-cache-opaque");
        let metadata = format!(
            r#"{{"packages":[{{"id":"path+file://{root}#0.1.0","name":"p",
                "source":null,"manifest_path":"{root}/Cargo.toml"}}],"resolve":null,
                "workspace_root":"{root}"}}"#,
            root = root.display()
        );
        let packages = packages(&root, &Fixed(metadata)).unwrap();
        assert!(
            packages["p"].opaque.is_some(),
            "no graph means no closure is trustworthy"
        );
        assert_eq!(
            packages["p"].closure,
            BTreeMap::from([("p".to_owned(), root)]),
            "the package itself is still fingerprinted"
        );
    }

    #[test]
    fn the_whole_local_set_is_in_every_closure_when_the_config_tests_the_workspace() {
        let root = std::env::temp_dir().join("mutator-cache-workspace-tests");
        let _ = std::fs::remove_dir_all(&root);
        let dep = root.join("dep");
        std::fs::create_dir_all(root.join(".cargo")).unwrap();
        std::fs::write(
            root.join(MUTANTS_CONFIG),
            "# a comment\ntest_workspace = true\n",
        )
        .unwrap();
        let metadata = format!(
            r#"{{"packages":[
                {{"id":"path+file://{root}#0.1.0","name":"p","source":null,
                  "manifest_path":"{root}/Cargo.toml"}},
                {{"id":"path+file://{dep}#0.1.0","name":"dep","source":null,
                  "manifest_path":"{dep}/Cargo.toml"}}],
               "resolve":{{"nodes":[
                {{"id":"path+file://{root}#0.1.0","dependencies":[]}},
                {{"id":"path+file://{dep}#0.1.0","dependencies":[]}}]}},
               "workspace_root":"{root}"}}"#,
            root = root.display(),
            dep = dep.display()
        );
        let packages = packages(&root, &Fixed(metadata)).unwrap();
        assert_eq!(
            packages["p"].closure,
            BTreeMap::from([("p".to_owned(), root.clone()), ("dep".to_owned(), dep),]),
            "every workspace test observes p, so dep's verdicts can change p's"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_config_comment_naming_the_key_does_not_widen_a_closure() {
        let root = std::env::temp_dir().join("mutator-cache-comment");
        let _ = std::fs::remove_dir_all(&root);
        let dep = root.join("dep");
        std::fs::create_dir_all(root.join(".cargo")).unwrap();
        std::fs::write(root.join(MUTANTS_CONFIG), "# test_workspace is not set\n").unwrap();
        let metadata = format!(
            r#"{{"packages":[
                {{"id":"path+file://{root}#0.1.0","name":"p","source":null,
                  "manifest_path":"{root}/Cargo.toml"}},
                {{"id":"path+file://{dep}#0.1.0","name":"dep","source":null,
                  "manifest_path":"{dep}/Cargo.toml"}}],
               "resolve":{{"nodes":[
                {{"id":"path+file://{root}#0.1.0","dependencies":[]}},
                {{"id":"path+file://{dep}#0.1.0","dependencies":[]}}]}},
               "workspace_root":"{root}"}}"#,
            root = root.display(),
            dep = dep.display()
        );
        let packages = packages(&root, &Fixed(metadata)).unwrap();
        assert_eq!(
            packages["p"].closure,
            BTreeMap::from([("p".to_owned(), root.clone())]),
            "a package p does not depend on stays out"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    fn arb_files() -> impl Strategy<Value = Vec<FileHash>> {
        prop::collection::vec(("[a-z/]{1,12}", any::<u8>()), 0..6).prop_map(|entries| {
            let mut files: Vec<FileHash> = entries
                .into_iter()
                .map(|(path, byte)| file(&path, byte))
                .collect();
            files.sort();
            files
        })
    }

    proptest! {
        #[test]
        fn the_fingerprint_is_a_function_of_the_inputs(files in arb_files(), version in "[0-9.]{1,8}") {
            let a = Fingerprint::compute("p", &files, &version, "1");
            let b = Fingerprint::compute("p", &files, &version, "1");
            prop_assert_eq!(a, b);
            prop_assert_eq!(a.to_hex().len(), 64);
        }

        #[test]
        fn any_content_change_moves_the_fingerprint(
            files in arb_files(),
            index in any::<prop::sample::Index>(),
        ) {
            prop_assume!(!files.is_empty());
            let mut changed = files.clone();
            let position = index.index(changed.len());
            let target = &changed[position];
            let replacement = FileHash {
                path: target.path.clone(),
                hash: [target.hash[0].wrapping_add(1); 32],
            };
            changed[position] = replacement;
            changed.sort();
            // A file's hash only matters through its own path, so a change that
            // leaves the (path, hash) pairs identical is a no-op.
            prop_assume!(changed != files);
            prop_assert_ne!(
                Fingerprint::compute("p", &files, "27.1.0", "1"),
                Fingerprint::compute("p", &changed, "27.1.0", "1"),
            );
        }

        #[test]
        fn a_path_moves_the_fingerprint(
            hash in any::<u8>(),
            name in "[a-z]{1,8}",
        ) {
            prop_assert_ne!(
                Fingerprint::compute("p", &[file(&name, hash)], "27.1.0", "1"),
                Fingerprint::compute("p", &[file(&format!("x/{name}"), hash)], "27.1.0", "1"),
            );
        }

        #[test]
        fn the_cargo_mutants_version_moves_the_fingerprint(files in arb_files()) {
            prop_assert_ne!(
                Fingerprint::compute("p", &files, "27.1.0", "1"),
                Fingerprint::compute("p", &files, "27.1.1", "1"),
            );
        }
    }
}
