//! Attach-time generation detection (spec #364 §2, ADR-0055).
//!
//! The discriminator is a plain HTTP probe of `GET /api/info` with the
//! credentials the attach already holds. One rule, and every record states it
//! the same way:
//!
//! - **200 + V2's JSON info envelope = V2.** The envelope is recognised by its
//!   `version` string and `urls` array (V2 serves `{version, pid, urls,
//!   paths}`; only those two keys are required).
//! - **200 `text/html` = V1.** Measured against real V1 servers: V1 answers
//!   unknown paths with its web UI through a catch-all, so `GET /api/info` on
//!   V1 is a 200 HTML page, not a 404 (verified against the running 1.18.23
//!   and the pinned 1.18.31).
//! - **404 = V1** (no `/api` surface at all).
//! - **Everything else = inconclusive**: a 200 without the envelope (JSON or
//!   any other content type), 401, 503, a transport error. An inconclusive
//!   probe is never guessed at — cola stays serverless and Lazy Start / the
//!   reconnect scan retries (spec #364 §2).
//!
//! The `[opencode] generation` override exists for proxies and unusual builds
//! where the probe cannot be trusted (GLOSSARY.md "Generation Override").

use std::time::Duration;

use crate::config::GenerationOverride;

use super::strategy::Generation;
use super::transport::Transport;

/// The V2 info route: the probe's one request.
pub(crate) const INFO_PATH: &str = "/api/info";

/// Bound the probe's whole request (connect + response): the official V2
/// client probes with a 5 s timeout, and an attach must not hang on a wedged
/// server.
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// What one `GET /api/info` probe observed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProbeOutcome {
    /// The generation the probe classified; `None` means inconclusive.
    pub(crate) generation: Option<Generation>,
    /// Whether the server answered HTTP at all (`false` = transport error or
    /// timeout). An explicit override may force past an inconclusive answer,
    /// but never past an unreachable server: there is nothing to attach to.
    pub(crate) reachable: bool,
    /// One-line evidence for the attach log and for failure messages.
    pub(crate) evidence: String,
}

impl ProbeOutcome {
    fn classified(generation: Generation, evidence: String) -> Self {
        Self {
            generation: Some(generation),
            reachable: true,
            evidence,
        }
    }

    fn inconclusive(evidence: String) -> Self {
        Self {
            generation: None,
            reachable: true,
            evidence,
        }
    }

    fn unreachable(evidence: String) -> Self {
        Self {
            generation: None,
            reachable: false,
            evidence,
        }
    }
}

/// Probe a server's generation over the caller's transport (the credential
/// holder the attach path built) — tests point it at the fake HTTP server with
/// the env proxy disabled so a developer shell's `http_proxy` cannot intercept
/// loopback (the wire-test pattern, ADR-0031).
pub(crate) async fn probe_transport(transport: &Transport) -> ProbeOutcome {
    let url = transport.url(INFO_PATH);
    let attempt = async {
        let response = transport.client().get(&url).send().await?;
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let body = response.bytes().await?;
        Ok::<_, reqwest::Error>((status, content_type, body))
    };
    match tokio::time::timeout(PROBE_TIMEOUT, attempt).await {
        // Bound the whole exchange, response body included: a server that
        // accepts the connection then stalls must not hold the attach.
        Err(_) => {
            ProbeOutcome::unreachable(format!("GET {url} timed out after {}s", PROBE_TIMEOUT.as_secs()))
        }
        Ok(Err(e)) => ProbeOutcome::unreachable(format!("GET {url} failed: {e}")),
        Ok(Ok((status, content_type, body))) => classify(status, content_type.as_deref(), &body, &url),
    }
}

