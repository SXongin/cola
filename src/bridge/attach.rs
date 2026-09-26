//! Attach-time generation resolution (spec #364 §2, ADR-0055).
//!
//! Discovery produces candidate servers; this module is the one place that
//! turns a candidate into an attachment: probe `GET /api/info` with the
//! credentials discovery resolved, apply the `[opencode] generation` override
//! ([`crate::opencode::generation`]), log the evidence, and carry the resolved
//! generation on the [`ResolvedServer`].
//!
//! An inconclusive probe returns its evidence as an error: the caller stays
//! serverless and Lazy Start / the reconnect scan retries later. A generation
//! is never guessed (spec #364 §2).

use crate::config::GenerationOverride;
use crate::opencode::generation::{self, ProbeOutcome};
use crate::opencode::strategy::Generation;
use crate::opencode::transport::Transport;

use super::discovery::{DEFAULT_SERVER_USERNAME, ResolvedServer, ServerCandidate};

/// Probe a discovered candidate and build the attachment, or return the probe
/// evidence when the generation could not be resolved.
pub(crate) async fn resolve_candidate(
    candidate: &ServerCandidate,
    generation_override: GenerationOverride,
) -> Result<ResolvedServer, String> {
    let transport = Transport::new(
        Some(&candidate.username),
        Some(&candidate.password),
        candidate.url(),
    );
    resolve_candidate_over(&transport, candidate, generation_override).await
}

/// [`resolve_candidate`] over a caller-built transport, so tests can point the
/// whole attach path at the fake HTTP server without a developer shell's
/// `http_proxy` intercepting loopback (the wire-test pattern, ADR-0031).
pub(crate) async fn resolve_candidate_over(
    transport: &Transport,
    candidate: &ServerCandidate,
    generation_override: GenerationOverride,
) -> Result<ResolvedServer, String> {
    let probe = generation::probe_transport(transport).await;
    resolve_probe(candidate, generation_override, probe)
}

