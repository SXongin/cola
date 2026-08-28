use std::process::{Command, exit};

fn main() {
    let task = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("Usage: cargo xtask <task>");
        eprintln!("Tasks: check, test, clippy, fmt");
        exit(1);
    });

    match task.as_str() {
        "check" => run("check"),
        "test" => run("test"),
        "clippy" => run_clippy(),
        "fmt" => run_fmt(),
        "audit" => run_audit(),
        other => {
            eprintln!("Unknown task: {other}. Available: check, test, clippy, fmt, audit");
            exit(1);
        }
    }
}

fn run(sub: &str) {
    let status = Command::new("cargo")
        .args([sub, "--workspace", "--exclude", "xtask"])
        .status()
        .unwrap();
    if !status.success() {
        exit(status.code().unwrap_or(1));
    }
}

fn run_clippy() {
    let status = Command::new("cargo")
        .args([
            "clippy",
            "--workspace",
            "--exclude",
            "xtask",
            "--",
            "-D",
            "warnings",
        ])
        .status()
        .unwrap();
    if !status.success() {
        exit(status.code().unwrap_or(1));
    }
}

fn run_fmt() {
    let status = Command::new("cargo")
        .args(["fmt", "--all", "--", "--check"])
        .status()
        .unwrap();
    if !status.success() {
        exit(status.code().unwrap_or(1));
    }
}

fn run_audit() {
    let status = Command::new("cargo").args(["deny", "check"]).status().unwrap();
    if !status.success() {
        eprintln!("Dependency audit failed. Install cargo-deny: cargo install cargo-deny");
        exit(status.code().unwrap_or(1));
    }
}
