//! The `cargo-mutants` mutant model, diff selection, and position-independent ids.

use std::collections::BTreeMap;
use std::fmt;

use serde::Deserialize;

use crate::diff::{Diff, Span};

/// One mutant as `cargo mutants --list --json` reports it.
#[derive(Debug, Clone, Deserialize)]
pub struct Mutant {
    pub name: String,
    pub file: String,
    pub package: String,
    pub genre: String,
    pub replacement: String,
    pub span: Span,
    pub function: Option<Function>,
}

/// The function enclosing a mutant.
#[derive(Debug, Clone, Deserialize)]
pub struct Function {
    #[serde(rename = "function_name")]
    pub name: String,
    pub span: Span,
}

impl Mutant {
    /// The span a diff must touch for this mutant to matter: its function, or the mutant itself.
    #[must_use]
    pub fn enclosing_span(&self) -> Span {
        self.function.as_ref().map_or(self.span, |f| f.span)
    }

    /// The enclosing function name, or `-` at module scope.
    #[must_use]
    pub fn function_name(&self) -> &str {
        self.function.as_ref().map_or("-", |f| f.name.as_str())
    }

    #[must_use]
    pub const fn line(&self) -> u32 {
        self.span.start.line
    }

    /// The parts of a mutant's identity that do not move with the source.
    fn key(&self) -> (&str, &str, &str, &str) {
        (
            self.file.as_str(),
            self.function_name(),
            self.genre.as_str(),
            self.replacement.as_str(),
        )
    }
}

/// A mutant's identity: its place in the code, free of line numbers.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct MutantId(String);

impl MutantId {
    /// `{file}::{function}::{genre}::{replacement}#{ordinal}`, where the ordinal counts
    /// same-shaped mutants inside one function in source order.
    #[must_use]
    pub fn new(file: &str, function: &str, genre: &str, replacement: &str, ordinal: u32) -> Self {
        Self(format!(
            "{file}::{function}::{genre}::{replacement}#{ordinal}"
        ))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for MutantId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(&self.0)
    }
}

impl From<MutantId> for String {
    fn from(id: MutantId) -> Self {
        id.0
    }
}

/// Parse `cargo mutants --list --json` output. An empty array is valid.
///
/// # Errors
///
/// Returns when the output is not the documented array of mutants.
pub fn parse_list(json: &str) -> anyhow::Result<Vec<Mutant>> {
    serde_json::from_str(json)
        .map_err(|err| anyhow::anyhow!("parse cargo-mutants --list JSON: {err}"))
}

/// Assign every mutant its id, in input order, numbering same-shaped mutants by source position.
#[must_use]
pub fn assign_ids(mutants: &[Mutant]) -> Vec<MutantId> {
    let mut ordered: Vec<usize> = (0..mutants.len()).collect();
    ordered.sort_by_key(|&index| {
        let mutant = &mutants[index];
        (
            mutant.file.clone(),
            mutant.function_name().to_owned(),
            mutant.span.start,
        )
    });
    let mut next_ordinal: BTreeMap<(&str, &str, &str, &str), u32> = BTreeMap::new();
    let mut ids = Vec::with_capacity(mutants.len());
    for index in ordered {
        let mutant = &mutants[index];
        let ordinal = next_ordinal.entry(mutant.key()).or_insert(0);
        let (file, function, genre, replacement) = mutant.key();
        ids.push((
            index,
            MutantId::new(file, function, genre, replacement, *ordinal),
        ));
        *ordinal = ordinal.saturating_add(1);
    }
    ids.sort_by_key(|(index, _)| *index);
    ids.into_iter().map(|(_, id)| id).collect()
}

