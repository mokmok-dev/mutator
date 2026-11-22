//! Verdict reuse keyed by a digest of the package the mutant lives in.
//!
//! `cargo-mutants` reruns a package's whole test suite for every mutant, so a
//! mutant's verdict depends on every source and test file of that package. The
//! key is therefore the content of those files: a change anywhere in the package
//! invalidates the entry, and an unchanged package hits.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::exec::Exec;

pub const DIR: &str = "target/mutator-cache";

const VERSION: u32 = 1;

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

/// A file's path and content hash, as `git hash-object` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct FileHash {
    pub path_index: usize,
    pub hash: [u8; 32],
}

/// The digest that decides whether a cached verdict still applies.
///
/// `sources` are every source and test file of the package, sorted by path, with
/// the tool versions that change what a verdict means.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Fingerprint([u8; 32]);

impl Fingerprint {
    #[must_use]
    pub fn compute(package: &str, sources: &[FileHash], cargo_mutants: &str, this: &str) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(format!("v{VERSION}\0{package}\0{cargo_mutants}\0{this}\0"));
        for FileHash { path_index, hash } in sources {
            hasher.update(path_index.to_string().as_bytes());
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

/// The files whose content decides a package's test outcome: sources, tests,
/// benches, examples, and the manifests and lockfile above them.
#[must_use]
pub fn fingerprint_sources(root: &Path, package_dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for sub in ["src", "tests", "benches", "examples"] {
        collect(&package_dir.join(sub), &mut files);
    }
    let manifest = package_dir.join("Cargo.toml");
    if manifest.is_file() {
        files.push(manifest);
    }
    for name in ["Cargo.lock", "Cargo.toml"] {
        let shared = root.join(name);
        if shared.is_file() {
            files.push(shared);
        }
    }
    files.sort();
    files.dedup();
    files
}

/// Every `.rs` file under `dir`, recursively.
fn collect(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, files);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            files.push(path);
        }
    }
}

/// `cargo`'s argv for listing the workspace packages and their manifest paths.
#[must_use]
pub fn metadata_args() -> Vec<String> {
    vec![
        "metadata".to_owned(),
        "--format-version".to_owned(),
        "1".to_owned(),
        "--no-deps".to_owned(),
    ]
}

/// The package directory for each workspace member, keyed by package name.
#[derive(Debug, Deserialize)]
struct Metadata {
    packages: Vec<MetadataPackage>,
}

#[derive(Debug, Deserialize)]
struct MetadataPackage {
    name: String,
    manifest_path: String,
}

/// Map package name to directory, from `cargo metadata`.
///
/// # Errors
///
/// Returns when `cargo metadata` fails or its output is not package records.
pub fn package_dirs(root: &Path, exec: &dyn Exec) -> Result<BTreeMap<String, PathBuf>> {
    let output = exec.run("cargo", &metadata_args(), root)?;
    if !output.success() {
        return Err(Error::Metadata {
            stderr: output.stderr.trim().to_owned(),
        });
    }
    let metadata: Metadata =
        serde_json::from_str(&output.stdout).map_err(|source| Error::MetadataJson { source })?;
    Ok(metadata
        .packages
        .into_iter()
        .filter_map(|package| {
            PathBuf::from(package.manifest_path)
                .parent()
                .map(|dir| (package.name, dir.to_path_buf()))
        })
        .collect())
}

/// Hash one package's fingerprint inputs.
///
/// # Errors
///
/// Returns when a file cannot be read.
pub fn fingerprint(package: &str, files: &[PathBuf], cargo_mutants: &str) -> Result<Fingerprint> {
    let mut hashes = Vec::with_capacity(files.len());
    for (index, path) in files.iter().enumerate() {
        let content = std::fs::read(path).map_err(|source| Error::Read {
            path: path.clone(),
            source,
        })?;
        hashes.push(FileHash {
            path_index: index,
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

/// The relative path cargo-mutants uses for a file, for reporting.
#[must_use]
pub fn strip_root<'a>(root: &Path, path: &'a Path) -> &'a Path {
    path.strip_prefix(root).unwrap_or(path)
}
#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn file(path_index: usize, byte: u8) -> FileHash {
        FileHash {
            path_index,
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
        let fingerprint = Fingerprint::compute("p", &[file(0, 7)], "27.1.0", "1");
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
    fn sources_include_tests_and_both_manifests() {
        let dir = std::env::temp_dir().join("mutator-cache-sources");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("tests")).unwrap();
        std::fs::write(dir.join("src/lib.rs"), "").unwrap();
        std::fs::write(dir.join("tests/t.rs"), "").unwrap();
        std::fs::write(dir.join("Cargo.toml"), "").unwrap();
        std::fs::write(dir.join("README.md"), "").unwrap();
        let files = fingerprint_sources(&dir, &dir);
        let names: Vec<String> = files
            .iter()
            .map(|path| path.strip_prefix(&dir).unwrap().display().to_string())
            .collect();
        assert_eq!(names, vec!["Cargo.toml", "src/lib.rs", "tests/t.rs"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn arb_files() -> impl Strategy<Value = Vec<FileHash>> {
        prop::collection::vec((0_usize..20, any::<u8>()), 0..6).prop_map(|entries| {
            let mut files: Vec<FileHash> = entries
                .into_iter()
                .map(|(index, byte)| file(index, byte))
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
            let target = changed[position];
            let replacement = FileHash {
                path_index: target.path_index,
                hash: [target.hash[0].wrapping_add(1); 32],
            };
            changed[position] = replacement;
            changed.sort();
            // A file's hash only matters through its own path index, so a
            // change that leaves the (index, hash) pairs identical is a no-op.
            prop_assume!(changed != files);
            prop_assert_ne!(
                Fingerprint::compute("p", &files, "27.1.0", "1"),
                Fingerprint::compute("p", &changed, "27.1.0", "1"),
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
