//! The pinned `codex app-server` surface and the probe that checks it
//! (spec #860, ticket #876; ADR-0090's probe-and-degrade consequence).
//!
//! Codex does not advertise its method list at runtime: `initialize`'s result
//! carries the binary's identity (`userAgent`, `codexHome`, platform) and
//! nothing else. The declared surface is therefore checked in as the reference
//! document generated from the pinned binary —
//! `docs/reference/codex-app-server-0.163.0.md`, produced by
//! `codex app-server generate-json-schema --out <dir>` — and the adapter reads
//! that document as its contract. A server reporting the pinned version is
//! checked against it directly.
//!
//! A server reporting another version is never assumed compatible. Its own
//! binary is asked for a schema (`generate-json-schema`) and the required
//! subset is checked against that; the binary's `--version` must match the
//! version the server reported, so a 0.163 CLI cannot vouch for a 0.164
//! daemon. A missing required method fails construction, named — never at the
//! first prompt. An older binary is refused outright.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use super::client::ServerVersion;
use super::error::{CodexError, Result};

/// The version the adapter was written against, and the version of the checked
/// in reference document.
pub(crate) const PINNED_VERSION: &str = "0.163.0";

/// Bound the two subprocesses a non-pinned server costs: `--version` plus
/// `generate-json-schema`. A probe that hangs must not hang the attach.
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// The methods this adapter calls. A surface missing any of them cannot serve
/// a single feature cola's Codex path promises, so construction fails.
pub(crate) const REQUIRED_CLIENT_METHODS: &[&str] = &[
    "initialize",
    "model/list",
    "skills/list",
    "thread/compact/start",
    "thread/fork",
    "thread/items/list",
    "thread/list",
    "thread/read",
    "thread/resume",
    "thread/start",
    "thread/turns/list",
    "thread/unsubscribe",
    "turn/interrupt",
    "turn/start",
    "turn/steer",
];

/// The notifications the read model, Session Sync and card withdrawal consume
/// (tickets #879, #882, #883).
pub(crate) const REQUIRED_NOTIFICATIONS: &[&str] = &[
    "error",
    "item/agentMessage/delta",
    "item/commandExecution/outputDelta",
    "item/completed",
    "item/fileChange/outputDelta",
    "item/fileChange/patchUpdated",
    "item/reasoning/summaryTextDelta",
    "item/reasoning/textDelta",
    "item/started",
    "serverRequest/resolved",
    "thread/compacted",
    "thread/started",
    "thread/status/changed",
    "thread/tokenUsage/updated",
    "turn/completed",
    "turn/diff/updated",
    "turn/started",
];

/// The server-initiated requests cola answers (the ten the ADR's correction
/// records). The three the adapter does not answer — account token refresh,
/// attestation and dynamic `item/tool/call` — are part of the pinned surface
/// but not part of this required set: a server that never sends them needs no
/// handler from cola.
pub(crate) const REQUIRED_SERVER_REQUESTS: &[&str] = &[
    "applyPatchApproval",
    "execCommandApproval",
    "item/commandExecution/requestApproval",
    "item/fileChange/requestApproval",
    "item/permissions/requestApproval",
    "item/tool/requestUserInput",
    "mcpServer/elicitation/request",
];

/// The checked-in reference document is the pinned surface's source of truth:
/// the same list the schema diff is read against, embedded so a build cannot
/// drift from the document it ships beside.
const PINNED_REFERENCE: &str = include_str!("../../docs/reference/codex-app-server-0.163.0.md");

/// One server's own method lists, in the three directions the protocol
/// defines.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Surface {
    pub(crate) client_requests: BTreeSet<String>,
    pub(crate) notifications: BTreeSet<String>,
    pub(crate) server_requests: BTreeSet<String>,
}

impl Surface {
    /// The pinned `0.163.0` surface, parsed from the checked-in reference.
    pub(crate) fn pinned() -> Self {
        let mut surface = Self::default();
        let mut section = None;
        for line in PINNED_REFERENCE.lines() {
            if let Some(heading) = line.strip_prefix("## ") {
                section = Section::from_heading(heading);
                continue;
            }
            let Some(section) = section else {
                continue;
            };
            let Some(method) = bullet_method(line) else {
                continue;
            };
            match section {
                Section::ClientRequests => {
                    surface.client_requests.insert(method.to_string());
                }
                Section::Notifications => {
                    surface.notifications.insert(method.to_string());
                }
                Section::ServerRequests => {
                    surface.server_requests.insert(method.to_string());
                }
            }
        }
        surface
    }

