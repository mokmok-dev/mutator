//! Outcome parsing, the status table, scoring, the gate, and the three reporters.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::Serialize;

use crate::accept::Accepted;
use crate::error::{Error, Result};
use crate::mutant::{Mutant, MutantId};

/// What one `cargo-mutants` run said about a mutant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Caught,
    Missed,
    Timeout,
    Unviable,
    Failure,
}

impl Outcome {
    fn parse(summary: &str) -> Result<Self> {
        match summary {
            "CaughtMutant" => Ok(Self::Caught),
            "MissedMutant" => Ok(Self::Missed),
            "Timeout" => Ok(Self::Timeout),
            "Unviable" => Ok(Self::Unviable),
            "Failure" => Ok(Self::Failure),
            other => Err(Error::UnknownSummary {
                summary: other.to_owned(),
            }),
        }
    }
}

/// One mutant scenario's verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub name: String,
    pub outcome: Outcome,
}

#[derive(serde::Deserialize)]
struct Lab {
    outcomes: Vec<Scenario>,
}

#[derive(serde::Deserialize)]
struct Scenario {
    scenario: ScenarioKind,
    summary: String,
}

#[derive(serde::Deserialize)]
enum ScenarioKind {
    Baseline,
    Mutant { name: String },
}

fn lab(json: &str) -> Result<Lab> {
    serde_json::from_str(json).map_err(|source| Error::Outcomes { source })
}

/// Every scenario in `mutants.out/outcomes.json`, split by kind.
fn scenarios(json: &str) -> Result<impl Iterator<Item = (bool, String, String)> + '_> {
    Ok(lab(json)?.outcomes.into_iter().map(|scenario| {
        let is_mutant = !matches!(scenario.scenario, ScenarioKind::Baseline);
        let name = match scenario.scenario {
            ScenarioKind::Baseline => String::new(),
            ScenarioKind::Mutant { name } => name,
        };
        (is_mutant, name, scenario.summary)
    }))
}

/// The mutant verdicts in `mutants.out/outcomes.json`.
///
/// # Errors
///
/// Returns when the file is not the documented lab outcome or holds an unknown summary.
pub fn parse_outcomes(json: &str) -> Result<Vec<Record>> {
    scenarios(json)?
        .filter(|(is_mutant, _, _)| *is_mutant)
        .map(|(_, name, summary)| {
            Ok(Record {
                name,
                outcome: Outcome::parse(&summary)?,
            })
        })
        .collect()
}

/// Whether any baseline scenario failed, which means the tests are broken without any mutant.
///
/// # Errors
///
/// Returns when the file is not the documented lab outcome.
pub fn baseline_failed(json: &str) -> Result<bool> {
    Ok(scenarios(json)?.any(|(is_mutant, _, summary)| !is_mutant && summary != "Success"))
}

/// A mutant's standing after accepted entries and the run are combined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Status {
    Caught,
    Missed,
    Timeout,
    Unviable,
    Accepted,
    NotRun,
}

impl Status {
    #[must_use]
    const fn detected(self) -> bool {
        matches!(self, Self::Caught | Self::Timeout)
    }

    #[must_use]
    const fn label(self) -> &'static str {
        match self {
            Self::Caught => "KILLED",
            Self::Missed => "SURVIVED",
            Self::Timeout => "TIMEOUT",
            Self::Unviable => "UNVIABLE",
            Self::Accepted => "ACCEPTED",
            Self::NotRun => "NOT RUN",
        }
    }
}

/// One selected mutant and its verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Row {
    pub id: MutantId,
    pub name: String,
    pub file: String,
    pub line: u32,
    pub function: String,
    pub status: Status,
}

/// Per-status counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Counts {
    pub caught: usize,
    pub missed: usize,
    pub timeout: usize,
    pub unviable: usize,
    pub accepted: usize,
    pub not_run: usize,
}

/// The whole report for one run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    pub rows: Vec<Row>,
    pub scope: String,
    pub scope_total: usize,
    pub stale: Vec<String>,
    /// Mutants settled by a stored verdict instead of a run: caught, then missed.
    pub reused: (usize, usize),
}

impl Summary {
    /// Note how many verdicts came from the cache.
    #[must_use]
    pub const fn with_reuse(mut self, caught: usize, missed: usize) -> Self {
        self.reused = (caught, missed);
        self
    }