/// Turn one HTTP response into a probe outcome. The status alone is not
/// enough (V1's UI catch-all answers 200 to `/api/info`), so a 200 is V2 only
/// when the body is V2's info envelope, V1 when it is the HTML catch-all, and
/// inconclusive otherwise; a 404 is V1 and every other status is judged by its
/// own evidence.
fn classify(status: u16, content_type: Option<&str>, body: &[u8], url: &str) -> ProbeOutcome {
    let content_type = content_type.unwrap_or("-");
    match status {
        200 => match v2_info_version(body) {
            Some(version) => ProbeOutcome::classified(
                Generation::V2,
                format!("GET {url} -> 200 {content_type} (V2 info envelope, version {version})"),
            ),
            None if content_type.starts_with("text/html") => ProbeOutcome::classified(
                Generation::V1,
                format!("GET {url} -> 200 {content_type} (V1 web UI catch-all, not a V2 info envelope)"),
            ),
            None => ProbeOutcome::inconclusive(format!(
                "GET {url} -> 200 {content_type} (not a V2 info envelope)"
            )),
        },
        404 => ProbeOutcome::classified(Generation::V1, format!("GET {url} -> 404 (no V2 /api route)")),
        401 | 403 => ProbeOutcome::inconclusive(format!("GET {url} -> {status} (credentials rejected)")),
        503 => ProbeOutcome::inconclusive(format!("GET {url} -> 503 (server not ready; possibly migrating)")),
        other => ProbeOutcome::inconclusive(format!("GET {url} -> {other} {content_type}")),
    }
}

/// The `version` of V2's info envelope, when `body` is that envelope. The
/// distinctive pair (`version` string + `urls` array) guards against a random
/// JSON 200 (e.g. a proxy error page) being mistaken for V2.
fn v2_info_version(body: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let version = value.get("version")?.as_str()?;
    value.get("urls")?.as_array()?;
    Some(version.to_string())
}

/// Apply the `[opencode] generation` override to the probe's outcome.
///
/// `auto` trusts only a classified probe. An explicit value forces the
/// strategy whenever the server is reachable — that is the escape hatch for a
/// proxy or an unusual build the probe cannot classify — but an unreachable
/// server stays serverless even when forced: there is nothing to attach to,
/// and Lazy Start / the reconnect scan retries.
pub(crate) fn decide(generation_override: GenerationOverride, probe: &ProbeOutcome) -> Option<Generation> {
    match generation_override {
        GenerationOverride::Auto => probe.generation,
        GenerationOverride::V1 if probe.reachable => Some(Generation::V1),
        GenerationOverride::V2 if probe.reachable => Some(Generation::V2),
        GenerationOverride::V1 | GenerationOverride::V2 => None,
    }
}

