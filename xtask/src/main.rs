use std::process::{Command, exit};

fn main() {
    let task = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("Usage: cargo xtask <task>");
        eprintln!("Tasks: check, test, clippy, fmt, audit, check-commit-msg");
        exit(1);
    });

    match task.as_str() {
        "check" => run("check"),
        "test" => run("test"),
        "clippy" => run_clippy(),
        "fmt" => run_fmt(),
        "audit" => run_audit(),
        "check-commit-msg" => run_check_commit_msg(),
        other => {
            eprintln!("Unknown task: {other}. Available: check, test, clippy, fmt, audit, check-commit-msg");
            exit(1);
        }
    }
}

fn run_check_commit_msg() {
    let file = std::env::args().nth(2).unwrap_or_else(|| {
        eprintln!("usage: cargo xtask check-commit-msg <commit-msg-file>");
        exit(1);
    });
    let subject = std::fs::read_to_string(&file)
        .unwrap_or_else(|e| {
            eprintln!("error: reading {file}: {e}");
            exit(1);
        })
        // The first non-comment, non-blank line is the subject. Comment (`#`)
        // and blank lines are git's own scaffolding.
        .lines()
        .find(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .unwrap_or_default()
        .to_string();
    if let Err(msg) = validate_subject(&subject) {
        eprintln!("{msg}");
        exit(1);
    }
}

const TYPES: &[&str] = &[
    "feat", "fix", "docs", "style", "refactor", "test", "chore", "ci", "build", "perf", "revert",
];

fn validate_subject(subject: &str) -> Result<(), String> {
    // Tolerate git-generated merge/revert messages.
    if subject.starts_with("Merge") || subject.starts_with("Revert") {
        return Ok(());
    }

    if !is_conventional(subject) {
        return Err(format!(
            "error: commit subject must follow Conventional Commits\n  \
             <type>(<scope>)?: <subject>\n  \
             types: feat, fix, docs, style, refactor, test, chore, ci, build, perf, revert\n  \
             got: {subject}"
        ));
    }

    let len = subject.chars().count();
    if len > 72 {
        return Err(format!(
            "error: commit subject is {len} chars (max 72)\n  {subject}"
        ));
    }
    Ok(())
}

/// `feat(scope)!: subject` — `<type>(<scope>)?(!)?: <non-empty subject>`.
fn is_conventional(subject: &str) -> bool {
    let type_end = subject
        .char_indices()
        .find(|&(_, c)| matches!(c, '(' | '!' | ':'))
        .map(|(i, _)| i)
        .unwrap_or(subject.len());
    if !TYPES.contains(&&subject[..type_end]) {
        return false;
    }

    let mut rest = &subject[type_end..];
    if let Some(inner) = rest.strip_prefix('(') {
        let Some(close) = inner.find(')') else {
            return false;
        };
        let scope = &inner[..close];
        if scope.is_empty()
            || !scope
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
        {
            return false;
        }
        rest = &inner[close + 1..];
    }
    if let Some(r) = rest.strip_prefix('!') {
        rest = r;
    }
    matches!(rest.strip_prefix(": "), Some(s) if !s.is_empty())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid_conventional_subjects() {
        for s in [
            "feat: add thing",
            "fix(core): correct bug",
            "docs(bridge): update readme",
            "refactor!: big change",
            "feat(scope)!: breaking",
            "chore: bump deps",
            "ci(github): cache cargo",
        ] {
            assert!(is_conventional(s), "{s} should be valid");
        }
    }

    #[test]
    fn rejects_invalid_subjects() {
        for s in [
            "",
            "added thing",
            "feat",
            "feat:",
            "feat: ",
            "feat() : x",
            "feat(Scope): x",
            "feat(scope) x",
            "fix : x",
            "unknown: x",
            "feat(): x",
        ] {
            assert!(!is_conventional(s), "{s} should be invalid");
        }
    }

    #[test]
    fn merge_and_revert_subjects_are_tolerated() {
        assert!(validate_subject("Merge branch 'main'").is_ok());
        assert!(validate_subject("Revert \"feat: add thing\"").is_ok());
    }

    #[test]
    fn overlong_subject_is_rejected() {
        let long = format!("docs: {}", "x".repeat(70));
        assert_eq!(long.chars().count(), 76);
        assert!(validate_subject(&long).is_err());
        // Exactly 72 chars is the boundary — still valid.
        let boundary = format!("docs: {}", "x".repeat(66));
        assert_eq!(boundary.chars().count(), 72);
        assert!(validate_subject(&boundary).is_ok());
    }
}
