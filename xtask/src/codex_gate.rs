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
    use std::io::{Read, Write};
    use std::net::TcpListener;
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

    /// The `run:` block of the step text `step`, de-indented the way YAML's
    /// block scalar indents it (bash ignores that indentation, so the body runs
    /// as-is).
    fn run_block(step: &str) -> String {
        let after = step
            .split_once("run: |\n")
            .unwrap_or_else(|| panic!("the step runs an inline script"))
            .1;
        let mut script = String::new();
        for line in after.lines() {
            if let Some(rest) = line.strip_prefix("          ") {
                script.push_str(rest);
                script.push('\n');
            } else if line.trim().is_empty() {
                script.push('\n');
            } else {
                break;
            }
        }
        assert!(!script.is_empty(), "the step's run block has a body");
        script
    }

    /// A one-shot HTTP stub: answers the first request on `listener` with
    /// `status` and `body`, and hands back the whole request it read (head and
    /// body), so the caller can assert on what the check actually sent.
    fn respond_once(
        listener: TcpListener,
        status: u16,
        body: &'static str,
    ) -> std::thread::JoinHandle<String> {
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("curl connects");
            let mut request = Vec::new();
            let mut buf = [0u8; 1024];
            loop {
                let read = stream.read(&mut buf).expect("the request reads");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buf[..read]);
                let text = String::from_utf8_lossy(&request);
                if let Some(head_end) = text.find("\r\n\r\n") {
                    let declared: usize = text[..head_end]
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|value| value.trim().parse().expect("a numeric length"))
                        })
                        .unwrap_or(0);
                    if request.len() >= head_end + 4 + declared {
                        break;
                    }
                }
            }
            let reason = if status == 200 { "OK" } else { "Unauthorized" };
            write!(
                stream,
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .expect("the response writes");
            String::from_utf8_lossy(&request).into_owned()
        })
    }

    /// A one-shot HTTP stub whose response promises more body than it sends
    /// and closes, so curl fails the transfer after the headers were read.
    fn respond_cut(listener: TcpListener, status: u16) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("curl connects");
            let mut head = String::new();
            let mut buf = [0u8; 1024];
            while !head.contains("\r\n\r\n") {
                let read = stream.read(&mut buf).expect("the request reads");
                if read == 0 {
                    break;
                }
                head.push_str(&String::from_utf8_lossy(&buf[..read]));
            }
            let reason = if status == 200 {
                "OK"
            } else {
                "Service Unavailable"
            };
            write!(
                stream,
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n\
                 Content-Length: 400\r\nConnection: close\r\n\r\n{{\"error\":{{\"mess"
            )
            .expect("the partial response writes");
        })
    }

    /// Run the workflow's credential check against `endpoint`. The stubs live
    /// on loopback, so the harness bypasses any proxy the host sets — the
    /// workflow itself must keep curl's normal proxy handling.
    fn run_credential_check(endpoint: &str) -> std::process::Output {
        run_credential_check_with(endpoint, "test-key")
    }

    /// Run the workflow's credential check against `endpoint` with `key`.
    fn run_credential_check_with(endpoint: &str, key: &str) -> std::process::Output {
        let workflow = workflow();
        let script = run_block(step(&workflow, "Check the gate credential"));
        // A directory per call: the tests run in parallel, and each run writes
        // `gate-preflight.json` into it.
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "cola-codex-gate-credential-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Command::new("bash")
            .arg("-c")
            .arg(script)
            .env("RUNNER_TEMP", dir)
            .env("GATE_ENDPOINT", endpoint)
            .env("GATE_MODEL", "test-model")
            .env("GATE_KEY", key)
            .env("no_proxy", "*")
            .env("NO_PROXY", "*")
            .output()
            .expect("bash runs")
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

    /// GitHub fills a PR body from the template, so the template's own text is
    /// collector input: a worked example would be read as this PR's reference,
    /// seed an unrelated issue as the review's spec, and satisfy the
    /// no-reference failure the collector exists to raise (#555). The template
    /// therefore spells its examples `#N`, and this pins that.
    #[test]
    fn the_pr_template_seeds_no_issue_reference() {
        let template =
            std::fs::read_to_string(crate::repo_root().join(".github/pull_request_template.md")).unwrap();
        assert_eq!(
            run_stdin("codex_issue_refs_stdin", &template).trim(),
            "",
            "the PR template must not carry a reference the collector picks up"
        );
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
    fn the_gate_checks_its_credential_before_codex_runs() {
        let workflow = workflow();
        // A dead key must name itself (#776): the preflight runs before the
        // sandboxed run, reads the same endpoint/model the action does, and
        // fails on anything but 200.
        let check = step(&workflow, "Check the gate credential");
        for needle in [
            "$GATE_KEY",
            "$GATE_ENDPOINT",
            "$GATE_MODEL",
            "%{http_code}",
            // The bound: without it a stalled endpoint holds the run until the
            // job's own timeout.
            "--max-time 30",
            "exit 1",
        ] {
            assert!(check.contains(needle), "the credential check must use {needle}");
        }
        let check_at = workflow.find("- name: Check the gate credential").unwrap();
        let codex_at = workflow.find("- name: Run Codex (read-only)").unwrap();
        assert!(
            check_at < codex_at,
            "the credential check must precede the Codex run"
        );
        // One definition of the provider, shared by the check and the action:
        // the job env declares both (six-space indentation — a step env would
        // be deeper), and the action steps read the declarations.
        for declared in ["\n      GATE_ENDPOINT: ", "\n      GATE_MODEL: "] {
            assert!(workflow.contains(declared), "the job env must declare {declared}");
        }
        assert!(
            workflow.contains("responses-api-endpoint: ${{ env.GATE_ENDPOINT }}"),
            "the Codex step must read the shared endpoint"
        );
        assert!(
            workflow.contains("model: ${{ env.GATE_MODEL }}"),
            "the Codex step must read the shared model"
        );
    }

    #[test]
    fn the_credential_check_fails_on_a_dead_key_and_names_it() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let request = respond_once(
            listener,
            401,
            r#"{"error":{"message":"This API key has expired."}}"#,
        );
        let out = run_credential_check(&format!("http://{addr}/v1/responses"));
        let log = String::from_utf8_lossy(&out.stdout);
        assert!(!out.status.success(), "a 401 must fail the step");
        assert!(log.contains("HTTP 401"), "the step must name the status: {log}");
        assert!(
            log.contains(&addr.to_string()),
            "the step must name the endpoint: {log}"
        );
        assert!(
            log.contains("This API key has expired."),
            "the step must carry the body, so the reason is the upstream's: {log}"
        );
        let seen = request.join().unwrap();
        assert!(
            seen.contains("Authorization: Bearer test-key"),
            "the step must send the workflow's key"
        );
        assert!(
            seen.to_lowercase()
                .contains("x-opencode-session: cola-codex-gate-preflight"),
            "the Go lane rejects a request without a session header: {seen}"
        );
        for field in [
            r#""model":"test-model""#,
            r#""input":"ping""#,
            r#""max_output_tokens":16"#,
        ] {
            assert!(seen.contains(field), "the probe must send {field}: {seen}");
        }
    }

    #[test]
    fn the_credential_check_refuses_an_empty_key() {
        let out = run_credential_check_with("http://127.0.0.1:1/v1/responses", "");
        let log = String::from_utf8_lossy(&out.stdout);
        assert!(!out.status.success(), "an empty key must fail the step");
        assert!(
            log.contains("OPENCODE_API_KEY is empty"),
            "the step must name the missing secret: {log}"
        );
    }

    #[test]
    fn the_credential_check_names_an_unreachable_endpoint() {
        // Bind and drop, so the port is closed: curl records no status and
        // exits on the connection failure, which is a different report from an
        // endpoint that answered with a status.
        let addr = {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap()
        };
        let out = run_credential_check(&format!("http://{addr}/v1/responses"));
        let log = String::from_utf8_lossy(&out.stdout);
        assert!(
            !out.status.success(),
            "an unreachable endpoint must fail the step"
        );
        assert!(
            log.contains("could not reach") && log.contains(&addr.to_string()),
            "the transport failure must name the endpoint: {log}"
        );
    }

    #[test]
    fn the_credential_check_passes_on_a_200() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let request = respond_once(listener, 200, r#"{"id":"resp_test","status":"incomplete"}"#);
        let out = run_credential_check(&format!("http://{addr}/v1/responses"));
        assert!(
            out.status.success(),
            "a 200 must pass, whatever the answer's status: {}",
            String::from_utf8_lossy(&out.stdout)
        );
        assert!(request.join().unwrap().starts_with("POST /v1/responses"));
    }

    #[test]
    fn the_credential_check_fails_when_a_200_body_is_cut() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let stub = respond_cut(listener, 200);
        let out = run_credential_check(&format!("http://{addr}/v1/responses"));
        let log = String::from_utf8_lossy(&out.stdout);
        stub.join().unwrap();
        assert!(
            !out.status.success(),
            "a 200 whose body never completed is not a healthy credential: {log}"
        );
        assert!(
            log.contains("HTTP 200"),
            "the cut response still names the status it sent: {log}"
        );
    }

    #[test]
    fn the_credential_check_keeps_the_status_when_the_body_is_cut() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let stub = respond_cut(listener, 503);
        let out = run_credential_check(&format!("http://{addr}/v1/responses"));
        let log = String::from_utf8_lossy(&out.stdout);
        stub.join().unwrap();
        assert!(!out.status.success(), "a 503 must fail the step");
        assert!(
            log.contains("HTTP 503"),
            "a cut body must not hide the status the endpoint sent: {log}"
        );
        assert!(
            !log.contains("could not reach"),
            "the endpoint was reached, so the failure is not a transport one: {log}"
        );
    }

    #[test]
    fn the_workflow_uses_the_shared_scripts() {
        let workflow = workflow();
        // The Codex step itself: the bound and the pin must stay, or a hang
        // burns the whole job and discards a finished review again (#530).
        // The live salvage path is exercised by the gate's own runs on every
        // PR; this guards the wiring against a silent revert.
        //
        // The step cap was raised from 12 to 30 minutes so a large diff plus
        // its embedded linked issues can finish (a 44-commit PR timed out at
        // 12m15s); it stays well inside the job's own cap, which is what
        // keeps a hang from burning the job.
        let codex = step(&workflow, "Run Codex (read-only)");
        for needle in [
            "bdf19a4a223ec2549a3e2274a0cf61556bc07675", // v1.13: bounds #150/#169's stdio hang
            "continue-on-error: true",
            "timeout-minutes: 30",
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
