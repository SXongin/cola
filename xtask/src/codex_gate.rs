//! The Codex gate's shared scripts: the verdict parser (#530) — one
//! definition of "a complete review" (publication) and one of "the marker
//! stands on its own line" (approval), exercised against the shapes the live
//! gate has produced (plain, bolded, code span, CRLF, cut streams, quoted
//! markers) — and the linked-issue collector (#555), one definition of which
//! references the review prompt carries and how many of them it embeds. A
//! wiring check guards that the workflow actually uses both.
//!
//! Each script's single definition is the heredoc the workflow writes to
//! `$RUNNER_TEMP` at run time — never a file from the reviewed checkout, which
//! may predate it (#527 lost a completed review that way). These tests
//! extract those heredocs from `.github/workflows/codex-review.yml` and run
//! them, so the tested text and the executed text are one.

#[cfg(all(test, unix))]
mod tests {
    use std::io::Write;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};

    /// The workflow the gate reads its scripts from.
    fn workflow() -> String {
        std::fs::read_to_string(crate::repo_root().join(".github/workflows/codex-review.yml")).unwrap()
    }

    /// The body of `text`'s `<<'marker'` heredoc, up to its terminator line.
    /// The YAML block indents the body, which bash ignores, so the extracted
    /// text runs as-is.
    fn heredoc(text: &str, marker: &str) -> String {
        let opener = format!("<<'{marker}'");
        let lines = text
            .split(&opener)
            .nth(1)
            .unwrap_or_else(|| panic!("the workflow writes the {marker} heredoc"))
            .lines()
            .skip(1);
        let mut body = String::new();
        for line in lines {
            if line.trim() == marker {
                assert!(!body.is_empty(), "the {marker} heredoc has a body");
                return body;
            }
            body.push_str(line);
            body.push('\n');
        }
        panic!("the {marker} heredoc is terminated");
    }

    /// The verdict parser as the workflow writes it.
    fn verdict_script() -> String {
        heredoc(&workflow(), "VERDICT_SH")
    }

    /// The linked-issue collector as the workflow writes it.
    fn linked_issues_script() -> String {
        heredoc(&workflow(), "LINKED_ISSUES_SH")
    }

    /// The extracted parser, written once for all tests.
    fn parser_file() -> PathBuf {
        static PARSER: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
        PARSER
            .get_or_init(|| {
                let dir = temp_dir("parser");
                let file = dir.join("codex-verdict.sh");
                std::fs::write(&file, verdict_script()).unwrap();
                file
            })
            .clone()
    }

    /// The extracted collector, written once for all tests.
    fn linked_issues_file() -> PathBuf {
        static LINKED: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
        LINKED
            .get_or_init(|| {
                let dir = temp_dir("linked-issues");
                let file = dir.join("codex-linked-issues.sh");
                std::fs::write(&file, linked_issues_script()).unwrap();
                file
            })
            .clone()
    }

    /// Run `expr` with both extracted scripts sourced; stdout verbatim.
    fn run_raw(expr: &str) -> String {
        let script = parser_file();
        let linked = linked_issues_file();
        let out = Command::new("bash")
            .arg("-c")
            .arg(format!(
                ". '{}'; . '{}'; {expr}",
                script.display(),
                linked.display()
            ))
            .output()
            .expect("bash runs");
        assert!(
            out.status.success(),
            "bash failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Run `expr` with both extracted scripts sourced and `input` on stdin;
    /// stdout verbatim.
    fn run_stdin(expr: &str, input: &str) -> String {
        let script = parser_file();
        let linked = linked_issues_file();
        let mut child = Command::new("bash")
            .arg("-c")
            .arg(format!(
                ". '{}'; . '{}'; {expr}",
                script.display(),
                linked.display()
            ))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("bash spawns");
        child
            .stdin
            .as_mut()
            .expect("stdin is piped")
            .write_all(input.as_bytes())
            .expect("stdin accepts the input");
        drop(child.stdin.take());
        let out = child.wait_with_output().expect("bash runs");
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
    fn linked_issue_refs_come_from_body_keywords_and_commit_trailers() {
        for (input, want) in [
            ("Closes #545\n", "545"),
            ("Fixes: #12\n", "12"),
            ("Resolves #7.\n", "7"),
            ("Refs: #571\n", "571"),
            ("Part of #123\n", "123"),
            ("closes #10, closes #11\n", "10\n11"),
            // The keyword must abut the number: prose around it does not count.
            ("Closes the gap of #545\n", ""),
            ("no reference here\n", ""),
        ] {
            assert_eq!(
                run_stdin("codex_issue_refs_stdin", input).trim(),
                want,
                "input: {input:?}"
            );
        }
    }

    #[test]
    fn linked_issue_refs_are_deduplicated_in_first_seen_order() {
        let out = run_stdin(
            "codex_issue_refs_stdin",
            "Closes #545\nRefs: #571\n\nCloses #545\nFixes #12\nRefs: #571\n",
        );
        assert_eq!(out.trim(), "545\n571\n12");
    }

    #[test]
    fn references_beyond_the_embed_cap_are_listed_for_the_prompt() {
        let out = run_stdin(
            "CODEX_MAX_LINKED_ISSUES=2; codex_issue_refs_overflow_stdin",
            "1\n2\n3\n4\n",
        );
        assert_eq!(out.trim(), "3\n4");
    }

    #[test]
    fn the_embed_cap_defaults_to_eight() {
        assert_eq!(run_raw(r#"printf '%s\n' "$CODEX_MAX_LINKED_ISSUES""#).trim(), "8");
    }

    #[test]
    fn the_workflow_uses_the_shared_scripts() {
        let workflow = workflow();
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
        // The post step publishes only complete reviews, through the parser
        // the workflow itself wrote (never a file from the reviewed tree).
        let post = step(&workflow, "Post the review as a comment");
        assert!(
            post.contains(r#". "$RUNNER_TEMP/codex-verdict.sh""#),
            "the post step must source the parser the workflow wrote"
        );
        for function in ["codex_verdict_of_file", "codex_trim_trailing_blanks"] {
            assert!(post.contains(function), "the post step must use {function}");
        }
        // The approve step reads the verdict strictly: only a marker on its
        // own line approves, never the publication parser (fail-closed).
        let approve = step(&workflow, "Approve when the verdict is clean");
        assert!(
            approve.contains(r#". "$RUNNER_TEMP/codex-verdict.sh""#),
            "the approve step must source the parser the workflow wrote"
        );
        for function in ["codex_last_line_stdin", "codex_strict_verdict_of_line"] {
            assert!(approve.contains(function), "the approve step must use {function}");
        }
        assert!(
            !approve.contains("codex_verdict_of_line"),
            "approval must not use the publication parser"
        );
        // Both scripts are written by the workflow itself (the heredocs
        // `verdict_script` and `linked_issues_script` extract); a tree-file
        // source is the bug #527 hit.
        let write = step(&workflow, "Write the workflow scripts");
        for marker in ["VERDICT_SH", "LINKED_ISSUES_SH"] {
            assert!(write.contains(marker), "the workflow must write its own {marker}");
        }
        // The prompt step fetches every linked issue through the collector the
        // workflow wrote, from the body and the commits (#555).
        let prompt = step(&workflow, "Build the review prompt");
        assert!(
            prompt.contains(r#". "$RUNNER_TEMP/codex-linked-issues.sh""#),
            "the prompt step must source the collector the workflow wrote"
        );
        for function in ["codex_issue_refs_stdin", "codex_issue_refs_overflow_stdin"] {
            assert!(prompt.contains(function), "the prompt step must use {function}");
        }
        assert!(
            prompt.contains("git log --format=%B"),
            "the prompt step must read the commits' refs too (#555)"
        );
        // A non-PASS verdict fails the check so `gh pr checks` mirrors the
        // verdict (#555); the approval itself stays fail-closed.
        assert!(
            approve.contains("exit 1"),
            "a non-PASS verdict must fail the approve step (#555)"
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
