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

use super::discovery::{DEFAULT_SERVER_USERNAME, ResolvedServer, ServerCandidate};

/// Probe a discovered candidate and build the attachment, or return the probe
/// evidence when the generation could not be resolved.
pub(crate) async fn resolve_candidate(
    candidate: &ServerCandidate,
    override_: GenerationOverride,
) -> Result<ResolvedServer, String> {
    let url = format!("http://localhost:{}", candidate.port);
    let probe = generation::probe(&url, &candidate.username, &candidate.password).await;
    resolve_probe(&url, &candidate.username, &candidate.password, override_, probe)
}

/// The decision half of [`resolve_candidate`], split out so the override
/// precedence and the logging are unit-testable without HTTP.
fn resolve_probe(
    url: &str,
    username: &str,
    password: &str,
    override_: GenerationOverride,
    probe: ProbeOutcome,
) -> Result<ResolvedServer, String> {
    match generation::decide(override_, &probe) {
        Some(generation) => {
            generation::log_attached(url, override_, &probe, generation);
            Ok(ResolvedServer {
                url: url.to_string(),
                // V2 hardcodes the username `opencode` and ignores
                // `OPENCODE_SERVER_USERNAME` (spec #364 §2): never hand a V2
                // server an env-derived username, not even when the override
                // forced the generation past an inconclusive probe.
                username: match generation {
                    Generation::V2 => DEFAULT_SERVER_USERNAME.to_string(),
                    Generation::V1 => username.to_string(),
                },
                password: password.to_string(),
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

    fn v2_probe() -> ProbeOutcome {
        ProbeOutcome {
            generation: Some(Generation::V2),
            reachable: true,
            evidence: "GET http://localhost:49374/api/info -> 200 application/json (V2 info envelope, version 2.0.18)".into(),
        }
    }

    #[test]
    fn a_classified_probe_builds_the_attachment_with_its_generation() {
        let resolved = resolve_probe(
            "http://localhost:49374",
            "opencode",
            "pw",
            GenerationOverride::Auto,
            v2_probe(),
        )
        .expect("a classified probe attaches");

        assert_eq!(resolved.url, "http://localhost:49374");
        assert_eq!(resolved.username, "opencode");
        assert_eq!(resolved.password, "pw");
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
        let resolved = resolve_probe(
            "http://localhost:49374",
            "poison",
            "pw",
            GenerationOverride::V2,
            probe,
        )
        .expect("the override forces the attach");
        assert_eq!(resolved.username, DEFAULT_SERVER_USERNAME);

        // V1 keeps the server's own effective username (auth.ts honors it).
        let v1 = ProbeOutcome {
            generation: Some(Generation::V1),
            reachable: true,
            evidence: "GET http://localhost:4096/api/info -> 200 text/html (V1 web UI catch-all)".into(),
        };
        let resolved = resolve_probe(
            "http://localhost:4096",
            "admin",
            "pw",
            GenerationOverride::Auto,
            v1,
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
            "http://localhost:49374",
            "opencode",
            "pw",
            GenerationOverride::Auto,
            probe,
        )
        .expect_err("an inconclusive probe must not attach");
        assert!(error.contains("503"), "evidence must travel: {error}");
    }

    #[test]
    fn an_unreachable_server_is_never_attached_even_when_forced() {
        let probe = ProbeOutcome {
            generation: None,
            reachable: false,
            evidence: "GET http://localhost:49374/api/info failed: connection refused".into(),
        };
        assert!(
            resolve_probe(
                "http://localhost:49374",
                "opencode",
                "pw",
                GenerationOverride::V2,
                probe
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn a_contradicting_override_still_attaches_as_the_forced_generation() {
        let (resolved, logs) = capture_logs(async {
            resolve_probe(
                "http://localhost:49374",
                "opencode",
                "pw",
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
                    assert_eq!(resolved.url, format!("http://localhost:{}", candidate.port));

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