    /// Read the three lists out of a `generate-json-schema` directory.
    pub(crate) fn from_generated_dir(directory: &Path) -> Result<Self> {
        Ok(Self {
            client_requests: methods_in(&directory.join("ClientRequest.json"))?,
            notifications: methods_in(&directory.join("ServerNotification.json"))?,
            server_requests: methods_in(&directory.join("ServerRequest.json"))?,
        })
    }

    /// The required methods this surface lacks, in the constants' order (so a
    /// failure names the same method every time).
    pub(crate) fn missing_required(&self) -> Vec<String> {
        let mut missing = Vec::new();
        for method in REQUIRED_CLIENT_METHODS {
            if !self.client_requests.contains(*method) {
                missing.push((*method).to_string());
            }
        }
        for method in REQUIRED_NOTIFICATIONS {
            if !self.notifications.contains(*method) {
                missing.push((*method).to_string());
            }
        }
        for method in REQUIRED_SERVER_REQUESTS {
            if !self.server_requests.contains(*method) {
                missing.push((*method).to_string());
            }
        }
        missing
    }

    /// The three list sizes, for the attach log (the pinned counts are
    /// 105 / 85 / 10).
    pub(crate) fn counts(&self) -> (usize, usize, usize) {
        (
            self.client_requests.len(),
            self.notifications.len(),
            self.server_requests.len(),
        )
    }
}

/// How construction learns the running server's method surface.
#[async_trait]
pub(crate) trait SurfaceProbe: Send + Sync {
    /// The surface of a server that reported `server`.
    async fn surface(&self, server: &ServerVersion) -> Result<Surface>;

    /// Where the surface came from, for the attach log and `cola doctor`.
    fn source(&self) -> String;
}

/// Accept the pinned surface for the pinned version and refuse every other
/// version loudly. This is the probe for a hermetic construction (tests) and
/// for a caller that has not wired the binary probe yet.
pub(crate) struct PinnedProbe;

#[async_trait]
impl SurfaceProbe for PinnedProbe {
    async fn surface(&self, server: &ServerVersion) -> Result<Surface> {
        if server.is_pinned() {
            return Ok(Surface::pinned());
        }
        if server.is_older_than_pin() {
            return Err(CodexError::OlderServer {
                pinned: PINNED_VERSION.to_string(),
                found: server.to_string(),
            });
        }
        Err(CodexError::Probe(format!(
            "server {server} is newer than the pinned codex-cli {PINNED_VERSION}, and this probe \
             only knows the pinned surface; probe the running binary instead of assuming the pin"
        )))
    }

    fn source(&self) -> String {
        format!("pinned codex-cli {PINNED_VERSION}")
    }
}

/// Probe the running binary: an exact pin uses the checked-in surface, any
/// other version regenerates the schema from that binary and verifies the
/// binary really is the one the server runs.
pub(crate) struct AutoProbe {
    binary: std::path::PathBuf,
}

impl AutoProbe {
    pub(crate) fn new(binary: impl Into<std::path::PathBuf>) -> Self {
        Self {
            binary: binary.into(),
        }
    }
}

#[async_trait]
impl SurfaceProbe for AutoProbe {
    async fn surface(&self, server: &ServerVersion) -> Result<Surface> {
        if server.is_pinned() {
            return Ok(Surface::pinned());
        }
        if server.is_older_than_pin() {
            return Err(CodexError::OlderServer {
                pinned: PINNED_VERSION.to_string(),
                found: server.to_string(),
            });
        }
        let (binary_version, surface) = probe_binary(&self.binary).await?;
        if binary_version.parsed != server.parsed {
            return Err(CodexError::BinaryVersionMismatch {
                binary: self.binary.display().to_string(),
                binary_version: binary_version.to_string(),
                server_version: server.to_string(),
            });
        }
        Ok(surface)
    }

    fn source(&self) -> String {
        format!(
            "pinned codex-cli {PINNED_VERSION}, otherwise generated from {}",
            self.binary.display()
        )
    }
}

