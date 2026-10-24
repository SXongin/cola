//! The OpenCode server lifecycle: discovery, attach, health, an optional start
//! and the yield/reap policy (ADR-0013, ADR-0085; spec #860 ticket #875).
//!
//! This is the OpenCode implementation of
//! [`crate::backend::ServerLifecycle`]. Everything runtime-specific lives here,
//! not in the bridge:
//!
//! - the Shared Store invariant (attach to whatever server reads the default
//!   store, whoever started it — ADR-0013);
//! - the Shared Service / Coexistent / Owned selection order and the
//!   yield-and-reap policy (ADR-0085);
//! - generation re-probing on every attach/reconnect (spec #364 §2);
//! - Lazy Start and the eager boot's spawn.
//!
//! The bridge sees one [`ServerLifecycle::ensure_attached`] call and an
//! [`AttachOutcome`]; it never names a process class or a discovery type.

use std::time::Duration;

use async_trait::async_trait;

use crate::backend::lifecycle::{
    AttachOutcome, AttachmentStatus, Capability, ClientReport, ClientState, EnsureAttached, OtherServer,
    RestartOutcome, ServerLifecycle,
};
use crate::backend::wait_for_server_ready;
use crate::config::{ServerStartPolicy, port_from_url};
use crate::error::BridgeError;
use crate::opencode::attach;
use crate::opencode::client::OpenCodeBackend;
use crate::opencode::discovery::{self, ServerCandidate};
use crate::opencode::transport::Transport;

/// Whether the server cola is currently attached to is still present in this
/// scan — the same process (pid, when the attach path carried one) on the same
/// port, still on the default store.
///
/// This is the guard that separates "the canonical pick moved to another live
/// server" (defer while a turn is in flight — the server being left behind is
/// still serving) from "the attached server is gone" (reattach immediately,
/// even busy: the endpoint is dead). A replacement on the same port is NOT the
/// attached server — its pid differs, so the old endpoint is gone and the pass
/// must re-resolve now.
fn current_is_alive(candidates: &[ServerCandidate], current: &str, attached_pid: Option<i32>) -> bool {
    let Some(port) = port_from_url(current) else {
        return false;
    };
    candidates.iter().any(|c| {
        c.uses_default_store && c.port != 0 && c.port == port && attached_pid.is_none_or(|pid| c.pid == pid)
    })
}

/// Whether this reconcile pass must re-resolve the attachment: the picked
/// server is not the one currently attached.
///
/// Identity, not just the URL, decides: a server replaced on the SAME port —
/// a new pid — may be the other generation or carry a new password, and
/// `auto` re-probes on every attach/reconnect (spec #364 §2). Comparing only
/// the URL would keep a stale strategy (and stale credentials) for that
/// replacement. A `None` attached pid means the attach path did not carry an
/// identity, so the pass re-resolves.
fn needs_reattach(current_url: &str, attached_pid: Option<i32>, picked: &ServerCandidate) -> bool {
    current_url != picked.url() || attached_pid != Some(picked.pid)
}

/// Terminate cola's Owned Server (verified by pid against the live candidates)
/// and forget it, so the Shared Store returns to one server once cola attached
/// to a Coexistent one. The yield (ADR-0013). The pid file is cleared only
/// after the process is confirmed dead (SIGTERM, escalating to SIGKILL) — a
/// lingering process cola lost the record of could never be reaped.
async fn reap_owned_server(
    layout: &crate::state::StateLayout,
    candidates: &[ServerCandidate],
    self_pid: Option<i32>,
) {
    let Some(self_pid) = self_pid else { return };
    if !candidates.iter().any(|c| c.pid == self_pid) {
        return;
    }
    tracing::info!("yielding: terminating own OpenCode server (pid {})", self_pid);
    if let Err(e) = crate::process::terminate(self_pid) {
        tracing::warn!("yield: failed to terminate Owned Server {}: {}", self_pid, e);
        return;
    }
    if wait_dead(self_pid, 5000).await {
        discovery::clear_self_spawned(layout);
        return;
    }
    tracing::warn!(
        "yield: Owned Server {} did not exit on SIGTERM; sending SIGKILL",
        self_pid
    );
    let _ = crate::process::force_kill(self_pid);
    if wait_dead(self_pid, 5000).await {
        discovery::clear_self_spawned(layout);
    } else {
        tracing::warn!(
            "yield: Owned Server {} did not exit after SIGKILL; keeping its pid record",
            self_pid
        );
    }
}