    #[must_use]
    pub fn counts(&self) -> Counts {
        let mut counts = Counts::default();
        for row in &self.rows {
            match row.status {
                Status::Caught => counts.caught += 1,
                Status::Missed => counts.missed += 1,
                Status::Timeout => counts.timeout += 1,
                Status::Unviable => counts.unviable += 1,
                Status::Accepted => counts.accepted += 1,
                Status::NotRun => counts.not_run += 1,
            }
        }
        counts
    }

    /// Mutants the tests killed: a caught mutant or one that timed out under test.
    #[must_use]
    pub fn detected(&self) -> usize {
        self.rows.iter().filter(|row| row.status.detected()).count()
    }

    /// Mutants the score is computed over: detected plus missed.
    #[must_use]
    pub fn decided(&self) -> usize {
        self.rows
            .iter()
            .filter(|row| row.status.detected() || row.status == Status::Missed)
            .count()
    }

    /// The kill rate in basis points, absent when nothing was decided.
    #[must_use]
    pub fn score_bp(&self) -> Option<u32> {
        let decided = self.decided();
        if decided == 0 {
            return None;
        }
        let detected = u32::try_from(self.detected()).unwrap_or(u32::MAX);
        let decided = u32::try_from(decided).unwrap_or(u32::MAX);
        Some(detected.saturating_mul(10_000) / decided)
    }

    #[must_use]
    pub fn score_text(&self) -> Option<String> {
        self.score_bp().map(|bp| format!("{}%", format_bp(bp)))
    }

    #[must_use]
    pub fn gate(&self, gate: &Gate) -> GateResult {
        let counts = self.counts();
        if counts.missed > 0 && gate.fail_on_survived {
            return GateResult::Fail(format!(
                "{} missed {}",
                counts.missed,
                plural(counts.missed, "mutant", "mutants")
            ));
        }
        if counts.not_run > 0 {
            return GateResult::Fail(format!(
                "{} {} not run",
                counts.not_run,
                plural(counts.not_run, "mutant was", "mutants were")
            ));
        }
        if let (Some(threshold), Some(score)) = (gate.threshold_bp, self.score_bp())
            && score < threshold
        {
            return GateResult::Fail(format!(
                "score {}% is below the {}% threshold",
                format_bp(score),
                format_bp(threshold)
            ));
        }
        GateResult::Pass
    }
}

/// What fails the gate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Gate {
    pub fail_on_survived: bool,
    pub threshold_bp: Option<u32>,
}

pub enum GateResult {
    Pass,
    Fail(String),
}

const fn plural(count: usize, one: &'static str, many: &'static str) -> &'static str {
    if count == 1 { one } else { many }
}

fn format_bp(bp: u32) -> String {
    format!("{}.{:02}", bp / 100, bp % 100)
}

/// Reduce selected mutants, run records, and accepted entries into rows.
#[must_use]
pub fn build(
    selected: &[(&Mutant, MutantId)],
    records: &[Record],
    accepted: &Accepted,
    scope: String,
    scope_total: usize,
    stale: Vec<String>,
) -> Summary {
    let by_name: BTreeMap<&str, Outcome> = records
        .iter()
        .map(|record| (record.name.as_str(), record.outcome))
        .collect();
    let mut rows: Vec<Row> = selected
        .iter()
        .map(|(mutant, id)| Row {
            id: id.clone(),
            name: mutant.name.clone(),
            file: mutant.file.clone(),
            line: mutant.line(),
            function: mutant.function_name().to_owned(),
            status: status(id, mutant, &by_name, accepted),
        })
        .collect();
    rows.sort_by(|left, right| {
        (&left.file, left.line, &left.name).cmp(&(&right.file, right.line, &right.name))
    });
    Summary {
        rows,
        scope,
        scope_total,
        stale,
        reused: (0, 0),
    }
}

