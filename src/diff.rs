//! Unified diff parsing and line-range intersection.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::Deserialize;

/// A 1-based source position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
pub struct Pos {
    pub line: u32,
    pub column: u32,
}

impl Pos {
    #[must_use]
    pub const fn new(line: u32, column: u32) -> Self {
        Self { line, column }
    }
}

/// A 1-based inclusive source span.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct Span {
    pub start: Pos,
    pub end: Pos,
}

impl Span {
    #[must_use]
    pub const fn new(start: Pos, end: Pos) -> Self {
        Self { start, end }
    }

    /// The inclusive range of lines this span covers.
    #[must_use]
    pub const fn lines(self) -> (u32, u32) {
        (self.start.line, self.end.line)
    }
}

/// An inclusive range of new-side lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineRange {
    pub start: u32,
    pub end: u32,
}

impl LineRange {
    #[must_use]
    pub const fn new(start: u32, end: u32) -> Self {
        Self { start, end }
    }

    #[must_use]
    const fn touches(self, start: u32, end: u32) -> bool {
        self.start <= end && start <= self.end
    }
}

/// A malformed diff, with the 1-based input line that broke the parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub text: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.text)
    }
}

impl std::error::Error for ParseError {}

/// The new-side lines each file's hunks touch, and every path the diff names.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Diff {
    files: BTreeMap<String, Vec<LineRange>>,
    paths: BTreeSet<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Before the first hunk of the current file: `---`, `+++` and `@@` carry meaning.
    Header,
    /// Inside a hunk body: every line is content until the next `@@` or `diff --git`.
    Body,
}

impl Diff {
    /// Parse `git diff` output. Only hunk headers shape the result, so context width is irrelevant.
    ///
    /// # Errors
    ///
    /// Returns the input line of a hunk header that is not `@@ -a,b +c,d @@`.
    pub fn parse(text: &str) -> Result<Self, ParseError> {
        let mut files = BTreeMap::<String, Vec<LineRange>>::new();
        let mut paths = BTreeSet::<String>::new();
        let mut path: Option<String> = None;
        let mut state = State::Header;
        for (index, raw) in text.lines().enumerate() {
            if let Some(rest) = raw.strip_prefix("diff --git ") {
                // A rename names both sides, and each one can hold a changed test.
                if let Some((old, new)) = rest.rsplit_once(" b/") {
                    paths.insert(old.strip_prefix("a/").unwrap_or(old).to_owned());
                    paths.insert(new.to_owned());
                }
                path = rest.rsplit_once(" b/").map(|(_, new)| new.to_owned());
                state = State::Header;
            } else if state == State::Header && raw.starts_with("+++ ") {
                // `+++ /dev/null` marks a deletion; `strip_prefix` then yields no path.
                path = raw.strip_prefix("+++ b/").map(str::to_owned);
                if let Some(new) = path.as_ref() {
                    paths.insert(new.clone());
                }
            } else if let Some(rest) = raw.strip_prefix("@@ ") {
                let range = parse_hunk(rest).ok_or_else(|| ParseError {
                    line: index.saturating_add(1),
                    text: raw.to_owned(),
                })?;
                state = State::Body;
                if let Some(path) = path.as_ref() {
                    files.entry(path.clone()).or_default().push(range);
                }
            }
        }
        Ok(Self { files, paths })
    }

    pub fn files(&self) -> impl Iterator<Item = (&str, &[LineRange])> {
        self.files
            .iter()
            .map(|(path, ranges)| (path.as_str(), ranges.as_slice()))
    }