/// The mutants a diff can affect, in input order.
#[must_use]
pub fn select<'a>(mutants: &'a [Mutant], diff: &Diff) -> Vec<&'a Mutant> {
    mutants
        .iter()
        .filter(|mutant| diff.touching(&mutant.file, mutant.enclosing_span()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::Pos;
    use proptest::prelude::*;

    fn span(start: (u32, u32), end: (u32, u32)) -> Span {
        Span::new(Pos::new(start.0, start.1), Pos::new(end.0, end.1))
    }

    fn mutant(
        name: &str,
        function: &str,
        genre: &str,
        replacement: &str,
        body: (u32, u32),
    ) -> Mutant {
        Mutant {
            name: name.to_owned(),
            file: "src/lib.rs".to_owned(),
            package: "mutator".to_owned(),
            genre: genre.to_owned(),
            replacement: replacement.to_owned(),
            span: span((body.0, 5), (body.0, 9)),
            function: Some(Function {
                name: function.to_owned(),
                span: span((body.0, 1), (body.1, 2)),
            }),
        }
    }

    fn fixture() -> Vec<Mutant> {
        vec![
            mutant("a", "parse", "FnValue", "0", (3, 9)),
            mutant("b", "parse", "BinaryOperator", "-", (5, 6)),
            mutant("c", "render", "FnValue", "0", (20, 24)),
        ]
    }

    #[test]
    fn parses_the_list_output() {
        let json = r#"[{"name":"src/lib.rs:2:5: replace add -> i32 with 0","file":"src/lib.rs",
            "package":"proto","genre":"FnValue","replacement":"0",
            "span":{"start":{"line":2,"column":5},"end":{"line":2,"column":10}},
            "function":{"function_name":"add","return_type":"-> i32",
                        "span":{"start":{"line":1,"column":1},"end":{"line":3,"column":2}}},
            "diff":"--- x"}]"#;
        let mutants = parse_list(json).unwrap();
        assert_eq!(mutants.len(), 1);
        assert_eq!(mutants[0].function_name(), "add");
        assert_eq!(mutants[0].enclosing_span(), span((1, 1), (3, 2)));
        assert!(parse_list("[]").unwrap().is_empty());
        assert!(parse_list("{").is_err());
    }

    #[test]
    fn the_id_displays_itself_with_any_padding_honoured() {
        let id = MutantId::new("src/lib.rs", "parse", "FnValue", "0", 2);
        let text = "src/lib.rs::parse::FnValue::0#2";
        assert_eq!(id.to_string(), text);
        assert_eq!(format!("{id:>41}"), format!("{:>41}", text));
        assert_ne!(format!("{id:>41}"), text);
        assert_eq!(String::from(id), text);
    }

    #[test]
    fn a_mutant_without_a_function_is_at_module_scope() {
        let mut only = mutant("a", "x", "FnValue", "0", (1, 2));
        only.function = None;
        let id = assign_ids(&[only]);
        assert_eq!(id[0].as_str(), "src/lib.rs::-::FnValue::0#0");
    }

    #[test]
    fn same_shaped_mutants_in_one_function_are_numbered() {
        let ids = assign_ids(&fixture());
        assert_eq!(
            ids.iter().map(MutantId::as_str).collect::<Vec<_>>(),
            vec![
                "src/lib.rs::parse::FnValue::0#0",
                "src/lib.rs::parse::BinaryOperator::-#0",
                "src/lib.rs::render::FnValue::0#0",
            ]
        );
    }

    #[test]
    fn ids_ignore_positions() {
        let shifted: Vec<Mutant> = fixture()
            .into_iter()
            .map(|mut m| {
                m.span = span((m.span.start.line + 100, 3), (m.span.end.line + 100, 3));
                m.function = m.function.map(|f| Function {
                    name: f.name,
                    span: span((f.span.start.line + 100, 1), (f.span.end.line + 100, 1)),
                });
                m
            })
            .collect();
        assert_eq!(assign_ids(&shifted), assign_ids(&fixture()));
    }

    #[test]
    fn a_new_mutant_in_another_function_keeps_existing_ids() {
        let mut extended = fixture();
        extended.push(mutant("d", "ahead", "FnValue", "0", (1, 2)));
        let (before, after) = (assign_ids(&fixture()), assign_ids(&extended));
        assert_eq!(before, after[..3]);
    }

    #[test]
    fn select_takes_mutants_whose_function_the_diff_touches() {
        let diff = Diff::parse(
            "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n\
             @@ -7,1 +7,1 @@\n-old\n+new\n",
        )
        .unwrap();
        let mutants = fixture();
        let selected = select(&mutants, &diff);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].name, "a");
    }

    #[test]
    fn select_ignores_other_files() {
        let diff = Diff::parse(
            "diff --git a/src/other.rs b/src/other.rs\n--- a/src/other.rs\n+++ b/src/other.rs\n\
             @@ -1,1 +1,1 @@\n-a\n+b\n",
        )
        .unwrap();
        assert!(select(&fixture(), &diff).is_empty());
    }

    fn arb_line() -> impl Strategy<Value = u32> {
        1_u32..200
    }

    proptest! {
        #[test]
        fn ids_are_stable_under_a_uniform_line_shift(shift in 0_u32..1000) {
            let shifted: Vec<Mutant> = fixture()
                .into_iter()
                .map(|mut m| {
                    m.span = span(
                        (m.span.start.line + shift, m.span.start.column),
                        (m.span.end.line + shift, m.span.end.column),
                    );
                    m.function = m.function.map(|f| Function {
                        name: f.name,
                        span: span(
                            (f.span.start.line + shift, f.span.start.column),
                            (f.span.end.line + shift, f.span.end.column),
                        ),
                    });
                    m
                })
                .collect();
            prop_assert_eq!(assign_ids(&shifted), assign_ids(&fixture()));
        }

        #[test]
        fn ids_are_unique_for_distinct_positions(first in arb_line(), second in arb_line()) {
            prop_assume!(first != second);
            let mutants = vec![
                mutant("a", "f", "FnValue", "0", (first, first)),
                mutant("b", "f", "FnValue", "0", (second, second)),
            ];
            let ids = assign_ids(&mutants);
            prop_assert_ne!(&ids[0], &ids[1]);
        }

        #[test]
        fn selection_matches_function_overlap(line in arb_line()) {
            let diff = Diff::parse(&format!(
                "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -{line},1 +{line},1 @@\n-a\n+b\n"
            )).unwrap();
            let mutants = fixture();
            let selected: Vec<&str> = select(&mutants, &diff).iter().map(|m| m.name.as_str()).collect();
            let expected: Vec<&str> = mutants
                .iter()
                .filter(|m| {
                    let s = m.enclosing_span();
                    s.start.line <= line && line <= s.end.line
                })
                .map(|m| m.name.as_str())
                .collect();
            prop_assert_eq!(selected, expected);
        }
    }
}