/// Accepted first, then the run, then the crash rule. One table, one place.
fn status(
    id: &MutantId,
    mutant: &Mutant,
    by_name: &BTreeMap<&str, Outcome>,
    accepted: &Accepted,
) -> Status {
    if accepted.is_accepted(id) {
        return Status::Accepted;
    }
    match by_name.get(mutant.name.as_str()) {
        Some(Outcome::Caught) => Status::Caught,
        Some(Outcome::Missed | Outcome::Failure) => Status::Missed,
        Some(Outcome::Timeout) => Status::Timeout,
        Some(Outcome::Unviable) => Status::Unviable,
        None => Status::NotRun,
    }
}

/// The human report.
#[must_use]
pub fn render_text(summary: &Summary) -> String {
    if summary.rows.is_empty() {
        return format!("mutator: no mutants in scope ({})\n", summary.scope);
    }
    let counts = summary.counts();
    let mut text = format!(
        "mutator: {} of {} mutants in scope ({})\n",
        summary.rows.len(),
        summary.scope_total,
        summary.scope
    );
    for row in &summary.rows {
        let _ = writeln!(
            text,
            "  {}:{}  {}  {}",
            row.file,
            row.line,
            row.function,
            row.status.label()
        );
    }
    let _ = writeln!(
        text,
        "score: {} ({} detected, {} missed, {} accepted, {} unviable, {} not run)",
        summary.score_text().unwrap_or_else(|| "n/a".to_owned()),
        summary.detected(),
        counts.missed,
        counts.accepted,
        counts.unviable,
        counts.not_run
    );
    if summary.reused != (0, 0) {
        let (caught, missed) = summary.reused;
        let _ = writeln!(
            text,
            "cache: {caught} killed {} reused, {missed} survivors decided again",
            plural(caught, "verdict", "verdicts")
        );
    }
    if !summary.stale.is_empty() {
        let _ = writeln!(
            text,
            "stale accepted: {} {} ({})",
            summary.stale.len(),
            plural(summary.stale.len(), "entry", "entries"),
            summary.stale.join(", ")
        );
    }
    text
}

#[derive(Serialize)]
struct JsonReport<'a> {
    #[serde(rename = "schemaVersion")]
    schema_version: u32,
    scope: &'a str,
    counts: Counts,
    detected: usize,
    decided: usize,
    #[serde(rename = "scoreBp")]
    score_bp: Option<u32>,
    rows: &'a [Row],
    #[serde(rename = "staleAccepted")]
    stale_accepted: &'a [String],
    #[serde(rename = "reused")]
    reused: Reused,
}

#[derive(Serialize)]
struct Reused {
    caught: usize,
    missed: usize,
}

/// The machine report.
///
/// # Errors
///
/// Returns when the report cannot be serialized.
pub fn render_json(summary: &Summary) -> Result<String> {
    let report = JsonReport {
        schema_version: 1,
        scope: &summary.scope,
        counts: summary.counts(),
        detected: summary.detected(),
        decided: summary.decided(),
        score_bp: summary.score_bp(),
        rows: &summary.rows,
        stale_accepted: &summary.stale,
        reused: Reused {
            caught: summary.reused.0,
            missed: summary.reused.1,
        },
    };
    let mut text = serde_json::to_string(&report).map_err(|source| Error::Serialize {
        what: "JSON report",
        source,
    })?;
    text.push('\n');
    Ok(text)
}

