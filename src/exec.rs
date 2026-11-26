//! The one process seam: every external command goes through [`Exec`].

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use crate::error::{Error, Result};

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
    fn run(&self, program: &str, args: &[String], cwd: &Path) -> Result<Output>;

    /// Runs a program, writing each line of its stdout to `sink` as it arrives.
    ///
    /// The default replays the captured stdout line by line once the program has
    /// exited. [`Real`] forwards each line while the child still runs.
    ///
    /// A sink that refuses a line does not fail the run.
    ///
    /// # Errors
    ///
    /// Returns when the program cannot be started or its streams cannot be read.
    fn run_stdout_to(
        &self,
        program: &str,
        args: &[String],
        cwd: &Path,
        sink: &mut dyn Write,
    ) -> Result<Output> {
        let output = self.run(program, args, cwd)?;
        let _ = pump(std::io::Cursor::new(output.stdout.as_str()), sink);
        Ok(output)
    }
}

/// Runs the real process, capturing both streams.
pub struct Real;

/// The one way this crate starts a child process.
///
/// An explicit null stdin matches what [`Command::output`] does implicitly, so
/// both a captured run and a streamed one give the child the same stdin.
fn command(program: &str, args: &[String], cwd: &Path) -> Command {
    let mut command = Command::new(program);
    command.args(args).current_dir(cwd).stdin(Stdio::null());
    command
}

impl Exec for Real {
    fn run(&self, program: &str, args: &[String], cwd: &Path) -> Result<Output> {
        let output = command(program, args, cwd)
            .output()
            .map_err(|source| Error::Spawn {
                program: program.to_owned(),
                source,
            })?;
        Ok(Output {
            code: exit_code(output.status),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    fn run_stdout_to(
        &self,
        program: &str,
        args: &[String],
        cwd: &Path,
        sink: &mut dyn Write,
    ) -> Result<Output> {
        let mut child = command(program, args, cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| Error::Spawn {
                program: program.to_owned(),
                source,
            })?;
        // Drain stderr on its own thread. A full stderr pipe blocks the child, and
        // the progress on stdout would stop with it.
        let stderr = child.stderr.take().map(|mut pipe| {
            std::thread::spawn(move || {
                let mut bytes = Vec::new();
                let _ = pipe.read_to_end(&mut bytes);
                String::from_utf8_lossy(&bytes).into_owned()
            })
        });
        let stdout = child.stdout.take().map_or_else(
            || Ok(String::new()),
            |pipe| pump(BufReader::new(pipe), sink),
        );
        let status = child.wait().map_err(|source| Error::Spawn {
            program: program.to_owned(),
            source,
        })?;
        let stdout = stdout.map_err(|source| Error::Spawn {
            program: program.to_owned(),
            source,
        })?;
        Ok(Output {
            code: exit_code(status),
            stdout,
            stderr: stderr.map_or_else(String::new, |handle| handle.join().unwrap_or_default()),
        })
    }
}

/// Copies `reader` to `sink` one line at a time, returning the whole text.
fn pump(mut reader: impl BufRead, sink: &mut dyn Write) -> std::io::Result<String> {
    let mut text = String::new();
    let mut bytes = Vec::new();
    loop {
        bytes.clear();
        reader.read_until(b'\n', &mut bytes)?;
        if bytes.is_empty() {
            return Ok(text);
        }
        let _ = sink.write_all(&bytes);
        let _ = sink.flush();
        text.push_str(&String::from_utf8_lossy(&bytes));
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
/// `caught` adds `-v` (`--caught`), which prints one line per caught mutant.
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
    caught: bool,
) -> Result<Vec<String>> {
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
    if caught {
        args.push("-v".to_owned());
    }
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
pub fn select_regex(names: &[&str]) -> Result<String> {
    if names.is_empty() {
        return Err(Error::EmptySelection);
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
        let args = run_args(
            &["x"],
            Path::new("/tmp/out"),
            None,
            Some(2),
            Some(30),
            false,
        )
        .unwrap();
        let text = args.join(" ");
        assert!(text.contains("--output /tmp/out"));
        assert!(text.contains("--re ^(?:x)$"));
        assert!(text.contains("-j 2"));
        assert!(text.contains("-t 30"));
        assert!(!text.contains("--manifest-path"));
    }

    #[test]
    fn run_args_print_every_verdict_only_when_asked() {
        let quiet = run_args(&["x"], Path::new("/tmp/out"), None, None, None, false).unwrap();
        assert!(!quiet.iter().any(|arg| arg == "-v"), "{quiet:?}");

        let caught = run_args(&["x"], Path::new("/tmp/out"), None, None, None, true).unwrap();
        assert!(caught.iter().any(|arg| arg == "-v"), "{caught:?}");
    }

    #[derive(Default)]
    struct Lines(Vec<String>);

    impl Write for Lines {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.0.push(String::from_utf8_lossy(buffer).into_owned());
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn pump_forwards_each_line_and_returns_the_whole_text() {
        let text = "ok       Unmutated baseline\ncaught   src/lib.rs:1:45: replace + with -\nnone";
        let mut sink = Lines::default();
        let whole = pump(std::io::Cursor::new(text), &mut sink).unwrap();
        assert_eq!(whole, text);
        assert_eq!(
            sink.0,
            [
                "ok       Unmutated baseline\n",
                "caught   src/lib.rs:1:45: replace + with -\n",
                "none",
            ]
        );
    }

    struct Captured(&'static str);

    impl Exec for Captured {
        fn run(&self, _program: &str, _args: &[String], _cwd: &Path) -> Result<Output> {
            Ok(Output {
                code: 0,
                stdout: self.0.to_owned(),
                stderr: String::new(),
            })
        }
    }

    #[test]
    fn the_default_replays_the_captured_stdout_line_by_line() {
        let captured = "ok       Unmutated baseline\nMISSED   src/lib.rs:1:34: replace + with *\n";
        let mut sink = Lines::default();
        let output = Captured(captured)
            .run_stdout_to("cargo", &[], Path::new("."), &mut sink)
            .unwrap();
        assert_eq!(
            sink.0,
            [
                "ok       Unmutated baseline\n",
                "MISSED   src/lib.rs:1:34: replace + with *\n",
            ]
        );
        assert_eq!(output.stdout, captured);
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
