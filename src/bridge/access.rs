//! The Access List and the Host gate (ADR-0035).
//!
//! The personal-machine model admits exactly one Principal — the Host, named
//! once by a Claim. Every inbound message and card action resolves its
//! Principal and is authorized against this store before cola acts.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The persisted, machine-local record of admitted Principals. In the
/// personal-machine model it holds exactly one entry: the Host. A missing or
/// unreadable file means the bot is unclaimed (deny everything but the Claim).
#[derive(Debug)]
pub struct AccessList {
    path: PathBuf,
    host: Option<String>,
}

/// The on-disk shape. Versionless by design: one optional Host today, room to
/// grow into the server-mode shape (ADR-0036) without a migration.
#[derive(Debug, Default, Serialize, Deserialize)]
struct AccessFile {
    #[serde(default)]
    host: Option<String>,
}

impl AccessList {
    /// Load the list from `path`. An absent, unreadable, or malformed file is
    /// an unclaimed list — never an error: a broken store must fail closed.
    pub fn load(path: &Path) -> Self {
        let host = std::fs::read_to_string(path)
            .ok()
            .and_then(|data| serde_json::from_str::<AccessFile>(&data).ok())
            .and_then(|file| file.host);
        Self {
            path: path.to_path_buf(),
            host,
        }
    }

    /// The Host's `open_id`, when claimed.
    pub fn host(&self) -> Option<&str> {
        self.host.as_deref()
    }

    /// Whether the bot has been claimed.
    pub fn is_claimed(&self) -> bool {
        self.host.is_some()
    }

    /// Claim the bot for `open_id` and persist. One-time: returns `false` and
    /// changes nothing when a Host is already recorded.
    pub fn claim(&mut self, open_id: &str) -> crate::error::Result<bool> {
        if self.host.is_some() {
            return Ok(false);
        }
        self.host = Some(open_id.to_string());
        self.write_to_disk()?;
        Ok(true)
    }

    fn write_to_disk(&self) -> crate::error::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let data = serde_json::to_string_pretty(&AccessFile {
            host: self.host.clone(),
        })?;
        std::fs::write(&self.path, data)?;
        Ok(())
    }
}

/// The runtime gate state: the Access List plus the startup Claim Code.
#[derive(Debug)]
pub struct Access {
    list: AccessList,
    /// Present only while unclaimed: printed at startup, rotated every start,
    /// never persisted, consumed by the first successful Claim.
    claim_code: Option<String>,
}

/// What the gate decides for one inbound action.
#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    /// The Principal is the Host.
    Allow,
    /// A matching Claim from a p2p chat: record the sender as Host.
    Claim,
    /// A Claim attempt on an already-claimed bot: acknowledge, change nothing.
    AlreadyClaimed,
    /// Refuse — nothing reaches the backend.
    Deny(DenyReason),
}

#[derive(Debug, PartialEq, Eq)]
pub enum DenyReason {
    /// No Host recorded yet — only the Claim is served.
    Unclaimed,
    /// Claimed, but the action's Principal is not the Host (or is missing).
    NotHost,
}

impl Access {
    /// Load the list from `path` and mint a Claim Code when it is unclaimed.
    /// The code is logged once here — the startup log is the only place it is
    /// ever shown.
    pub fn new(path: PathBuf) -> Self {
        let list = AccessList::load(&path);
        let claim_code = (!list.is_claimed()).then(generate_claim_code);
        let access = Self { list, claim_code };
        if let Some(code) = access.claim_code() {
            tracing::warn!("cola 尚未认领 —— 认领码: {code}（在飞书私聊中发送 /claim {code} 成为宿主）");
        }
        access
    }

    /// The startup Claim Code, while unclaimed.
    pub fn claim_code(&self) -> Option<&str> {
        self.claim_code.as_deref()
    }

