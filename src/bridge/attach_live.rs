//! The machine-local attach check: [`detects_the_running_servers_generations`].
//!
//! It walks the OpenCode servers actually running on this machine and attaches
//! to each one through [`resolve_candidate`], so it can only pass on a
//! developer box with a live V1 server or V2 service. It is `#[ignore]`d for
//! exactly that reason, and it lives here rather than in `attach.rs` because
//! none of its lines can ever execute in the hermetic `Coverage` job:
//! `codecov.yml` ignores this file by path, with the same rationale as the live
//! harness (ADR-0057). Deliberately not named `live*` — the CI live filters
//! (`cargo test -- --ignored live_v1` / `live_v2`) must never pick it up.

use crate::bridge::attach::resolve_candidate;
use crate::bridge::discovery::{DEFAULT_SERVER_USERNAME, scan_processes};
use crate::bridge::test_support::{assert_line_level, capture_logs, line_with};
use crate::config::GenerationOverride;
use crate::opencode::strategy::Generation;

/// Live check (manual): detect every OpenCode server running on this
/// machine. With the pinned V1 server up it must resolve `v1` from its
/// 200-`text/html` UI catch-all evidence; with the isolated V2 service up
/// (`opencode-v2 service start`) it must resolve `v2` from the
/// registration-file credentials — regardless of a poisoned
/// `OPENCODE_SERVER_USERNAME` in the daemon's environment — and, when
/// forced to `v1`, attach as V1 with a contradicting-probe WARN.
///
/// Ignored because it needs machine-local servers; it skips cleanly when
/// none is running.
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
                    capture_logs(async { resolve_candidate(&candidate, GenerationOverride::V1).await }).await;
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
