//! The Codex gate's shared verdict parser (#530): one definition of "a
//! complete review" (publication) and one of "the marker stands on its own
//! line" (approval), exercised against the shapes the live gate has produced
//! (plain, bolded, code span, CRLF, cut streams, quoted markers) plus a
//! wiring check that the workflow actually uses the parser.

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

    fn strict_verdict_of(line: &str) -> String {
        run_raw(&format!("codex_strict_verdict_of_line '{}'", line))
            .trim()
            .to_string()
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
    fn the_approval_marker_must_stand_on_its_own_line() {
        for (line, want) in [
            ("CODEX_REVIEW_VERDICT: PASS", "PASS"),
            ("CODEX_REVIEW_VERDICT: FAIL", "FAIL"),
            ("**CODEX_REVIEW_VERDICT: PASS**", "PASS"),
            ("`CODEX_REVIEW_VERDICT: FAIL`", "FAIL"),
            ("  CODEX_REVIEW_VERDICT: PASS  ", "PASS"),
            // The false-approval case the #531 review raised: a review that
            // quotes a PASS marker while blocking the change must not approve.
            (
                "This review is blocked; the quoted footer is **CODEX_REVIEW_VERDICT: PASS**",
                "",
            ),
            ("CODEX_REVIEW_VERDICT: PASS and then prose", ""),
            ("CODEX_REVIEW_VERDICT: PASS.", ""),
            ("no marker at all", ""),
        ] {
            assert_eq!(strict_verdict_of(line), want, "line: {line:?}");
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
        // The Codex step itself: the bound and the pin must stay, or a hang
        // burns the whole job and discards a finished review again (#530).
        // The live salvage path is exercised by the gate's own runs on every
        // PR; this guards the wiring against a silent revert.
        let codex = step(&workflow, "Run Codex (read-only)");
        for needle in [
            "52fe01ec70a42f454c9d2ebd47598f9fd6893d56", // v1.11: v1.12 hangs (#150/#169)
            "continue-on-error: true",
            "timeout-minutes: 12",
        ] {
            assert!(codex.contains(needle), "the Codex step must keep {needle}");
        }
        // The post step publishes only complete reviews, through the parser.
        let post = step(&workflow, "Post the review as a comment");
        for function in ["codex_verdict_of_file", "codex_trim_trailing_blanks"] {
            assert!(post.contains(function), "the post step must use {function}");
        }
        // The approve step reads the verdict strictly: only a marker on its
        // own line approves, never the publication parser (fail-closed).
        let approve = step(&workflow, "Approve when the verdict is clean");
        for function in ["codex_last_line_stdin", "codex_strict_verdict_of_line"] {
            assert!(approve.contains(function), "the approve step must use {function}");
        }
        assert!(
            !approve.contains("codex_verdict_of_line"),
            "approval must not use the publication parser"
        );
    }

    /// The YAML text of the step named `name`, up to the next step.
    fn step<'a>(workflow: &'a str, name: &str) -> &'a str {
        workflow
            .split(&format!("- name: {name}"))
            .nth(1)
            .and_then(|rest| rest.split("\n      - ").next())
            .unwrap_or_else(|| panic!("the {name} step exists"))
    }
}