    /// Authorize one inbound action — its Principal: the message's sender or
    /// the card click's user. `is_p2p` restricts the Claim to a private chat.
    pub fn decide(&self, principal: Option<&str>, is_p2p: bool, text: &str) -> Decision {
        if let Some(code) = claim_code_of(text) {
            if self.list.is_claimed() {
                return Decision::AlreadyClaimed;
            }
            return if is_p2p && principal.is_some() && self.claim_code.as_deref() == Some(code) {
                Decision::Claim
            } else {
                Decision::Deny(DenyReason::Unclaimed)
            };
        }
        if !self.list.is_claimed() {
            return Decision::Deny(DenyReason::Unclaimed);
        }
        if principal == self.list.host() {
            Decision::Allow
        } else {
            Decision::Deny(DenyReason::NotHost)
        }
    }

    /// Record `open_id` as the Host after a [`Decision::Claim`]. One-time:
    /// `false` means the list already had a Host.
    pub fn claim(&mut self, open_id: &str) -> crate::error::Result<bool> {
        let claimed = self.list.claim(open_id)?;
        if claimed {
            self.claim_code = None;
        }
        Ok(claimed)
    }
}

/// The code a `/claim <code>` message carries, or `None` when the text is not
/// a claim attempt. Every `/claim ...` message is a claim attempt — it must
/// never fall through to the model as a forwarded prompt — and only the first
/// argument is read; a bare `/claim` carries an empty code (never matches).
fn claim_code_of(text: &str) -> Option<&str> {
    let mut parts = text.split_whitespace();
    match parts.next() {
        Some("/claim") => Some(parts.next().unwrap_or("")),
        _ => None,
    }
}

/// 8 characters, ambiguous glyphs excluded (no 0/O, 1/I), derived from a v4
/// UUID's bytes. Short enough to transcribe from a log, long enough that
/// guessing it over a chat is impractical.
fn generate_claim_code() -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    uuid::Uuid::new_v4()
        .as_bytes()
        .iter()
        .take(8)
        .map(|byte| ALPHABET[*byte as usize % ALPHABET.len()] as char)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_file_is_unclaimed() {
        let dir = tempfile::tempdir().unwrap();
        let list = AccessList::load(&dir.path().join("access.json"));
        assert!(!list.is_claimed());
        assert_eq!(list.host(), None);
    }

    #[test]
    fn claim_persists_the_host() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("access.json");
        let mut list = AccessList::load(&path);
        assert!(list.claim("ou_alice").unwrap());
        assert_eq!(list.host(), Some("ou_alice"));
        assert_eq!(AccessList::load(&path).host(), Some("ou_alice"));
    }

    #[test]
    fn claim_is_one_time() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("access.json");
        let mut list = AccessList::load(&path);
        assert!(list.claim("ou_alice").unwrap());
        assert!(!list.claim("ou_bob").unwrap());
        assert_eq!(AccessList::load(&path).host(), Some("ou_alice"));
    }

    #[test]
    fn malformed_file_is_unclaimed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("access.json");
        std::fs::write(&path, "not json").unwrap();
        let list = AccessList::load(&path);
        assert!(!list.is_claimed());
        assert_eq!(list.host(), None);
    }

    #[test]
    fn unclaimed_access_mints_a_code_without_writing_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("access.json");
        let mut access = Access::new(path.clone());
        assert!(access.claim_code().is_some(), "unclaimed has a code");
        assert!(!path.exists(), "the claim code must never be persisted");
        assert!(access.claim("ou_alice").unwrap());
        assert_eq!(access.claim_code(), None, "claiming retires the code");
        assert_eq!(AccessList::load(&path).host(), Some("ou_alice"));
    }

    #[test]
    fn claim_code_is_eight_unambiguous_characters() {
        const ALPHABET: &str = "ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
        let code = generate_claim_code();
        assert_eq!(code.len(), 8);
        assert!(
            code.chars().all(|c| ALPHABET.contains(c)),
            "ambiguous glyph in {code}"
        );
    }

    #[test]
    fn claim_attempts_are_recognized() {
        assert_eq!(claim_code_of(" /claim abc "), Some("abc"));
        assert_eq!(claim_code_of("/claim"), Some(""));
        assert_eq!(claim_code_of("/claimx abc"), None);
        assert_eq!(claim_code_of("hello"), None);
    }
}