/// Log the attach decision: INFO with the positive probe evidence, and a WARN
/// when an explicit override contradicts a probe that did classify the server
/// (spec #364 §2 — "a contradicting probe logs a WARN with the evidence").
pub(crate) fn log_attached(
    url: &str,
    generation_override: GenerationOverride,
    probe: &ProbeOutcome,
    generation: Generation,
) {
    let forced = generation_override != GenerationOverride::Auto;
    if forced && probe.generation.is_some_and(|probed| probed != generation) {
        tracing::warn!(
            "generation override {} contradicts the probe: {}; forcing generation={}",
            generation_override,
            probe.evidence,
            generation.as_str()
        );
    }
    let note = if forced && probe.generation.is_none() {
        // The override is the reason this attachment exists at all: say so,
        // with the inconclusive evidence.
        format!(
            "generation override {}; probe: {}",
            generation_override, probe.evidence
        )
    } else {
        format!("probe: {}", probe.evidence)
    };
    tracing::info!(
        "Attached to OpenCode server at {url} — generation={} ({note})",
        generation.as_str()
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::test_support::{assert_line_level, capture_logs, level_count};
    use crate::test_http::TestHttpServer;

    /// A transport pointed at `url`, with the env proxy disabled so a
    /// developer shell's `http_proxy` cannot intercept the loopback probe.
    async fn probe_url(username: &str, password: &str, url: impl Into<String>) -> ProbeOutcome {
        let transport = Transport::new(Some(username), Some(password), url);
        transport.disable_env_proxy(Some(username), Some(password));
        probe_transport(&transport).await
    }

    /// A transport pointed at the fake server.
    async fn probe_server(server: &TestHttpServer, username: &str, password: &str) -> ProbeOutcome {
        probe_url(username, password, server.base_url()).await
    }

    /// How many fresh ports [`probe_refused_port`] tries before giving up.
    ///
    /// A dropped ephemeral listener does not reserve its port, so a parallel
    /// test's `TestHttpServer` can claim the fresh one before the probe
    /// arrives (issue #382). Each retry needs a fresh port, not a fresh wait:
    /// whatever claimed the old one is still there.
    const REFUSED_PROBE_ATTEMPTS: usize = 32;

    /// Probe a loopback port that has no listener and return the refusal.
    ///
    /// The port is not held open while probing: it is bound, its number read,
    /// and the listener dropped, so the OS may reuse it at any moment — in
    /// this binary, for the next `TestHttpServer` another test starts (issue
    /// #382). An answer therefore means the port was claimed, not that the
    /// classifier failed, and a fresh port is tried; a claimed port always
    /// answers immediately, so a stolen port retries at once. The retries are
    /// bounded so a machine where every port answers still fails loudly, and
    /// the message names both possibilities.
    async fn probe_refused_port(username: &str, password: &str) -> ProbeOutcome {
        for _ in 0..REFUSED_PROBE_ATTEMPTS {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            drop(listener);
            let outcome = probe_url(username, password, format!("http://127.0.0.1:{port}")).await;
            if !outcome.reachable {
                return outcome;
            }
        }
        panic!(
            "no refused port in {REFUSED_PROBE_ATTEMPTS} attempts: every dropped ephemeral port was answered (issue #382), or the probe misclassifies refused connections"
        );
    }

    #[tokio::test]
    async fn a_200_info_envelope_is_v2() {
        let server = TestHttpServer::start().await;
        server.route(
            "GET",
            "/api/info",
            200,
            r#"{"version":"2.0.18","pid":7494,"urls":["http://127.0.0.1:49374"],"paths":{"tmp":"/tmp/opencode"}}"#,
        );

        let outcome = probe_server(&server, "opencode", "pw").await;

        assert_eq!(outcome.generation, Some(Generation::V2));
        assert!(outcome.reachable);
        assert!(
            outcome.evidence.contains("2.0.18"),
            "evidence: {}",
            outcome.evidence
        );
        let request = server.requests().pop().expect("the probe sent a request");
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, "/api/info");
    }

    /// The measured V1 behaviour: its web UI catch-all answers `200 text/html`
    /// to `/api/info` (1.18.23 running and 1.18.31 pinned both do), so the
    /// status alone cannot discriminate and the body must.
    #[tokio::test]
    async fn a_200_html_ui_page_is_v1() {
        let server = TestHttpServer::start().await;
        server.route_raw(
            "GET",
            "/api/info",
            200,
            "text/html",
            "<!doctype html><title>OpenCode</title>",
        );

        let outcome = probe_server(&server, "opencode", "pw").await;

        assert_eq!(outcome.generation, Some(Generation::V1));
        assert!(
            outcome.evidence.contains("V1 web UI"),
            "evidence: {}",
            outcome.evidence
        );
    }

    #[tokio::test]
    async fn a_404_is_v1() {
        let server = TestHttpServer::start().await;
        server.route_raw(
            "GET",
            "/api/info",
            404,
            "application/json",
            r#"{"error":"Not Found"}"#,
        );

        let outcome = probe_server(&server, "opencode", "pw").await;

        assert_eq!(outcome.generation, Some(Generation::V1));
        assert!(outcome.evidence.contains("404"), "evidence: {}", outcome.evidence);
    }

    #[tokio::test]
    async fn a_401_is_inconclusive() {
        let server = TestHttpServer::start().await;
        server.route_raw(
            "GET",
            "/api/info",
            401,
            "application/json",
            r#"{"error":"Unauthorized"}"#,
        );

        let outcome = probe_server(&server, "wrong", "pw").await;

        assert_eq!(outcome.generation, None);
        assert!(outcome.reachable);
        assert!(outcome.evidence.contains("401"), "evidence: {}", outcome.evidence);
        assert!(
            outcome.evidence.contains("credentials"),
            "evidence: {}",
            outcome.evidence
        );
    }

    #[tokio::test]
    async fn a_503_during_migration_is_inconclusive() {
        let server = TestHttpServer::start().await;
        server.route_raw("GET", "/api/info", 503, "text/plain", "migrating");

        let outcome = probe_server(&server, "opencode", "pw").await;

        assert_eq!(outcome.generation, None);
        assert!(outcome.reachable);
        assert!(outcome.evidence.contains("503"), "evidence: {}", outcome.evidence);
        assert!(
            outcome.evidence.contains("migrating"),
            "evidence: {}",
            outcome.evidence
        );
    }

    #[tokio::test]
    async fn a_200_without_the_envelope_is_inconclusive_not_v1() {
        // A JSON 200 that is not V2's info is unclassifiable: never guess.
        let server = TestHttpServer::start().await;
        server.route("GET", "/api/info", 200, r#"{"error":"gateway"}"#);

        let outcome = probe_server(&server, "opencode", "pw").await;

        assert_eq!(outcome.generation, None);
        assert!(outcome.reachable);
        assert!(outcome.evidence.contains("not a V2 info envelope"));
    }

    /// The evidence string flows into the attach log (and, on an inconclusive
    /// probe, into a `BridgeError` that can reach a card): it must never carry
    /// the Basic-auth password.
    #[tokio::test]
    async fn probe_evidence_never_carries_the_password() {
        const PASSWORD: &str = "super-secret-probe-pw";
        let cases: &[(u16, &str, &str)] = &[
            (200, "application/json", r#"{"version":"2.0.18","urls":[]}"#),
            (200, "text/html", "<!doctype html>"),
            (200, "application/json", r#"{"error":"gateway"}"#),
            (401, "application/json", r#"{"error":"Unauthorized"}"#),
            (503, "text/plain", "migrating"),
            (404, "application/json", r#"{"error":"Not Found"}"#),
        ];
        for &(status, content_type, body) in cases {
            let server = TestHttpServer::start().await;
            server.route_raw("GET", "/api/info", status, content_type, body);
            let outcome = probe_server(&server, "opencode", PASSWORD).await;
            // Positive control first: the absence check must not pass
            // vacuously on an empty evidence string.
            assert!(
                !outcome.evidence.is_empty(),
                "status {status} produced no evidence to inspect"
            );
            assert!(
                !outcome.evidence.contains(PASSWORD),
                "status {status} leaked the password: {}",
                outcome.evidence
            );
        }

        // The unreachable path builds its evidence from the transport error,
        // which can echo the URL but never the credentials. The refused port
        // is retried because a parallel test can claim the dropped one (#382).
        let outcome = probe_refused_port("opencode", PASSWORD).await;
        assert!(
            !outcome.evidence.is_empty(),
            "the unreachable path produced no evidence to inspect"
        );
        assert!(
            !outcome.evidence.contains(PASSWORD),
            "the unreachable evidence leaked the password: {}",
            outcome.evidence
        );
    }

    #[tokio::test]
    async fn an_unreachable_server_is_inconclusive() {
        // A port with no listener: connection refused, fast. No-proxy so a
        // developer shell's `http_proxy` cannot answer in the dead port's
        // place (the proxy would make the server look reachable). The helper
        // retries with a fresh port when a parallel test claims the dropped
        // one before the probe arrives (issue #382).
        let outcome = probe_refused_port("opencode", "pw").await;

        assert_eq!(outcome.generation, None);
        assert!(!outcome.reachable, "a refused connection is not reachable");
        assert!(
            outcome.evidence.contains("failed"),
            "evidence: {}",
            outcome.evidence
        );
    }

    #[test]
    fn auto_trusts_only_a_classified_probe() {
        let v2 = ProbeOutcome::classified(Generation::V2, "ev".into());
        let v1 = ProbeOutcome::classified(Generation::V1, "ev".into());
        let inconclusive = ProbeOutcome::inconclusive("ev".into());
        let unreachable = ProbeOutcome::unreachable("ev".into());

        assert_eq!(decide(GenerationOverride::Auto, &v2), Some(Generation::V2));
        assert_eq!(decide(GenerationOverride::Auto, &v1), Some(Generation::V1));
        assert_eq!(decide(GenerationOverride::Auto, &inconclusive), None);
        assert_eq!(decide(GenerationOverride::Auto, &unreachable), None);
    }

    #[test]
    fn an_explicit_override_forces_on_any_reachable_answer() {
        let conflicting = ProbeOutcome::classified(Generation::V1, "ev".into());
        let inconclusive = ProbeOutcome::inconclusive("ev".into());

        // The override wins over a disagreeing probe...
        assert_eq!(decide(GenerationOverride::V2, &conflicting), Some(Generation::V2));
        // ...and over an inconclusive one (the proxy/odd-build escape hatch).
        assert_eq!(
            decide(GenerationOverride::V1, &inconclusive),
            Some(Generation::V1)
        );
        assert_eq!(
            decide(GenerationOverride::V2, &inconclusive),
            Some(Generation::V2)
        );
    }

    #[test]
    fn an_explicit_override_never_attaches_to_an_unreachable_server() {
        // Nothing answered: no generation, forced or not (Lazy Start retries).
        let unreachable = ProbeOutcome::unreachable("ev".into());
        assert_eq!(decide(GenerationOverride::V1, &unreachable), None);
        assert_eq!(decide(GenerationOverride::V2, &unreachable), None);
    }

    #[tokio::test]
    async fn attach_logs_info_with_the_evidence() {
        let outcome = ProbeOutcome::classified(
            Generation::V2,
            "GET http://localhost:49374/api/info -> 200 application/json (V2 info envelope, version 2.0.18)"
                .into(),
        );
        let (_, logs) = capture_logs(async {
            log_attached(
                "http://localhost:49374",
                GenerationOverride::Auto,
                &outcome,
                Generation::V2,
            );
        })
        .await;

        let line = assert_line_level(&logs, "Attached to OpenCode server", "INFO");
        assert!(line.contains("generation=v2"), "line: {line}");
        assert!(line.contains("probe: GET"), "line: {line}");
        assert!(line.contains("version 2.0.18"), "line: {line}");
        assert_eq!(level_count(&logs, "contradicts the probe", "WARN"), 0);
    }

    #[tokio::test]
    async fn a_contradicting_override_warns_with_the_evidence() {
        let outcome = ProbeOutcome::classified(
            Generation::V2,
            "GET http://localhost:49374/api/info -> 200 application/json (V2 info envelope, version 2.0.18)"
                .into(),
        );
        let (_, logs) = capture_logs(async {
            log_attached(
                "http://localhost:49374",
                GenerationOverride::V1,
                &outcome,
                Generation::V1,
            );
        })
        .await;

        let line = assert_line_level(&logs, "contradicts the probe", "WARN");
        assert!(line.contains("override v1"), "line: {line}");
        assert!(line.contains("version 2.0.18"), "line: {line}");
        assert!(line.contains("forcing generation=v1"), "line: {line}");
        // The attach itself is still recorded, with the forced generation.
        let attached = assert_line_level(&logs, "Attached to OpenCode server", "INFO");
        assert!(attached.contains("generation=v1"), "line: {attached}");
    }

    #[tokio::test]
    async fn a_forced_inconclusive_attach_says_the_override_is_the_reason() {
        let outcome = ProbeOutcome::inconclusive(
            "GET http://localhost:49374/api/info -> 401 (credentials rejected)".into(),
        );
        let (_, logs) = capture_logs(async {
            log_attached(
                "http://localhost:49374",
                GenerationOverride::V2,
                &outcome,
                Generation::V2,
            );
        })
        .await;

        let line = assert_line_level(&logs, "Attached to OpenCode server", "INFO");
        assert!(line.contains("generation=v2"), "line: {line}");
        assert!(line.contains("generation override v2"), "line: {line}");
        assert!(line.contains("401"), "line: {line}");
        assert_eq!(level_count(&logs, "contradicts the probe", "WARN"), 0);
    }
}
