//! The error type every public operation returns.

use std::path::PathBuf;

use thiserror::Error;

use crate::diff::ParseError;

/// Everything `mutator` can fail at, named so a caller can match on the cause.
#[derive(Debug, Error)]
pub enum Error {
    #[error("root {path}: {source}")]
    Root {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("git {command} failed: {stderr}")]
    Git { command: String, stderr: String },

    #[error("git merge-base {revision} HEAD printed no commit")]
    EmptyMergeBase { revision: String },

    #[error("git diff {commit}: {source}")]
    Diff {
        commit: String,
        #[source]
        source: ParseError,
    },

    #[error("parse {path}: {source}")]
    DiffFile {
        path: PathBuf,
        #[source]
        source: ParseError,
    },

    #[error("read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("write {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("create {path}: {source}")]
    Create {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("write the report: {0}")]
    Report(std::io::Error),

    #[error("run {program}: {source}")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },

    #[error("cannot select an empty set of mutants")]
    EmptySelection,

    #[error("cargo mutants --list failed with exit {code}: {stderr}")]
    List { code: i32, stderr: String },

    #[error("parse cargo-mutants --list JSON: {source}")]
    ListJson {
        #[source]
        source: serde_json::Error,
    },

    #[error("cargo mutants failed with exit {code}: {stderr}")]
    Run { code: i32, stderr: String },

    #[error("parse outcomes.json: {source}")]
    Outcomes {
        #[source]
        source: serde_json::Error,
    },

    #[error("unknown cargo-mutants summary {summary:?}")]
    UnknownSummary { summary: String },

    #[error("render {what}: {source}")]
    Serialize {
        what: &'static str,
        #[source]
        source: serde_json::Error,
    },

    #[error("parse {file}: {source}")]
    Accepted {
        file: &'static str,
        #[source]
        source: serde_json::Error,
    },

    #[error("{file}: unsupported version {version}")]
    AcceptedVersion { file: &'static str, version: u32 },

    #[error("parse cache entry: {source}")]
    Cache {
        #[source]
        source: serde_json::Error,
    },

    #[error("cache entry has unsupported version {version}")]
    CacheVersion { version: u32 },

    #[error("cache entry is for package {found}, not {expected}")]
    CachePackage { found: String, expected: String },

    #[error("cache entry is for {found}, not {expected}")]
    CacheTool { found: String, expected: String },

    #[error("cargo mutants --version failed: {stderr}")]
    Version { stderr: String },

    #[error("cargo metadata failed: {stderr}")]
    Metadata { stderr: String },

    #[error("parse cargo metadata: {source}")]
    MetadataJson {
        #[source]
        source: serde_json::Error,
    },

    #[error("--reason must say why no input can observe the mutant")]
    EmptyReason,

    #[error("no mutant with id {id}; run `mutator --list --all` for the ids in the project")]
    UnknownMutant { id: String },
}

/// Every fallible operation returns this.
pub type Result<T> = std::result::Result<T, Error>;

impl From<std::io::Error> for Error {
    fn from(source: std::io::Error) -> Self {
        Self::Report(source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_names_the_path_and_the_cause() {
        let error = Error::Read {
            path: PathBuf::from("src/lib.rs"),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "missing"),
        };
        assert_eq!(error.to_string(), "read src/lib.rs: missing");
    }

    #[test]
    fn a_source_is_reachable_without_parsing_the_message() {
        let error = Error::Root {
            path: PathBuf::from("/nope"),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "gone"),
        };
        let source = std::error::Error::source(&error).unwrap();
        assert_eq!(source.to_string(), "gone");
    }

    #[test]
    fn a_parse_error_keeps_its_line() {
        let error = Error::DiffFile {
            path: PathBuf::from("change.diff"),
            source: ParseError {
                line: 7,
                text: "@@ broken".to_owned(),
            },
        };
        assert_eq!(error.to_string(), "parse change.diff: line 7: @@ broken");
    }

    #[test]
    fn an_io_error_becomes_a_report_error() {
        let error = Error::from(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "closed",
        ));
        assert!(matches!(error, Error::Report(_)));
        assert_eq!(error.to_string(), "write the report: closed");
    }

    #[test]
    fn the_usage_errors_say_what_to_do() {
        assert_eq!(
            Error::UnknownMutant {
                id: "pkg::f::FnValue::0#0".to_owned()
            }
            .to_string(),
            "no mutant with id pkg::f::FnValue::0#0; run `mutator --list --all` for the ids in the project"
        );
        assert_eq!(
            Error::CacheTool {
                found: "27.1.0".to_owned(),
                expected: "28.0.0".to_owned()
            }
            .to_string(),
            "cache entry is for 27.1.0, not 28.0.0"
        );
    }
}
