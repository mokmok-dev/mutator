//! The accepted-equivalent store, committed with the project.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::mutant::MutantId;

pub const FILE: &str = "mutator.accepted.json";

const VERSION: u32 = 1;

/// `mutator.accepted.json`: ids with no input that can observe them, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Accepted {
    version: u32,
    accepted: BTreeMap<String, Entry>,
}

impl Default for Accepted {
    fn default() -> Self {
        Self {
            version: VERSION,
            accepted: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Entry {
    reason: String,
}

impl Accepted {
    /// # Errors
    ///
    /// Returns when the text is not a version 1 store.
    pub fn parse(text: &str) -> Result<Self> {
        let parsed: Self =
            serde_json::from_str(text).map_err(|source| Error::Accepted { file: FILE, source })?;
        if parsed.version != VERSION {
            return Err(Error::AcceptedVersion {
                file: FILE,
                version: parsed.version,
            });
        }
        Ok(parsed)
    }

    /// The committed file text, with sorted ids and a trailing newline.
    ///
    /// # Errors
    ///
    /// Returns when the store cannot be serialized.
    pub fn render(&self) -> Result<String> {
        let mut text = serde_json::to_string_pretty(self)
            .map_err(|source| Error::Serialize { what: FILE, source })?;
        text.push('\n');
        Ok(text)
    }

    #[must_use]
    pub fn is_accepted(&self, id: &MutantId) -> bool {
        self.accepted.contains_key(id.as_str())
    }

    /// Record an id, returning the reason it had before.
    pub fn insert(&mut self, id: MutantId, reason: String) -> Option<String> {
        self.accepted
            .insert(String::from(id), Entry { reason })
            .map(|entry| entry.reason)
    }

    /// Read the store under `root`. A missing file is an empty store.
    ///
    /// # Errors
    ///
    /// Returns when the file exists but cannot be read or parsed.
    pub fn load(root: &Path) -> Result<Self> {
        let path = root.join(FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) => Self::parse(&text),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(Error::Read { path, source }),
        }
    }

    /// Write the store under `root`.
    ///
    /// # Errors
    ///
    /// Returns when the file cannot be written.
    pub fn save(&self, root: &Path) -> Result<()> {
        let path = root.join(FILE);
        std::fs::write(&path, self.render()?).map_err(|source| Error::Write { path, source })
    }
}

/// Accepted ids with no matching live mutant, sorted.
#[must_use]
pub fn stale<'a>(accepted: &'a Accepted, live: &BTreeSet<String>) -> Vec<(&'a str, &'a str)> {
    accepted
        .accepted
        .iter()
        .filter(|(id, _)| !live.contains(id.as_str()))
        .map(|(id, entry)| (id.as_str(), entry.reason.as_str()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn id(text: &str) -> MutantId {
        MutantId::new(text, "f", "FnValue", "0", 0)
    }

    fn reason_map(pairs: Vec<(String, String)>) -> Accepted {
        let accepted = pairs
            .into_iter()
            .map(|(key, reason)| (key, Entry { reason }))
            .collect();
        Accepted {
            version: VERSION,
            accepted,
        }
    }

    #[test]
    fn parses_and_renders_a_store() {
        let text = r#"{"version":1,"accepted":{"a::f::FnValue::0#0":{"reason":"always zero"}}}"#;
        let accepted = Accepted::parse(text).unwrap();
        assert!(accepted.is_accepted(&id("a")));
        assert_eq!(
            Accepted::parse(&accepted.render().unwrap()).unwrap(),
            accepted
        );
    }

    #[test]
    fn rejects_other_versions() {
        let err = Accepted::parse(r#"{"version":2,"accepted":{}}"#).unwrap_err();
        assert!(err.to_string().contains("version 2"));
    }

    #[test]
    fn insert_returns_the_previous_reason() {
        let mut accepted = Accepted::default();
        assert_eq!(accepted.insert(id("a"), "one".into()), None);
        assert_eq!(
            accepted.insert(id("a"), "two".into()),
            Some("one".to_owned())
        );
        assert_eq!(
            accepted,
            reason_map(vec![("a::f::FnValue::0#0".to_owned(), "two".to_owned())])
        );
    }

    #[test]
    fn a_missing_file_is_an_empty_store() {
        let dir = std::env::temp_dir().join("mutator-accept-does-not-exist");
        assert_eq!(Accepted::load(&dir).unwrap(), Accepted::default());
    }

    #[test]
    fn an_unreadable_path_is_an_error_not_an_empty_store() {
        let dir = std::env::temp_dir().join("mutator-accept-directory");
        let path = dir.join(FILE);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&path).unwrap();
        assert!(Accepted::load(&dir).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = std::env::temp_dir().join("mutator-accept-save");
        std::fs::create_dir_all(&dir).unwrap();
        let mut accepted = Accepted::default();
        accepted.insert(id("a"), "why".into());
        accepted.save(&dir).unwrap();
        assert_eq!(Accepted::load(&dir).unwrap(), accepted);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn stale_lists_only_ids_without_live_mutants() {
        let accepted = reason_map(vec![
            ("a::f::FnValue::0#0".to_owned(), "x".to_owned()),
            ("a::f::FnValue::0#1".to_owned(), "y".to_owned()),
        ]);
        let live = BTreeSet::from(["a::f::FnValue::0#0".to_owned()]);
        assert_eq!(stale(&accepted, &live), vec![("a::f::FnValue::0#1", "y")]);
    }

    fn arb_key() -> impl Strategy<Value = String> {
        "[a-zA-Z0-9_:#./<> -é日]{0,40}"
    }

    proptest! {
        #[test]
        fn render_then_parse_is_identity(pairs in prop::collection::vec((arb_key(), arb_key()), 0..8)) {
            let accepted = reason_map(pairs);
            prop_assert_eq!(Accepted::parse(&accepted.render().unwrap()).unwrap(), accepted);
        }

        #[test]
        fn stale_holds_exactly_the_ids_absent_from_live(
            keys in prop::collection::btree_set(arb_key(), 0..8),
            live_key in arb_key(),
        ) {
            let accepted = reason_map(
                keys.iter()
                    .map(|key| (String::from(id(key)), key.clone()))
                    .collect(),
            );
            let live = BTreeSet::from([String::from(id(&live_key))]);
            let stale_ids: Vec<&str> = stale(&accepted, &live)
                .into_iter()
                .map(|(id, _)| id)
                .collect();
            let live_id = format!("{live_key}::f::FnValue::0#0");
            let mut expected: Vec<String> = keys
                .iter()
                .map(|key| format!("{key}::f::FnValue::0#0"))
                .filter(|id| *id != live_id)
                .collect();
            expected.sort();
            prop_assert_eq!(
                stale_ids,
                expected.iter().map(String::as_str).collect::<Vec<_>>()
            );
        }
    }
}
