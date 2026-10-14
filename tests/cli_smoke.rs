//! End-to-end proof that the introspection commands are handled before config
//! loading, logging and the singleton lock (ADR-0081) — they must work against
//! a broken install, and while another cola runs. Moving those dispatch arms
//! after the startup machinery would leave the unit tests green while breaking
//! the promise, so this drives the real binary.
//!
//! These tests do not touch a running cola: `status`/`config`/`completions`
//! never take the Singleton Lock.

use std::process::Command;

/// Run the built `cola` binary and return `(success, stdout, stderr)`.
fn run(args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_cola"))
        .args(args)
        .output()
        .expect("run the cola binary");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// `cola status` must run with a config path that does not exist, reporting the
/// missing config rather than failing.
#[test]
fn status_runs_with_a_missing_config() {
    let (ok, stdout, stderr) = run(&["--config", "/nonexistent/cola.toml", "status", "--json"]);
    assert!(ok, "`cola status` must not need a config; stderr: {stderr}");
    assert!(
        stdout.contains("\"config_ok\": false"),
        "a missing config is reported, not fatal: {stdout}"
    );
}

/// `cola config path` and `cola completions` are pure: no config, no lock.
#[test]
fn config_and_completions_run_without_a_config() {
    let (ok, _, stderr) = run(&["--config", "/nonexistent/cola.toml", "config", "path"]);
    assert!(ok, "`cola config path` must not need a config; stderr: {stderr}");

    let (ok, stdout, stderr) = run(&["completions", "bash"]);
    assert!(ok, "`cola completions` must not need a config; stderr: {stderr}");
    assert!(
        stdout.contains("_cola"),
        "a bash completion script is printed: {stdout}"
    );
}
