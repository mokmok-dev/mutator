//! The binary's own contract: exit codes a caller can depend on.

#![allow(clippy::panic, reason = "a failed assertion is the test report")]

use std::process::Command;

fn mutator(args: &[&str]) -> std::process::Output {
    let output = Command::new(env!("CARGO_BIN_EXE_mutator"))
        .args(args)
        .output();
    match output {
        Ok(output) => output,
        Err(err) => panic!("the binary runs: {err}"),
    }
}

#[test]
fn a_usage_error_exits_two() {
    let output = mutator(&["--threshold", "101"]);
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("above 100") || stderr.contains("not a percentage"),
        "stderr: {stderr}"
    );
}

#[test]
fn version_exits_zero_and_names_the_crate() {
    let output = mutator(&["--version"]);
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("mutator"), "stdout: {stdout}");
}

#[test]
fn a_missing_root_reports_the_path_and_exits_one() {
    let output = mutator(&["--root", "/nonexistent-mutator-root", "--list"]);
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("/nonexistent-mutator-root"),
        "stdout: {stdout}"
    );
}