async fn probe_binary(binary: &Path) -> Result<(ServerVersion, Surface)> {
    let version = tokio::time::timeout(
        PROBE_TIMEOUT,
        tokio::process::Command::new(binary).arg("--version").output(),
    )
    .await
    .map_err(|_| {
        CodexError::Probe(format!(
            "`{} --version` did not answer within {PROBE_TIMEOUT:?}",
            binary.display()
        ))
    })?
    .map_err(|error| CodexError::Probe(format!("run `{} --version`: {error}", binary.display())))?;
    let printed = String::from_utf8_lossy(&version.stdout);
    let binary_version = ServerVersion::parse(&printed).ok_or_else(|| {
        CodexError::Probe(format!(
            "`{} --version` printed no recognisable version: {printed:?}",
            binary.display()
        ))
    })?;

    let directory = tempfile::TempDir::new()
        .map_err(|error| CodexError::Probe(format!("create a schema directory: {error}")))?;
    let status = tokio::time::timeout(
        PROBE_TIMEOUT,
        tokio::process::Command::new(binary)
            .args(["app-server", "generate-json-schema", "--out"])
            .arg(directory.path())
            .status(),
    )
    .await
    .map_err(|_| {
        CodexError::Probe(format!(
            "`{} app-server generate-json-schema` did not finish within {PROBE_TIMEOUT:?}",
            binary.display()
        ))
    })?
    .map_err(|error| {
        CodexError::Probe(format!(
            "run `{} app-server generate-json-schema`: {error}",
            binary.display()
        ))
    })?;
    if !status.success() {
        return Err(CodexError::Probe(format!(
            "`{} app-server generate-json-schema` exited with {status}",
            binary.display()
        )));
    }
    Ok((binary_version, Surface::from_generated_dir(directory.path())?))
}

#[derive(Debug, Clone, Copy)]
enum Section {
    ClientRequests,
    Notifications,
    ServerRequests,
}

impl Section {
    fn from_heading(heading: &str) -> Option<Self> {
        if heading.starts_with("Client → server methods") {
            Some(Self::ClientRequests)
        } else if heading.starts_with("Notifications") {
            Some(Self::Notifications)
        } else if heading.starts_with("Server → client requests") {
            Some(Self::ServerRequests)
        } else {
            None
        }
    }
}

/// One `- \`method\`` bullet. Prose bullets (no backtick) are not methods, and
/// a later section's bullets are ignored because its heading selects no
/// [`Section`].
fn bullet_method(line: &str) -> Option<&str> {
    line.strip_prefix("- `")?.split('`').next()
}

fn methods_in(path: &Path) -> Result<BTreeSet<String>> {
    let raw = std::fs::read_to_string(path)
        .map_err(|error| CodexError::Probe(format!("read {}: {error}", path.display())))?;
    let value: Value = serde_json::from_str(&raw)
        .map_err(|error| CodexError::Probe(format!("parse {}: {error}", path.display())))?;
    let mut methods = BTreeSet::new();
    collect_methods(&value, &mut methods);
    if methods.is_empty() {
        return Err(CodexError::Probe(format!(
            "{} carries no method definitions",
            path.display()
        )));
    }
    Ok(methods)
}

