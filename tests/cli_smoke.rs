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

/// A failing check makes `doctor` exit non-zero — the dispatch must convert the
/// `false` from `run_doctor` into a non-zero exit, not just print the report. A
/// missing config fails the config and credential checks without any network.
#[test]
fn doctor_exits_nonzero_when_a_check_fails() {
    let (ok, stdout, stderr) = run(&["--config", "/nonexistent/cola.toml", "doctor"]);
    assert!(
        !ok,
        "a failing doctor must exit non-zero; stdout: {stdout}\nstderr: {stderr}"
    );
    assert!(stdout.contains("❌"), "the failed check is shown: {stdout}");
}

/// Run the built `cola` binary with extra environment variables and return
/// `(success, stdout, stderr)`.
#[cfg(unix)]
fn run_env(args: &[&str], envs: &[(&str, &str)]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_cola"))
        .args(args)
        .envs(envs.iter().copied())
        .output()
        .expect("run the cola binary");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// `cola completions --install` writes into the XDG trees it is given (never
/// `$HOME` directly), is idempotent, infers the shell from `$SHELL`, and
/// refuses to clobber a completion it did not generate.
#[cfg(unix)]
#[test]
fn completions_install_writes_into_the_xdg_tree() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    let config = tmp.path().join("config");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let envs = [
        ("HOME", home.to_str().unwrap()),
        ("XDG_DATA_HOME", data.to_str().unwrap()),
        ("XDG_CONFIG_HOME", config.to_str().unwrap()),
        ("SHELL", "/bin/zsh"),
    ];

    // An explicit shell installs to that shell's XDG location.
    let (ok, stdout, stderr) = run_env(&["completions", "bash", "--install"], &envs);
    assert!(ok, "install must succeed; stderr: {stderr}");
    let bash = data.join("bash-completion/completions/cola");
    assert!(bash.is_file(), "written at {}: {stdout}", bash.display());
    assert!(std::fs::read_to_string(&bash).unwrap().contains("_cola"));

    // A second run is a no-op, not an error.
    let (ok, stdout, stderr) = run_env(&["completions", "bash", "--install"], &envs);
    assert!(ok, "reinstalling must succeed; stderr: {stderr}");
    assert!(stdout.contains("already up to date"), "{stdout}");

    // Omitting the shell infers it from $SHELL.
    let (ok, stdout, stderr) = run_env(&["completions", "--install"], &envs);
    assert!(ok, "shell inference must succeed; stderr: {stderr}");
    let zsh = data.join("zsh/site-functions/_cola");
    assert!(zsh.is_file(), "zsh completion at {}", zsh.display());
    assert!(stdout.contains("fpath"), "the zsh note must print: {stdout}");
    assert!(
        stdout.contains("fpath=('"),
        "the fpath entry must be quoted: {stdout}"
    );

    // An up-to-date zsh install still carries the fpath setup note.
    let (ok, stdout, stderr) = run_env(&["completions", "--install"], &envs);
    assert!(ok, "reinstalling must succeed; stderr: {stderr}");
    assert!(stdout.contains("already up to date"), "{stdout}");
    assert!(stdout.contains("fpath"), "{stdout}");

    // --dry-run reports without writing.
    let (ok, stdout, _) = run_env(&["completions", "fish", "--install", "--dry-run"], &envs);
    assert!(ok, "--dry-run must succeed");
    assert!(stdout.contains("--dry-run"), "{stdout}");
    assert!(!config.join("fish/completions/cola.fish").exists());

    // A live singleton lock must not block completions: the subcommand is
    // dispatched before lock handling (ADR-0081). The recorded PID is this test
    // process, which is alive, so an implementation that took the lock first
    // would refuse here.
    //
    // This fixture mirrors `StateLayout`'s lock path (`~/.cola/cola.lock`). The
    // crate is binary-only (`[[bin]]`, no lib target), so this integration test
    // cannot link `StateLayout`; if the lock path changes, the unit test in
    // `src/state.rs` is the owner that must change with it.
    let lock = home.join(".cola").join("cola.lock");
    std::fs::create_dir_all(lock.parent().unwrap()).unwrap();
    std::fs::write(&lock, std::process::id().to_string()).unwrap();
    let (ok, stdout, stderr) = run_env(&["completions", "bash"], &envs);
    assert!(
        ok,
        "printing completions must ignore the singleton lock; stderr: {stderr}"
    );
    assert!(stdout.contains("_cola"), "{stdout}");
    assert!(lock.is_file(), "the lock file must be left alone");

    // A hand-written completion is left alone and the run fails.
    let fish = config.join("fish/completions/cola.fish");
    std::fs::create_dir_all(fish.parent().unwrap()).unwrap();
    std::fs::write(&fish, "complete -c cola -a mine\n").unwrap();
    let (ok, _, stderr) = run_env(&["completions", "fish", "--install"], &envs);
    assert!(!ok, "a foreign file must not be overwritten");
    assert!(stderr.contains("refusing to overwrite"), "{stderr}");
    assert_eq!(
        std::fs::read_to_string(&fish).unwrap(),
        "complete -c cola -a mine\n"
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
