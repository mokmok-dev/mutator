//! The one process seam: every external command goes through [`Exec`].

use std::path::Path;
use std::process::Command;

/// A finished child process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    #[must_use]
    pub const fn success(&self) -> bool {
        self.code == 0
    }
}

/// Runs a program. Tests substitute a fake.
pub trait Exec {
    /// # Errors
    ///
    /// Returns when the program cannot be started or its streams cannot be read.
    fn run(&self, program: &str, args: &[String], cwd: &Path) -> anyhow::Result<Output>;
}

/// Runs the real process, capturing both streams.
pub struct Real;

impl Exec for Real {
    fn run(&self, program: &str, args: &[String], cwd: &Path) -> anyhow::Result<Output> {
        let output = Command::new(program)
            .args(args)
            .current_dir(cwd)
            .output()
            .map_err(|err| anyhow::anyhow!("run {program}: {err}"))?;
        Ok(Output {
            code: exit_code(output.status),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// A signal-killed child has no exit code; report it as `-1`.
fn exit_code(status: std::process::ExitStatus) -> i32 {
    status.code().unwrap_or(-1)
}

/// `cargo mutants --list --json`.
#[must_use]
pub fn list_args(manifest: Option<&Path>) -> Vec<String> {
    let mut args = vec![
        "mutants".to_owned(),
        "--list".to_owned(),
        "--json".to_owned(),
    ];
    if let Some(path) = manifest {
        args.push("--manifest-path".to_owned());
        args.push(path.display().to_string());
    }
    args
}

/// A `cargo mutants` run restricted to exactly `names`.
///
/// # Errors
///
/// Returns when `names` is empty, since nothing could then be selected.
pub fn run_args(
    names: &[&str],
    out_dir: &Path,
    manifest: Option<&Path>,
    jobs: Option<u32>,
    timeout: Option<u64>,
) -> anyhow::Result<Vec<String>> {
    let mut args = vec![
        "mutants".to_owned(),
        "--no-times".to_owned(),
        "--colors".to_owned(),
        "never".to_owned(),
        "--output".to_owned(),
        out_dir.display().to_string(),
        "--re".to_owned(),
        select_regex(names)?,
    ];
    if let Some(path) = manifest {
        args.push("--manifest-path".to_owned());
        args.push(path.display().to_string());
    }
    if let Some(jobs) = jobs {
        args.push("-j".to_owned());
        args.push(jobs.to_string());
    }
    if let Some(seconds) = timeout {
        args.push("-t".to_owned());
        args.push(seconds.to_string());
    }
    Ok(args)
}

/// A regex matching exactly `names`, not the names one could be a substring of.
///
/// # Errors
///
/// Returns when `names` is empty.
pub fn select_regex(names: &[&str]) -> anyhow::Result<String> {
    if names.is_empty() {
        anyhow::bail!("cannot select an empty set of mutants");
    }
    let alternatives: Vec<String> = names.iter().map(|name| escape_regex(name)).collect();
    Ok(format!("^(?:{})$", alternatives.join("|")))
}

fn escape_regex(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        if "\\^$.|?*+()[]{}".contains(character) {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn a_regex_is_anchored_and_escapes_metacharacters() {
        let regex = select_regex(&["a.rs:2:5: replace + with (x|y)"]).unwrap();
        assert_eq!(regex, r"^(?:a\.rs:2:5: replace \+ with \(x\|y\))$");
    }

    #[test]
    fn several_names_are_one_alternation() {
        let regex = select_regex(&["one", "two"]).unwrap();
        assert_eq!(regex, "^(?:one|two)$");
    }

    #[test]
    fn an_empty_selection_is_an_error() {
        assert!(select_regex(&[]).is_err());
    }

    #[test]
    fn run_args_carry_the_output_directory_and_the_regex() {
        let args = run_args(&["x"], Path::new("/tmp/out"), None, Some(2), Some(30)).unwrap();
        let text = args.join(" ");
        assert!(text.contains("--output /tmp/out"));
        assert!(text.contains("--re ^(?:x)$"));
        assert!(text.contains("-j 2"));
        assert!(text.contains("-t 30"));
        assert!(!text.contains("--manifest-path"));
    }

    #[test]
    fn list_args_add_a_manifest_only_when_given() {
        assert_eq!(list_args(None), ["mutants", "--list", "--json"]);
        let args = list_args(Some(Path::new("x/Cargo.toml")));
        assert_eq!(args.last().map(String::as_str), Some("x/Cargo.toml"));
        assert!(list_args(None).iter().all(|arg| arg != "--manifest-path"));
    }

    #[cfg(unix)]
    #[test]
    fn a_signal_killed_child_reports_minus_one() {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(exit_code(std::process::ExitStatus::from_raw(9)), -1);
        assert_eq!(exit_code(std::process::ExitStatus::from_raw(0x0200)), 2);
    }

    proptest! {
        #[test]
        fn a_name_with_no_metacharacters_survives_escaping(name in "[a-zA-Z0-9 _:>-]{1,30}") {
            prop_assert_eq!(escape_regex(&name), name);
        }

        #[test]
        fn every_metacharacter_is_escaped(
            text in prop::sample::select(vec![
                '\\', '^', '$', '.', '|', '?', '*', '+', '(', ')', '[', ']', '{', '}',
            ]),
        ) {
            prop_assert_eq!(escape_regex(&text.to_string()), format!("\\{text}"));
        }
    }
}