/// Walk a generated schema and collect every `properties.method` enum. The
/// request/notification/request unions are the only shapes with that key, so
/// one recursive pass covers the file. A real definition has one value; every
/// enum member is collected so a merged fixture cannot silently lose one.
fn collect_methods(value: &Value, methods: &mut BTreeSet<String>) {
    match value {
        Value::Object(map) => {
            if let Some(Value::Object(properties)) = map.get("properties") {
                match properties.get("method") {
                    Some(Value::String(name)) => {
                        methods.insert(name.clone());
                    }
                    Some(Value::Object(object)) => {
                        if let Some(name) = object.get("const").and_then(Value::as_str) {
                            methods.insert(name.to_string());
                        }
                        if let Some(values) = object.get("enum").and_then(Value::as_array) {
                            for name in values.iter().filter_map(Value::as_str) {
                                methods.insert(name.to_string());
                            }
                        }
                    }
                    _ => {}
                }
            }
            for nested in map.values() {
                collect_methods(nested, methods);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_methods(item, methods);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pinned_surface_matches_the_reference_counts() {
        let surface = Surface::pinned();
        assert_eq!(
            surface.counts(),
            (105, 85, 10),
            "the checked-in reference must parse to the pinned counts"
        );
        assert!(
            surface.missing_required().is_empty(),
            "the pinned surface must carry every required method: {:?}",
            surface.missing_required()
        );
    }

    #[test]
    fn a_generated_directory_yields_the_same_lists() {
        let directory = tempfile::tempdir().expect("tempdir");
        for (file, methods) in [
            ("ClientRequest.json", &["alpha", "beta"] as &[&str]),
            ("ServerNotification.json", &["gamma"]),
            ("ServerRequest.json", &["delta"]),
        ] {
            let one_of: Vec<Value> = methods
                .iter()
                .map(|method| {
                    serde_json::json!({
                        "properties": { "method": { "enum": [method] } },
                        "required": ["method"],
                        "type": "object",
                    })
                })
                .collect();
            std::fs::write(
                directory.path().join(file),
                serde_json::json!({ "oneOf": one_of }).to_string(),
            )
            .expect("write schema fixture");
        }
        let surface = Surface::from_generated_dir(directory.path()).expect("generated surface");
        assert!(surface.client_requests.contains("alpha"));
        assert!(surface.client_requests.contains("beta"));
        assert!(surface.notifications.contains("gamma"));
        assert!(surface.server_requests.contains("delta"));
    }

    #[test]
    fn a_missing_required_method_is_reported_by_name() {
        let mut surface = Surface::pinned();
        surface.client_requests.remove("turn/steer");
        surface.notifications.remove("thread/status/changed");

        let missing = surface.missing_required();
        assert!(missing.contains(&"turn/steer".to_string()));
        assert!(missing.contains(&"thread/status/changed".to_string()));
    }

    #[tokio::test]
    async fn the_pinned_probe_refuses_an_older_or_newer_server_loudly() {
        let pinned = PinnedProbe;
        for (raw, needle) in [
            ("codex-tui/0.162.0 (test)", "0.162.0"),
            ("codex-tui/0.164.0 (test)", "0.164.0"),
        ] {
            let server = ServerVersion::parse(raw).expect("version");
            let error = pinned.surface(&server).await.expect_err("refused").to_string();
            assert!(error.contains(needle), "{error}");
        }
    }

    /// The pin costs no subprocess: the checked-in surface is used directly.
    #[tokio::test]
    async fn the_auto_probe_short_circuits_the_pinned_version() {
        let server = ServerVersion::parse("codex-tui/0.163.0 (test)").expect("version");
        let surface = AutoProbe::new("/nonexistent/codex")
            .surface(&server)
            .await
            .expect("the pin needs no binary");
        assert_eq!(surface.counts(), (105, 85, 10));
    }

    /// A non-pinned server is described by regenerating its own binary's
    /// schema, and a binary that is not the running one is refused. The fake
    /// binary stands in for `codex` so the probe's subprocess boundary is
    /// exercised hermetically.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_binary_probe_generates_and_verifies_a_newer_surface() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("tempdir");
        let binary = directory.path().join("codex");
        std::fs::write(
            &binary,
            r#"#!/bin/sh
if [ "$1" = "--version" ]; then
  echo "codex-cli 0.164.0"
  exit 0
fi
out="$4"
mkdir -p "$out" || exit 1
printf '%s' '{"oneOf":[{"properties":{"method":{"enum":["initialize","turn/steer"]}}}]}' > "$out/ClientRequest.json"
printf '%s' '{"oneOf":[{"properties":{"method":{"enum":["turn/started"]}}}]}' > "$out/ServerNotification.json"
printf '%s' '{"oneOf":[{"properties":{"method":{"enum":["item/tool/requestUserInput"]}}}]}' > "$out/ServerRequest.json"
"#,
        )
        .expect("write fake codex");
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755))
            .expect("make fake codex executable");

        let probe = AutoProbe::new(&binary);
        let newer = ServerVersion::parse("codex-tui/0.164.0 (test)").expect("version");
        let surface = probe.surface(&newer).await.expect("generated surface");
        assert!(surface.client_requests.contains("turn/steer"));
        assert_eq!(surface.counts(), (2, 1, 1));

        let other = ServerVersion::parse("codex-tui/0.165.0 (test)").expect("version");
        let error = probe
            .surface(&other)
            .await
            .expect_err("a different binary is refused");
        assert!(
            matches!(error, CodexError::BinaryVersionMismatch { .. }),
            "expected a version mismatch, got {error}"
        );
    }

    /// A binary that cannot run the generator is a loud probe failure.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_unusable_probe_binary_is_a_loud_failure() {
        let newer = ServerVersion::parse("codex-tui/0.164.0 (test)").expect("version");
        let error = AutoProbe::new("/nonexistent/codex")
            .surface(&newer)
            .await
            .expect_err("an unusable binary is refused");
        assert!(matches!(error, CodexError::Probe(_)), "{error}");
    }
}