/// Poll until a process is dead (not alive / not a zombie), within `timeout_ms`.
async fn wait_dead(pid: i32, timeout_ms: u64) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        if !crate::process::alive(pid) {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Whether a demand-time spawn is wanted for this reconcile pass.
///
/// `allow_spawn` is the message-path demand (Lazy Start). `heal_when_busy` is
/// the poll-path heal: also spawn when a turn is in flight and the server is
/// gone, so a mid-stream generation's next poll finds a live server instead of
/// 502ing forever. `Never` policy is enforced separately by
/// `ServerStartPolicy::spawns_when_needed`.
fn want_to_spawn(allow_spawn: bool, heal_when_busy: bool, busy: bool) -> bool {
    allow_spawn || (heal_when_busy && busy)
}

impl OpenCodeBackend {
    /// One pass of the server-ownership reconcile (ADR-0013, ADR-0085): attach
    /// to the canonical default-store server (Shared Service first, then a
    /// Coexistent Server over cola's Owned), lazily start a server when
    /// `allow_spawn` and none exists (the machine-wide Shared Service first,
    /// else an Owned `serve --port`), and reap a leftover Owned Server once
    /// another server is the pick. A migration — the yield included — is
    /// deferred while a turn is in flight AND the attached server is still
    /// alive, so an appearing Service (or a preferred sibling) never truncates
    /// a streaming generation; a dead attached server reattaches immediately
    /// (the heal path). When more than one eligible server exists, the pass
    /// also WARNs once naming the canonical and every ignored server, so the
    /// choice is visible without attempting to sync two servers.
    async fn reconcile(&self, request: &EnsureAttached<'_>) -> crate::error::Result<AttachOutcome> {
        let candidates = discovery::scan_processes();
        let self_pid = discovery::self_spawned_pid(self.layout());
        let Some(server) = discovery::pick_server(&candidates, self.settings().preferred_port(), self_pid)
        else {
            // No default-store server at all. Lazy Start: start a server on
            // demand (the Shared Service first, else an Owned `serve --port`);
            // otherwise stay serverless (the caller replies that OpenCode is
            // unavailable).
            let want_spawn = want_to_spawn(request.allow_spawn, request.heal_when_busy, (request.busy)());
            if want_spawn && self.settings().start_server.spawns_when_needed() {
                let spawned = match discovery::spawn_own_server(
                    self.layout(),
                    self.settings().preferred_port(),
                    self.settings().generation,
                )
                .await
                {
                    Ok(spawned) => spawned,
                    // A start that could not classify while a default-store
                    // server IS alive (a V2 Shared Service answering 503 while
                    // it migrates, say) must not take the bot down at boot:
                    // stay serverless and let the reconnect loop attach once
                    // the probe classifies. The demand and poll paths keep the
                    // error, so their caller reports it.
                    Err(e) if request.boot && discovery::has_live_default_store_server() => {
                        tracing::warn!("start could not resolve a generation yet; staying serverless: {e}");
                        return Ok(AttachOutcome::Serverless);
                    }
                    Err(e) => return Err(BridgeError::OpenCode(format!("lazy start failed: {e}"))),
                };
                self.reconnect(Some(&spawned));
                // The spawned server only serves requests after a short startup
                // window (requests landing in it are swallowed forever). Wait
                // until it actually responds, so the message flow's first
                // request lands on a live server instead of hanging the turn
                // silently. Boot skips this: `spawn_own_server` already
                // resolved the generation against the live server.
                if !request.boot
                    && let Err(evidence) = wait_for_server_ready(self, Duration::from_secs(20)).await
                {
                    // The helper reports neutral evidence; the adapter owns
                    // the runtime-specific error text and its existing wording.
                    let e = BridgeError::OpenCode(format!("own OpenCode server {evidence}"));
                    tracing::warn!("own OpenCode server never served a request: {}", e);
                    // Reap the wedged server and go serverless, so the NEXT
                    // message re-runs this reconcile and spawns a fresh one —
                    // leaving the record in place would re-attach a scan-found
                    // wedged server (its requests are swallowed, hanging the
                    // turn again).
                    if let Some(pid) = discovery::self_spawned_pid(self.layout()) {
                        if crate::process::terminate(pid).is_ok() && wait_dead(pid, 5000).await {
                            discovery::clear_self_spawned(self.layout());
                        } else {
                            let _ = crate::process::force_kill(pid);
                            if wait_dead(pid, 5000).await {
                                discovery::clear_self_spawned(self.layout());
                            }
                        }
                    }
                    self.reconnect(None);
                    return Err(e);
                }
                if request.boot {
                    tracing::info!(
                        "Started cola's OpenCode server at {} — generation={} (Shared Service when one could be raised, else self-started `opencode serve`)",
                        spawned.url,
                        spawned.generation.as_str()
                    );
                } else if request.allow_spawn {
                    tracing::info!(
                        "lazily started an OpenCode server at {} (generation={})",
                        spawned.url,
                        spawned.generation.as_str()
                    );
                } else {
                    tracing::warn!(
                        "attached server died mid-turn; started an OpenCode server at {} (generation={})",
                        spawned.url,
                        spawned.generation.as_str()
                    );
                }
                return Ok(AttachOutcome::Attached);
            }
            // The attached server, if any, is gone — go serverless so a later
            // message re-attaches or lazily spawns instead of 502ing against
            // the dead endpoint forever.
            if !self.base_url().is_empty() {
                tracing::warn!("attached OpenCode server is gone; going serverless");
                self.reconnect(None);
            }
            return Ok(AttachOutcome::Serverless);
        };
        let url = server.url();
        let ignored = discovery::other_servers(&candidates, server);
        if !ignored.is_empty() {
            // One server is the premise of real-time sync (ADR-0085). cola
            // cannot aggregate two servers, so make the choice and the ignored
            // servers visible every pass instead of silently flapping.
            tracing::warn!(
                "multiple default-store OpenCode servers; canonical {} (pid {}); ignoring {}",
                url,
                server.pid,
                ignored
                    .iter()
                    .map(|c| format!(
                        "{} (pid {}, {})",
                        c.url(),
                        c.pid,
                        if c.is_service() {
                            "service"
                        } else {
                            "not a service"
                        }
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        let current = self.base_url();
        let attached_pid = self.attached_server_pid();
        let pick_is_owned = self_pid == Some(server.pid);

        if needs_reattach(&current, attached_pid, server) {
            if current_is_alive(&candidates, &current, attached_pid) && (request.busy)() {
                // Defer ANY migration while a turn streams on a server that is
                // still alive — not only the Owned-Server yield. A Shared
                // Service appearing while cola is attached to OpenChamber (or a
                // preferred sibling within the same class) is exactly the case
                // the spec asks to defer, so the in-flight generation is never
                // truncated. A dead current server is not deferred: the heal
                // path reattaches now.
                return Ok(AttachOutcome::Attached);
            }
            tracing::warn!(
                "OpenCode server changed ({} pid {} -> {} pid {}); reconnecting",
                current,
                attached_pid.map_or("?".to_string(), |p| p.to_string()),
                url,
                server.pid
            );
            // Resolve the new attachment's generation before re-pointing (spec
            // #364 §2). A probe that cannot classify the server is never
            // guessed at: cola drops to serverless and the next pass retries —
            // the evidence lets the poll loop's latch report it once, not
            // every tick.
            let resolved = match attach::resolve_candidate(server, self.settings().generation).await {
                Ok(resolved) => resolved,
                Err(evidence) => {
                    if !current.is_empty() {
                        self.reconnect(None);
                    }
                    return Ok(AttachOutcome::Inconclusive { url, evidence });
                }
            };
            self.reconnect(Some(&resolved));
        }

        // Attached to the canonical server. If it is not cola's Owned Server
        // (Shared Service or Coexistent), reap a leftover Owned Server (the
        // yield) — but never while a turn is in flight.
        if !pick_is_owned {
            if (request.busy)() {
                return Ok(AttachOutcome::Attached);
            }
            reap_owned_server(self.layout(), &candidates, self_pid).await;
        }
        Ok(AttachOutcome::Attached)
    }
}

#[async_trait]
impl ServerLifecycle for OpenCodeBackend {
    async fn ensure_attached(&self, request: EnsureAttached<'_>) -> crate::error::Result<AttachOutcome> {
        let _guard = self.server_lock().lock().await;
        // The demand path's fast paths (the regex of the old `ensure_server`
        // hook): before rescanning the process table on every message, honour
        // attach-only and an existing attachment. The eager boot also sets
        // `allow_spawn`, but starts serverless, so it falls through to the scan.
        if request.allow_spawn {
            if self.settings().start_server == ServerStartPolicy::Never {
                return Ok(if self.base_url().is_empty() {
                    AttachOutcome::Serverless
                } else {
                    AttachOutcome::Attached
                });
            }
            if !self.base_url().is_empty() {
                return Ok(AttachOutcome::Attached);
            }
        }
        self.reconcile(&request).await
    }

    async fn attachment_status(&self) -> Option<AttachmentStatus> {
        let candidates = discovery::scan_processes();
        let candidate = discovery::pick_server(
            &candidates,
            self.settings().preferred_port(),
            discovery::self_spawned_pid(self.layout()),
        )?;
        let other_servers: Vec<OtherServer> = discovery::other_servers(&candidates, candidate)
            .into_iter()
            .map(|c| OtherServer {
                url: c.url(),
                pid: c.pid,
            })
            .collect();
        // The probe carries Basic credentials and the endpoint is always
        // loopback, so it must bypass any env proxy (ADR-0081) — `new_loopback`,
        // not the production attach transport.
        let transport = Transport::new_loopback(
            Some(&candidate.username),
            Some(&candidate.password),
            candidate.url(),
        );
        let (url, generation, evidence) =
            match attach::resolve_candidate_over(&transport, candidate, self.settings().generation).await {
                Ok(resolved) => (
                    resolved.url,
                    Some(resolved.generation.as_str().to_string()),
                    String::new(),
                ),
                Err(evidence) => (candidate.url(), None, evidence),
            };
        Some(AttachmentStatus {
            url,
            pid: candidate.pid,
            generation,
            evidence,
            other_servers,
        })
    }

    /// The client-scoped diagnosis (ticket #883). OpenCode's runtime is the
    /// shared server the attach probe already resolves, so this reuses
    /// [`Self::attachment_status`] rather than inventing a second probe; the
    /// only capability the attachment itself proves is the generation, and the
    /// Background Task surface is this adapter's own answer.
    async fn client_report(&self) -> Option<ClientReport> {
        let settings = self.settings();
        let configured = settings
            .url
            .clone()
            .filter(|url| !url.is_empty())
            .unwrap_or_else(|| "http://localhost:4096".to_string());
        let attachment = self.attachment_status().await;
        let endpoint = attachment
            .as_ref()
            .map(|status| status.url.clone())
            .unwrap_or(configured);
        let capabilities = attachment
            .as_ref()
            .map(|status| {
                let generation = status.generation.clone().unwrap_or_else(|| "unknown".to_string());
                vec![
                    Capability {
                        name: "generation".to_string(),
                        value: generation,
                    },
                    Capability {
                        name: "background_tasks".to_string(),
                        value: if self.background_tasks_supported() {
                            "yes".to_string()
                        } else {
                            "unsupported".to_string()
                        },
                    },
                ]
            })
            .unwrap_or_default();
        let (state, detail) = match attachment {
            Some(status) => (
                ClientState::Attached,
                match status.generation {
                    Some(generation) => format!("attached (generation {generation})"),
                    None => format!("attached, generation unknown: {}", status.evidence),
                },
            ),
            None => (
                ClientState::Absent,
                "no shared OpenCode server was found; LAZY start or attach-only".to_string(),
            ),
        };
        Some(ClientReport {
            endpoint,
            state,
            detail,
            runtime_binary: None,
            version: None,
            platform: None,
            surface_source: None,
            capabilities,
            user_clients: Vec::new(),
            client_binary: None,
        })
    }

    async fn restart_server(&self) -> crate::error::Result<RestartOutcome> {
        let outcome = discovery::restart_self_spawned_server(self.layout(), self.settings().preferred_port())
            .await
            .map_err(|e| BridgeError::OpenCode(format!("restart opencode failed: {e}")))?;
        Ok(match outcome {
            discovery::RestartOutcome::Restarted => RestartOutcome::Restarted,
            discovery::RestartOutcome::RestartedService => RestartOutcome::RestartedService,
            discovery::RestartOutcome::NotOwned => RestartOutcome::NotOwned,
            discovery::RestartOutcome::NoServer => RestartOutcome::NoServer,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opencode::discovery::ServerClass;

    fn cand(pid: i32, port: u16) -> ServerCandidate {
        ServerCandidate {
            pid,
            port,
            username: "opencode".into(),
            password: "x".into(),
            uses_default_store: true,
            class: ServerClass::Coexistent,
        }
    }

    #[test]
    fn current_is_alive_tracks_identity_not_just_the_port() {
        let servers = vec![cand(7, 4096)];
        assert!(current_is_alive(&servers, "http://localhost:4096", Some(7)));
        // A replacement on the same port is NOT the attached server: its
        // endpoint is gone, so the pass must reattach immediately.
        assert!(!current_is_alive(&servers, "http://localhost:4096", Some(8)));
        // No identity on the attachment: the port is the only thing to compare.
        assert!(current_is_alive(&servers, "http://localhost:4096", None));
        assert!(!current_is_alive(&servers, "http://localhost:4097", Some(7)));
        assert!(!current_is_alive(&servers, "http://mock", Some(7)));
        assert!(!current_is_alive(&servers, "", Some(7)));
        // A candidate on a custom store is someone else's data — not the
        // attached server, even on the same port.
        let custom = ServerCandidate {
            uses_default_store: false,
            ..cand(7, 4096)
        };
        assert!(!current_is_alive(&[custom], "http://localhost:4096", Some(7)));
    }

    /// Same-URL replacements are the reason identity (pid), not just the URL,
    /// decides: a server swapped on the same port may speak the other
    /// generation or carry a new password, and `auto` re-probes on every
    /// attach/reconnect (spec #364 §2).
    #[test]
    fn needs_reattach_on_url_or_pid_change() {
        let picked = cand(7, 4096);
        assert!(
            !needs_reattach("http://localhost:4096", Some(7), &picked),
            "the same server stays attached"
        );
        assert!(
            needs_reattach("http://localhost:4096", Some(8), &picked),
            "a replacement on the same port must re-resolve"
        );
        assert!(
            needs_reattach("http://localhost:4097", Some(7), &picked),
            "a server on another port must re-resolve"
        );
        assert!(
            needs_reattach("", None, &picked),
            "serverless re-attaches at the picked server"
        );
        assert!(
            needs_reattach("http://localhost:4096", None, &picked),
            "an unknown identity re-resolves rather than trust the URL"
        );
    }

    fn serverless_backend(settings: crate::config::OpenCodeConfig) -> OpenCodeBackend {
        OpenCodeBackend::with_tasks(
            None,
            &settings,
            crate::state::StateLayout::default(),
            crate::bridge::tasks::TasksHandle::new(),
        )
    }

    /// The demand path's real decision, not an extracted helper: with
    /// `start_server = "never"`, a serverless cola reports serverless (the
    /// caller replies NoServer) and an attached one reports attached.
    #[tokio::test]
    async fn the_demand_path_honours_the_attach_only_policy() {
        let settings = crate::config::OpenCodeConfig {
            start_server: ServerStartPolicy::Never,
            ..Default::default()
        };
        let backend = serverless_backend(settings);
        let never_busy = || false;
        let request = || EnsureAttached {
            allow_spawn: true,
            heal_when_busy: true,
            busy: &never_busy,
            boot: false,
        };

        assert_eq!(
            backend.ensure_attached(request()).await.unwrap(),
            AttachOutcome::Serverless,
            "attach-only with no server is the NoServer state"
        );

        backend.reconnect(Some(&crate::opencode::discovery::ResolvedServer {
            url: "http://localhost:4096".into(),
            username: "opencode".into(),
            password: "pw".into(),
            pid: Some(4242),
            generation: crate::opencode::strategy::Generation::V1,
        }));
        assert_eq!(
            backend.ensure_attached(request()).await.unwrap(),
            AttachOutcome::Attached,
            "attach-only with a live server stays attached"
        );
    }

    /// The lifecycle lock is what makes a busy sample meaningful: the probe
    /// must not run until this pass owns the lock, or a turn that starts while
    /// the pass waits would be read as idle.
    #[tokio::test]
    async fn the_busy_probe_is_read_only_after_the_lifecycle_lock() {
        let backend = serverless_backend(crate::config::OpenCodeConfig::default());
        let called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = std::sync::Arc::clone(&called);
        let busy = move || {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
            false
        };
        let request = EnsureAttached {
            allow_spawn: false,
            heal_when_busy: false,
            busy: &busy,
            boot: false,
        };

        let guard = backend.server_lock().lock().await;
        let mut pass = Box::pin(backend.ensure_attached(request));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut pass)
                .await
                .is_err(),
            "the pass must wait on the lifecycle lock"
        );
        assert!(
            !called.load(std::sync::atomic::Ordering::SeqCst),
            "the probe must not be read before the lock is acquired"
        );

        drop(guard);
        pass.await.expect("the pass completes once the lock is free");
        assert!(
            called.load(std::sync::atomic::Ordering::SeqCst),
            "the probe is read under the lock"
        );
    }

    #[test]
    fn want_to_spawn_message_path_always_spawns() {
        // Lazy Start (allow_spawn) spawns regardless of busy state.
        assert!(want_to_spawn(true, false, false));
        assert!(want_to_spawn(true, false, true));
        assert!(want_to_spawn(true, true, false));
        assert!(want_to_spawn(true, true, true));
    }

    #[test]
    fn want_to_spawn_poll_path_spawns_only_when_busy() {
        // Poll path (allow_spawn=false) spawns to heal a dead server only when
        // a turn is in flight; an idle poll stays serverless (no resource churn).
        assert!(!want_to_spawn(false, true, false));
        assert!(want_to_spawn(false, true, true));
        assert!(!want_to_spawn(false, false, false));
        assert!(!want_to_spawn(false, false, true));
    }
}
