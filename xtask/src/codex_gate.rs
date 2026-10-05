//! The Codex gate's shared verdict parser (#530): one definition of "a
//! complete review" and of the verdict it carries, exercised against the
//! shapes the live gate has produced (plain, bolded, code span, CRLF, cut
//! streams), plus a wiring check that the workflow actually uses the parser.

#[cfg(all(test, unix))]
mod tests {
    use std::path::PathBuf;
    use std::process::Command;

    /// Run `expr` with `.github/codex/verdict.sh` sourced; stdout verbatim.
    fn run_raw(expr: &str) -> String {
        let script = crate::repo_root().join(".github/codex/verdict.sh");
        let out = Command::new("bash")
            .arg("-c")
            .arg(format!(". '{}'; {expr}", script.display()))
            .output()
            .expect("bash runs");
        assert!(
            out.status.success(),
            "bash failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cola-codex-gate-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn verdict_of(content: &str) -> String {
        let dir = temp_dir("verdict");
        let file = dir.join("codex-output.md");
        std::fs::write(&file, content).unwrap();
        let out = run_raw(&format!("codex_verdict_of_file '{}'", file.display()));
        let _ = std::fs::remove_dir_all(&dir);
        out.trim().to_string()
    }

    #[test]
    fn the_verdict_is_read_from_the_last_non_blank_line() {
        for (content, want) in [
            ("body\nCODEX_REVIEW_VERDICT: PASS\n", "PASS"),
            ("body\nCODEX_REVIEW_VERDICT: FAIL\n", "FAIL"),
            ("body\nCODEX_REVIEW_VERDICT: PASS\n\n\n", "PASS"),
            ("body\r\nCODEX_REVIEW_VERDICT: PASS\r\n", "PASS"),
            ("closing note. **CODEX_REVIEW_VERDICT: FAIL**", "FAIL"),
            ("closing note. `CODEX_REVIEW_VERDICT: PASS`", "PASS"),
            ("", ""),
            ("   \n\t\n", ""),
            ("prose with no verdict\n", ""),
            ("CODEX_REVIEW_VERDICT: PASS and then prose\n", ""),
            ("CODEX_REVIEW_VERDICT: PASS.\n", ""),
        ] {
            assert_eq!(verdict_of(content), want, "content: {content:?}");
        }
    }

    #[test]
    fn the_trim_keeps_the_verdict_as_the_final_line() {
        let dir = temp_dir("trim");
        let file = dir.join("output.md");
        std::fs::write(&file, "body\n**CODEX_REVIEW_VERDICT: PASS**\r\n\r\n").unwrap();
        let trimmed = run_raw(&format!("codex_trim_trailing_blanks '{}'", file.display()));
        let lines: Vec<&str> = trimmed.lines().collect();
        assert_eq!(lines.len(), 2, "trailing CRLF blanks dropped: {trimmed:?}");
        assert_eq!(lines.last().copied().unwrap(), "**CODEX_REVIEW_VERDICT: PASS**");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_workflow_uses_the_shared_parser() {
        let workflow =
            std::fs::read_to_string(crate::repo_root().join(".github/workflows/codex-review.yml")).unwrap();
        for function in [
            "codex_verdict_of_file",
            "codex_last_line_stdin",
            "codex_trim_trailing_blanks",
        ] {
            assert!(workflow.contains(function), "the workflow must use {function}");
        }
    }
}