    /// Every path the diff names, whether or not a hunk touches it.
    ///
    /// A deletion and a rename also name the path they removed, so a selector can
    /// notice a changed test file that no new-side range describes.
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.paths.iter().map(String::as_str)
    }

    /// Whether any hunk on `path` touches `span`'s lines.
    #[must_use]
    pub fn touching(&self, path: &str, span: Span) -> bool {
        let (start, end) = span.lines();
        self.intersects(path, start, end)
    }

    /// Whether any hunk on `path` touches lines `start..=end`.
    #[must_use]
    pub fn intersects(&self, path: &str, start: u32, end: u32) -> bool {
        self.files
            .get(path)
            .is_some_and(|ranges| ranges.iter().any(|range| range.touches(start, end)))
    }

    /// Every line marked as changed by a hunk on `path`.
    pub fn lines(&self, path: &str) -> impl Iterator<Item = u32> + '_ {
        self.files
            .get(path)
            .into_iter()
            .flatten()
            .flat_map(|range| range.start..=range.end)
    }
}

/// Parse the tail of `@@ -a,b +c,d @@ …` into the new-side range.
fn parse_hunk(rest: &str) -> Option<LineRange> {
    let mut tokens = rest.split_whitespace();
    tokens.next()?;
    let (_, new) = tokens.next()?.split_once('+')?;
    let mut parts = new.split(',');
    let start: u32 = parts.next()?.parse().ok()?;
    let count: u32 = match parts.next() {
        Some(text) => text.parse().ok()?,
        None => 1,
    };
    if parts.next().is_some() {
        return None;
    }
    // A zero-length new side is a pure deletion: it touches the line it collapsed onto.
    let count = if count == 0 { 1 } else { count };
    let end = start.checked_add(count)?.checked_sub(1)?;
    Some(LineRange::new(start.max(1), end.max(1)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::fmt::Write as _;

    fn line_span(line: u32) -> Span {
        Span::new(Pos::new(line, 1), Pos::new(line, 1))
    }

    fn write_diff(diff: &Diff) -> String {
        let mut text = String::new();
        for (path, ranges) in diff.files() {
            let _ = write!(
                text,
                "diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n"
            );
            for range in ranges {
                let count = range.end - range.start + 1;
                let old = range.start + 10;
                let _ = writeln!(text, "@@ -{old},{count} +{},{count} @@ fn x()", range.start);
                for _ in 0..count {
                    text.push_str("+    changed();\n");
                }
            }
        }
        text
    }

    #[test]
    fn parses_a_single_hunk() {
        let text = "diff --git a/src/lib.rs b/src/lib.rs\n\
                    --- a/src/lib.rs\n\
                    +++ b/src/lib.rs\n\
                    @@ -2,0 +3,2 @@\n\
                    +    one\n\
                    +    two\n";
        let diff = Diff::parse(text).unwrap();
        assert!(diff.touching("src/lib.rs", line_span(3)));
        assert!(diff.touching("src/lib.rs", line_span(4)));
        assert!(!diff.touching("src/lib.rs", line_span(5)));
        assert!(!diff.touching("src/lib.rs", line_span(2)));
    }

    #[test]
    fn a_deletion_touches_the_following_line() {
        let text = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -5,2 +4,0 @@\n-x\n-y\n";
        let diff = Diff::parse(text).unwrap();
        assert_eq!(diff.lines("a.rs").collect::<Vec<_>>(), vec![4]);
    }

    #[test]
    fn deleting_the_first_line_touches_line_one() {
        let text = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,1 +0,0 @@\n-x\n";
        let diff = Diff::parse(text).unwrap();
        assert_eq!(diff.lines("a.rs").collect::<Vec<_>>(), vec![1]);
    }

    #[test]
    fn an_added_file_is_tracked_from_its_header() {
        let text =
            "diff --git a/new.rs b/new.rs\n--- /dev/null\n+++ b/new.rs\n@@ -0,0 +1,1 @@\n+x\n";
        let diff = Diff::parse(text).unwrap();
        assert_eq!(diff.lines("new.rs").collect::<Vec<_>>(), vec![1]);
    }

    #[test]
    fn a_deleted_file_records_no_lines() {
        let text = "diff --git a/gone.rs b/gone.rs\n--- a/gone.rs\n+++ /dev/null\n@@ -1,2 +0,0 @@\n-x\n-y\n";
        let diff = Diff::parse(text).unwrap();
        assert_eq!(diff.files().count(), 0);
        assert!(!diff.intersects("gone.rs", 1, 10));
        assert_eq!(diff.paths().collect::<Vec<_>>(), vec!["gone.rs"]);
    }

    #[test]
    fn a_rename_names_both_sides() {
        let text = "diff --git a/tests/old.rs b/tests/new.rs\n\
                    similarity index 100%\n\
                    rename from tests/old.rs\n\
                    rename to tests/new.rs\n";
        let diff = Diff::parse(text).unwrap();
        assert_eq!(
            diff.paths().collect::<Vec<_>>(),
            vec!["tests/new.rs", "tests/old.rs"]
        );
        assert_eq!(diff.files().count(), 0);
    }

    #[test]
    fn a_file_with_hunks_is_named_once() {
        let text = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,1 +1,1 @@\n-a\n+b\n";
        let diff = Diff::parse(text).unwrap();
        assert_eq!(diff.paths().collect::<Vec<_>>(), vec!["a.rs"]);
    }

    #[test]
    fn a_header_without_hunks_still_names_its_path() {
        let text = "diff --git a/src/x.rs b/src/x.rs\n";
        let diff = Diff::parse(text).unwrap();
        assert_eq!(diff.paths().collect::<Vec<_>>(), vec!["src/x.rs"]);
    }

    #[test]
    fn a_body_line_that_looks_like_a_header_is_content() {
        let text = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n\
                    @@ -1,1 +1,2 @@\n++++ not a header\n+    keep\n";
        let diff = Diff::parse(text).unwrap();
        assert_eq!(diff.files().count(), 1);
        assert_eq!(diff.lines("a.rs").collect::<Vec<_>>(), vec![1, 2]);
    }

    #[test]
    fn a_malformed_hunk_reports_its_line() {
        let err =
            Diff::parse("diff --git a/a.rs b/a.rs\n+++ b/a.rs\n@@ nonsense @@\n").unwrap_err();
        assert_eq!(err.line, 3);
        assert!(err.to_string().starts_with("line 3: "));
    }

    fn arb_diff() -> impl Strategy<Value = Diff> {
        let paths = prop::collection::btree_map(
            "[a-z][a-z0-9_]{0,6}\\.rs",
            prop::collection::vec((1_u32..40, 1_u32..5), 1..5),
            1..4,
        );
        paths.prop_map(|files| {
            let mut diff = Diff::default();
            for (path, hunks) in files {
                let mut ranges = Vec::new();
                let mut next = 1_u32;
                for (gap, len) in hunks {
                    next = next.saturating_add(gap);
                    let range = LineRange::new(next, next.saturating_add(len).saturating_sub(1));
                    next = range.end.saturating_add(1);
                    ranges.push(range);
                }
                diff.paths.insert(path.clone());
                diff.files.insert(path, ranges);
            }
            diff
        })
    }

    proptest! {
        #[test]
        fn write_then_parse_round_trips(diff in arb_diff()) {
            let text = write_diff(&diff);
            prop_assert_eq!(Diff::parse(&text).unwrap(), diff);
        }

        #[test]
        fn intersect_agrees_with_line_membership(diff in arb_diff(), start in 0_u32..50, len in 0_u32..6) {
            let end = start.saturating_add(len);
            let expected: Vec<u32> = diff.lines("a.rs").filter(|line| (start..=end).contains(line)).collect();
            prop_assert_eq!(expected.is_empty(), !diff.intersects("a.rs", start, end));
        }

        #[test]
        fn touching_agrees_with_intersects(diff in arb_diff(), line in 1_u32..60) {
            prop_assert_eq!(diff.touching("a.rs", line_span(line)), diff.intersects("a.rs", line, line));
        }
    }
}