/// GitHub Actions annotations for the diff.
#[must_use]
pub fn render_github(summary: &Summary) -> String {
    let mut text = String::new();
    for row in summary
        .rows
        .iter()
        .filter(|row| row.status == Status::Missed)
    {
        let _ = writeln!(
            text,
            "::warning file={},line={}::mutant survived: {}",
            row.file, row.line, row.name
        );
    }
    for id in &summary.stale {
        let _ = writeln!(text, "::notice::stale accepted mutant: {id}");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{Pos, Span};
    use crate::mutant::Function;
    use proptest::prelude::*;

    fn span(line: u32) -> Span {
        Span::new(Pos::new(line, 5), Pos::new(line, 9))
    }

    fn mutant(name: &str, function: &str, line: u32) -> Mutant {
        Mutant {
            name: name.to_owned(),
            file: "src/lib.rs".to_owned(),
            package: "mutator".to_owned(),
            genre: "FnValue".to_owned(),
            replacement: "0".to_owned(),
            span: span(line),
            function: Some(Function {
                name: function.to_owned(),
                span: Span::new(Pos::new(line, 1), Pos::new(line + 4, 2)),
            }),
        }
    }

    fn record(name: &str, outcome: Outcome) -> Record {
        Record {
            name: name.to_owned(),
            outcome,
        }
    }

    const LAB: &str = r#"{"total_mutants":2,"missed":1,"caught":1,"timeout":0,"unviable":0,"success":0,
        "outcomes":[
          {"scenario":"Baseline","summary":"Success"},
          {"scenario":{"Mutant":{"name":"a"}},"summary":"CaughtMutant"},
          {"scenario":{"Mutant":{"name":"b"}},"summary":"MissedMutant"}
        ]}"#;

    fn one(name: &str, function: &str, line: u32) -> (Vec<Mutant>, Vec<MutantId>) {
        let mutants = vec![mutant(name, function, line)];
        let ids = crate::mutant::assign_ids(&mutants);
        (mutants, ids)
    }

    fn selected<'a>(mutants: &'a [Mutant], ids: &[MutantId]) -> Vec<(&'a Mutant, MutantId)> {
        mutants
            .iter()
            .zip(ids)
            .map(|(m, i)| (m, i.clone()))
            .collect()
    }

    #[test]
    fn parses_mutant_outcomes_only() {
        let records = parse_outcomes(LAB).unwrap();
        assert_eq!(
            records,
            vec![record("a", Outcome::Caught), record("b", Outcome::Missed)]
        );
        assert!(!baseline_failed(LAB).unwrap());
    }

    #[test]
    fn every_summary_string_maps() {
        for (summary, outcome) in [
            ("CaughtMutant", Outcome::Caught),
            ("MissedMutant", Outcome::Missed),
            ("Timeout", Outcome::Timeout),
            ("Unviable", Outcome::Unviable),
            ("Failure", Outcome::Failure),
        ] {
            let json = format!(
                r#"{{"outcomes":[{{"scenario":{{"Mutant":{{"name":"x"}}}},"summary":"{summary}"}}]}}"#
            );
            assert_eq!(
                parse_outcomes(&json).unwrap().first().map(|r| r.outcome),
                Some(outcome)
            );
        }
    }

    #[test]
    fn an_unknown_summary_is_an_error() {
        let json = r#"{"outcomes":[{"scenario":{"Mutant":{"name":"x"}},"summary":"Exploded"}]}"#;
        assert!(
            parse_outcomes(json)
                .unwrap_err()
                .to_string()
                .contains("Exploded")
        );
    }

    #[test]
    fn a_failed_baseline_is_reported() {
        let json = r#"{"outcomes":[{"scenario":"Baseline","summary":"Failure"}]}"#;
        assert!(baseline_failed(json).unwrap());
        assert_eq!(parse_outcomes(json).unwrap(), Vec::new());
    }

    #[test]
    fn an_empty_report_names_the_scope() {
        let summary = build(
            &[],
            &[],
            &Accepted::default(),
            "diff since main".to_owned(),
            0,
            vec![],
        );
        assert_eq!(
            render_text(&summary),
            "mutator: no mutants in scope (diff since main)\n"
        );
    }

    #[test]
    fn a_crash_counts_as_missed_and_an_absent_record_as_not_run() {
        let mutants = vec![mutant("a", "one", 3), mutant("b", "two", 9)];
        let ids = crate::mutant::assign_ids(&mutants);
        let summary = build(
            &selected(&mutants, &ids),
            &[record("a", Outcome::Failure)],
            &Accepted::default(),
            "all mutants".to_owned(),
            2,
            vec![],
        );
        assert_eq!(summary.counts().missed, 1);
        assert_eq!(summary.counts().not_run, 1);
    }

    #[test]
    fn accepted_wins_over_the_run() {
        let (mutants, ids) = one("a", "one", 3);
        let mut accepted = Accepted::default();
        accepted.insert(ids[0].clone(), "unreachable".to_owned());
        let summary = build(
            &selected(&mutants, &ids),
            &[record("a", Outcome::Missed)],
            &accepted,
            "all mutants".to_owned(),
            1,
            vec![],
        );
        assert_eq!(summary.rows[0].status, Status::Accepted);
        assert_eq!(summary.score_bp(), None);
        assert!(matches!(summary.gate(&Gate::default()), GateResult::Pass));
    }

    #[test]
    fn the_score_counts_caught_and_timeout_as_detected() {
        let mutants = vec![
            mutant("a", "one", 3),
            mutant("b", "two", 9),
            mutant("c", "three", 15),
        ];
        let ids = crate::mutant::assign_ids(&mutants);
        let records = [
            record("a", Outcome::Caught),
            record("b", Outcome::Timeout),
            record("c", Outcome::Missed),
        ];
        let summary = build(
            &selected(&mutants, &ids),
            &records,
            &Accepted::default(),
            "all mutants".to_owned(),
            3,
            vec![],
        );
        assert_eq!(summary.detected(), 2);
        assert_eq!(summary.decided(), 3);
        assert_eq!(summary.score_bp(), Some(6666));
        assert_eq!(summary.score_text().as_deref(), Some("66.66%"));
    }

    proptest! {
        #[test]
        fn the_score_is_detected_over_decided(
            caught in 0_usize..20,
            timeout in 0_usize..20,
            missed in 0_usize..20,
        ) {
            let total = caught + timeout + missed;
            let mutants: Vec<Mutant> = (0..total)
                .map(|index| {
                    let line = u32::try_from(index).unwrap_or(0) * 10 + 1;
                    mutant(&format!("m{index}"), &format!("f{index}"), line)
                })
                .collect();
            let ids = crate::mutant::assign_ids(&mutants);
            let mut records = Vec::new();
            let mut index = 0;
            for _ in 0..caught { records.push(record(&format!("m{index}"), Outcome::Caught)); index += 1; }
            for _ in 0..timeout { records.push(record(&format!("m{index}"), Outcome::Timeout)); index += 1; }
            for _ in 0..missed { records.push(record(&format!("m{index}"), Outcome::Missed)); index += 1; }
            let summary = build(
                &selected(&mutants, &ids),
                &records,
                &Accepted::default(),
                "all mutants".to_owned(),
                total,
                vec![],
            );
            prop_assert_eq!(summary.detected(), caught + timeout);
            prop_assert_eq!(summary.decided(), total);
            let expected = (caught + timeout)
                .checked_mul(10_000)
                .and_then(|scaled| scaled.checked_div(total))
                .and_then(|bp| u32::try_from(bp).ok());
            prop_assert_eq!(summary.score_bp(), expected);
        }
    }

    #[test]
    fn the_gate_fails_on_missed_only_when_asked() {
        let (mutants, ids) = one("a", "one", 3);
        let summary = build(
            &selected(&mutants, &ids),
            &[record("a", Outcome::Missed)],
            &Accepted::default(),
            "all mutants".to_owned(),
            1,
            vec![],
        );
        assert!(matches!(summary.gate(&Gate::default()), GateResult::Pass));
        match summary.gate(&Gate {
            fail_on_survived: true,
            threshold_bp: None,
        }) {
            GateResult::Fail(message) => assert_eq!(message, "1 missed mutant"),
            GateResult::Pass => panic!("gate should fail"),
        }
    }

    #[test]
    fn the_gate_fails_on_unrun_mutants() {
        let (mutants, ids) = one("a", "one", 3);
        let summary = build(
            &selected(&mutants, &ids),
            &[],
            &Accepted::default(),
            "all mutants".to_owned(),
            1,
            vec![],
        );
        match summary.gate(&Gate::default()) {
            GateResult::Fail(message) => assert_eq!(message, "1 mutant was not run"),
            GateResult::Pass => panic!("gate should fail"),
        }
    }

    #[test]
    fn the_threshold_compares_at_the_bound() {
        let mutants = vec![mutant("a", "one", 3), mutant("b", "two", 9)];
        let ids = crate::mutant::assign_ids(&mutants);
        let records = [record("a", Outcome::Caught), record("b", Outcome::Missed)];
        let summary = build(
            &selected(&mutants, &ids),
            &records,
            &Accepted::default(),
            "all mutants".to_owned(),
            2,
            vec![],
        );
        assert!(matches!(
            summary.gate(&Gate {
                fail_on_survived: false,
                threshold_bp: Some(5000),
            }),
            GateResult::Pass
        ));
        match summary.gate(&Gate {
            fail_on_survived: false,
            threshold_bp: Some(5001),
        }) {
            GateResult::Fail(message) => {
                assert_eq!(message, "score 50.00% is below the 50.01% threshold");
            }
            GateResult::Pass => panic!("gate should fail"),
        }
    }

    #[test]
    fn counts_cover_every_status() {
        let mutants: Vec<Mutant> = (0..6)
            .map(|index| mutant(&format!("m{index}"), &format!("f{index}"), index * 10 + 1))
            .collect();
        let ids = crate::mutant::assign_ids(&mutants);
        let mut accepted = Accepted::default();
        accepted.insert(ids[4].clone(), "accepted".to_owned());
        let records = [
            record("m0", Outcome::Caught),
            record("m1", Outcome::Missed),
            record("m2", Outcome::Timeout),
            record("m3", Outcome::Unviable),
            record("m4", Outcome::Caught),
        ];
        let summary = build(
            &selected(&mutants, &ids),
            &records,
            &accepted,
            "all mutants".to_owned(),
            6,
            vec![],
        );
        assert_eq!(
            summary.counts(),
            Counts {
                caught: 1,
                missed: 1,
                timeout: 1,
                unviable: 1,
                accepted: 1,
                not_run: 1,
            }
        );
        assert_eq!(summary.detected(), 2);
        assert_eq!(summary.decided(), 3);
    }

    #[test]
    fn text_report_is_exact() {
        let mutants = vec![
            mutant("src/lib.rs:3:5: replace parse -> u32 with 0", "parse", 3),
            mutant("src/lib.rs:9:5: replace render -> u32 with 0", "render", 9),
        ];
        let ids = crate::mutant::assign_ids(&mutants);
        let records = [
            record(&mutants[0].name, Outcome::Caught),
            record(&mutants[1].name, Outcome::Missed),
        ];
        let summary = build(
            &selected(&mutants, &ids),
            &records,
            &Accepted::default(),
            "all mutants".to_owned(),
            42,
            vec!["stale#0".to_owned()],
        );
        assert_eq!(
            render_text(&summary),
            "mutator: 2 of 42 mutants in scope (all mutants)\n\
             \x20 src/lib.rs:3  parse  KILLED\n\
             \x20 src/lib.rs:9  render  SURVIVED\n\
             score: 50.00% (1 detected, 1 missed, 0 accepted, 0 unviable, 0 not run)\n\
             stale accepted: 1 entry (stale#0)\n"
        );
    }

    #[test]
    fn json_report_is_exact() {
        let (mutants, ids) = one("src/lib.rs:3:5: replace parse -> u32 with 0", "parse", 3);
        let summary = build(
            &selected(&mutants, &ids),
            &[record(&mutants[0].name, Outcome::Caught)],
            &Accepted::default(),
            "diff since main".to_owned(),
            7,
            vec![],
        );
        assert_eq!(
            render_json(&summary).unwrap(),
            "{\"schemaVersion\":1,\"scope\":\"diff since main\",\
             \"counts\":{\"caught\":1,\"missed\":0,\"timeout\":0,\"unviable\":0,\"accepted\":0,\"not_run\":0},\
             \"detected\":1,\"decided\":1,\"scoreBp\":10000,\
             \"rows\":[{\"id\":\"src/lib.rs::parse::FnValue::0#0\",\
             \"name\":\"src/lib.rs:3:5: replace parse -> u32 with 0\",\
             \"file\":\"src/lib.rs\",\"line\":3,\"function\":\"parse\",\"status\":\"Caught\"}],\
             \"staleAccepted\":[],\"reused\":{\"caught\":0,\"missed\":0}}\n"
        );
    }

    #[test]
    fn github_report_annotates_survivors_and_stale_entries() {
        let (mutants, ids) = one("src/lib.rs:9:5: replace render -> u32 with 0", "render", 9);
        let summary = build(
            &selected(&mutants, &ids),
            &[record(&mutants[0].name, Outcome::Missed)],
            &Accepted::default(),
            "all mutants".to_owned(),
            1,
            vec!["gone#0".to_owned()],
        );
        assert_eq!(
            render_github(&summary),
            "::warning file=src/lib.rs,line=9::mutant survived: src/lib.rs:9:5: replace render -> u32 with 0\n\
             ::notice::stale accepted mutant: gone#0\n"
        );
    }
}