/// The decision half of [`resolve_candidate_over`], split out so the override
/// precedence and the logging are unit-testable without HTTP.
fn resolve_probe(
    candidate: &ServerCandidate,
    generation_override: GenerationOverride,
    probe: ProbeOutcome,
) -> Result<ResolvedServer, String> {
    match generation::decide(generation_override, &probe) {
        Some(generation) => {
            generation::log_attached(&candidate.url(), generation_override, &probe, generation);
            Ok(ResolvedServer {
                url: candidate.url(),
                // V2 hardcodes the username `opencode` and ignores
                // `OPENCODE_SERVER_USERNAME` (spec #364 §2): never hand a V2
                // server an env-derived username, not even when the override
                // forced the generation past an inconclusive probe.
                username: match generation {
                    Generation::V2 => DEFAULT_SERVER_USERNAME.to_string(),
                    Generation::V1 => candidate.username.clone(),
                },
                password: candidate.password.clone(),
                pid: Some(candidate.pid),
                generation,
            })
        }
        None => Err(probe.evidence),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::discovery::scan_processes;
    use crate::bridge::test_support::{assert_line_level, capture_logs, level_count, line_with};
    use crate::opencode::strategy::Generation;
    use crate::test_http::TestHttpServer;

    fn candidate(port: u16, username: &str, password: &str) -> ServerCandidate {
        ServerCandidate {
            pid: 4242,
            port,
            username: username.to_string(),
            password: password.to_string(),
            uses_default_store: true,
        }
    }

    fn v2_probe() -> ProbeOutcome {
        ProbeOutcome {
            generation: Some(Generation::V2),
            reachable: true,
            evidence: "GET http://localhost:49374/api/info -> 200 application/json (V2 info envelope, version 2.0.18)".into(),
        }
    }

    fn v1_html_probe() -> ProbeOutcome {
        ProbeOutcome {
            generation: Some(Generation::V1),
            reachable: true,
            evidence: "GET http://localhost:4096/api/info -> 200 text/html (V1 web UI catch-all, not a V2 info envelope)".into(),
        }
    }

    #[test]
    fn a_classified_probe_builds_the_attachment_with_its_generation() {
        let resolved = resolve_probe(
            &candidate(49374, "opencode", "pw"),
            GenerationOverride::Auto,
            v2_probe(),
        )
        .expect("a classified probe attaches");

        assert_eq!(resolved.url, "http://localhost:49374");
        assert_eq!(resolved.username, "opencode");
        assert_eq!(resolved.password, "pw");
        assert_eq!(resolved.pid, Some(4242), "the attachment carries its identity");
        assert_eq!(resolved.generation, Generation::V2);
    }

    #[test]
    fn a_v2_attachment_never_keeps_an_env_derived_username() {
        // The override can force V2 past an inconclusive probe; V2 hardcodes
        // `opencode`, so an env-derived username must not survive the attach.
        let probe = ProbeOutcome {
            generation: None,
            reachable: true,
            evidence: "GET http://localhost:49374/api/info -> 401 (credentials rejected)".into(),
        };
        let resolved = resolve_probe(&candidate(49374, "poison", "pw"), GenerationOverride::V2, probe)
            .expect("the override forces the attach");
        assert_eq!(resolved.username, DEFAULT_SERVER_USERNAME);

        // V1 keeps the server's own effective username (auth.ts honors it).
        let resolved = resolve_probe(
            &candidate(4096, "admin", "pw"),
            GenerationOverride::Auto,
            v1_html_probe(),
        )
        .expect("a classified V1 probe attaches");
        assert_eq!(resolved.username, "admin");
    }

    #[test]
    fn an_inconclusive_probe_under_auto_returns_the_evidence_and_no_attachment() {
        let probe = ProbeOutcome {
            generation: None,
            reachable: true,
            evidence: "GET http://localhost:49374/api/info -> 503 (server not ready; possibly migrating)"
                .into(),
        };
        let error = resolve_probe(
            &candidate(49374, "opencode", "pw"),
            GenerationOverride::Auto,
            probe,
        )
        .expect_err("an inconclusive probe must not attach");
        assert!(error.contains("503"), "evidence must travel: {error}");
        assert!(!error.contains("pw"), "the credential must not: {error}");
    }

    #[test]
    fn an_unreachable_server_is_never_attached_even_when_forced() {
        let probe = ProbeOutcome {
            generation: None,
            reachable: false,
            evidence: "GET http://localhost:49374/api/info failed: connection refused".into(),
        };
        assert!(resolve_probe(&candidate(49374, "opencode", "pw"), GenerationOverride::V2, probe).is_err());
    }

    #[tokio::test]
    async fn a_contradicting_override_still_attaches_as_the_forced_generation() {
        let (resolved, logs) = capture_logs(async {
            resolve_probe(
                &candidate(49374, "opencode", "pw"),
                GenerationOverride::V1,
                v2_probe(),
            )
        })
        .await;
        let resolved = resolved.expect("the override forces the attach");

        assert_eq!(resolved.generation, Generation::V1);
        assert_line_level(&logs, "contradicts the probe", "WARN");
        assert_eq!(level_count(&logs, "Attached to OpenCode server", "INFO"), 1);
    }

    /// A forced V2 against the measured V1 signature (200 `text/html` from the
    /// UI catch-all): the override wins, the contradiction is logged, and the
    /// V2 username normalization applies even though discovery read the env.
    #[tokio::test]
    async fn a_forced_v2_override_on_a_v1_html_probe_warns_and_attaches_v2() {
        let (resolved, logs) = capture_logs(async {
            resolve_probe(
                &candidate(49374, "poison", "pw"),
                GenerationOverride::V2,
                v1_html_probe(),
            )
        })
        .await;
        let resolved = resolved.expect("the override forces the attach");

        assert_eq!(resolved.generation, Generation::V2);
        assert_eq!(resolved.username, DEFAULT_SERVER_USERNAME);
        let warning = assert_line_level(&logs, "contradicts the probe", "WARN");
        assert!(warning.contains("V1 web UI catch-all"), "line: {warning}");
        assert!(
            assert_line_level(&logs, "Attached to OpenCode server", "INFO").contains("generation=v2"),
            "the attach must record the forced generation"
        );
    }

    /// The whole attach path against the fake server: V1's 200 `text/html`
    /// catch-all must resolve `v1`, not be mistaken for V2.
    #[tokio::test]
    async fn a_200_html_catch_all_resolves_v1_through_the_whole_attach_path() {
        let server = TestHttpServer::start().await;
        server.route_raw(
            "GET",
            "/api/info",
            200,
            "text/html",
            "<!doctype html><title>OpenCode</title>",
        );
        let candidate = candidate(server_port(&server), "opencode", "attach-secret");
        let transport = no_proxy_transport(&server, "opencode", "attach-secret");

        let (resolved, logs) = capture_logs(async {
            resolve_candidate_over(&transport, &candidate, GenerationOverride::Auto).await
        })
        .await;
        let resolved = resolved.expect("the V1 catch-all attaches");

        assert_eq!(resolved.generation, Generation::V1);
        assert_eq!(resolved.username, "opencode");
        assert_eq!(resolved.pid, Some(4242));
        assert_line_level(&logs, "Attached to OpenCode server", "INFO");
        assert!(
            !logs.contains("attach-secret"),
            "the credential must never reach the log: {logs}"
        );
    }

    /// The failure path of the whole attach path: the returned evidence must
    /// never carry the password (it flows into `BridgeError` and a card).
    #[tokio::test]
    async fn an_inconclusive_probe_error_never_carries_the_password() {
        let server = TestHttpServer::start().await;
        server.route_raw("GET", "/api/info", 503, "text/plain", "migrating");
        let candidate = candidate(server_port(&server), "opencode", "attach-secret");
        let transport = no_proxy_transport(&server, "opencode", "attach-secret");

        let error = resolve_candidate_over(&transport, &candidate, GenerationOverride::Auto)
            .await
            .expect_err("503 is inconclusive");

        assert!(error.contains("503"), "unexpected: {error}");
        assert!(!error.contains("attach-secret"), "the credential leaked: {error}");
    }

    /// The fake server's listening port, as the scan would report it.
    fn server_port(server: &TestHttpServer) -> u16 {
        crate::config::port_from_url(&server.base_url()).expect("the fake server URL has a port")
    }

    /// A transport pointing at the fake server with the env proxy disabled, so
    /// a developer shell's `http_proxy` cannot intercept loopback.
    fn no_proxy_transport(server: &TestHttpServer, username: &str, password: &str) -> Transport {
        let transport = Transport::new(Some(username), Some(password), server.base_url());
        transport.disable_env_proxy(Some(username), Some(password));
        transport
    }

    /// Live check (manual): detect every OpenCode server running on this
    /// machine. With the pinned V1 server up it must resolve `v1` from its
    /// 200-`text/html` UI catch-all evidence; with the isolated V2 service up
    /// (`opencode-v2 service start`) it must resolve `v2` from the
    /// registration-file credentials — regardless of a poisoned
    /// `OPENCODE_SERVER_USERNAME` in the daemon's environment — and, when
    /// forced to `v1`, attach as V1 with a contradicting-probe WARN.
    ///
    /// Ignored because it needs machine-local servers; it skips cleanly when
    /// none is running. Deliberately not named `live*` — the hermetic CI live
    /// filter (`cargo test -- --ignored live`) must never pick it up.
    #[tokio::test]
    #[ignore = "needs local OpenCode servers (opencode serve / opencode-v2 service start)"]
    async fn detects_the_running_servers_generations() {
        let mut v1 = false;
        let mut v2 = false;
        for candidate in scan_processes() {
            let Ok(resolved) = resolve_candidate(&candidate, GenerationOverride::Auto).await else {
                continue;
            };
            match resolved.generation {
                Generation::V1 => {
                    // The V1 attach log carries the generation and the probe
                    // evidence (spec #364, S3 acceptance).
                    let (again, logs) =
                        capture_logs(async { resolve_candidate(&candidate, GenerationOverride::Auto).await })
                            .await;
                    assert!(again.is_ok());
                    let line = assert_line_level(&logs, "Attached to OpenCode server", "INFO");
                    assert!(line.contains("generation=v1"), "line: {line}");
                    assert!(line.contains("probe: GET"), "line: {line}");
                    assert!(
                        line_with(&logs, "Attached to OpenCode server").contains("/api/info"),
                        "line: {line}"
                    );
                    v1 = true;
                }
                Generation::V2 => {
                    // A V2 service's credentials come from the registration
                    // file, and its username is always `opencode`.
                    assert_eq!(resolved.username, DEFAULT_SERVER_USERNAME);
                    assert_eq!(resolved.url, candidate.url());

                    let (forced, logs) =
                        capture_logs(async { resolve_candidate(&candidate, GenerationOverride::V1).await })
                            .await;
                    let forced = forced.expect("the v1 override forces the attach");
                    assert_eq!(forced.generation, Generation::V1);
                    assert_line_level(&logs, "contradicts the probe", "WARN");
                    v2 = true;
                }
            }
        }
        assert!(
            v1 || v2,
            "skip: no running OpenCode server detected (start V1 or `opencode-v2 service start`)"
        );
        eprintln!("live attach detection: v1={v1} v2={v2}");
    }
}
