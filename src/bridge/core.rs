use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::bridge::session::{SessionSettings, SessionStore};
use crate::config::{SessionEntry, ThreadKey};
use crate::feishu;

/// A cached session-list snapshot (cross-store, most recently active first)
/// with its fetch time. The 30 s TTL keeps `/switch`/`/attach` off the
/// wire for rapid reuse; cola invalidates it immediately on create/adopt/rename.
#[derive(Clone)]
pub struct SessionListCache {
    pub fetched_at: std::time::Instant,
    pub sessions: Vec<crate::backend::types::SessionListInfo>,
}

impl SessionListCache {
    /// Whether the snapshot is still fresh (fetched within the TTL).
    pub fn fresh(&self) -> bool {
        self.fetched_at.elapsed() < std::time::Duration::from_secs(30)
    }
}

/// The title shown on a topic's cover card, plus the model line it carries —
/// the post-turn hook compares the server title against this and patches the
/// card in place when it changed (ADR-0023).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoverTitle {
    pub title: String,
    pub model: Option<String>,
    /// The cover was recorded while the topic's session was still a Pending
    /// Session (ADR-0041), so it still says 「下一条消息创建」. The first sync
    /// after materialisation must re-render the full brief even when the title
    /// matches (the pending title and the server title can be identical, e.g.
    /// `/topic <dir> <name>`).
    pub pending: bool,
}

/// Bound on a server-side session-info fetch (`GET /session/{id}`) — the
/// prompt subtitle's title read and the effective-model ladder's
/// server-recorded rung both go through it. A freshly spawned Owned Server
/// (Lazy Start) can swallow the first requests in its startup window, and a
/// hung fetch must degrade that one field — not hang the whole turn or the
/// `/think`/`/model` cards (the Lazy Start silent-hang incident).
pub(crate) const SESSION_INFO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// How long a settlement claim may sit before it is treated as abandoned (see
/// `SharedCore::claimed_requests`). A live settlement renders its receipt within
/// one Feishu round trip; the bound only ever fires when the task that took the
/// claim was cancelled.
pub(crate) const SETTLING_CLAIM_TTL: std::time::Duration = std::time::Duration::from_secs(30);

/// State shared across every flow: the session map, the per-session live cards
/// ([`CardSession`] — accumulator + card identity in one place), the two
/// request flows (permission/question pollers + card actions, on the core so
/// snapshot claims and question remembering are reachable from the command
/// layer, ADR-0028), the double-click guard, prompt serialization, and the two
/// adapters. Owned by the bridge coordinator ([`super::App`]) and passed by
/// handle to the flow modules that need it: a flow receives the narrow
/// per-concern handles it uses ([`crate::bridge::handles`]), never the whole
/// aggregate — [`SharedCore::turn_handles`] is the Turn's bundle (spec #298).
pub struct SharedCore {
    pub sessions: Arc<Mutex<SessionStore>>,
    /// The one owner of the on-disk layout (ADR-0090 §6): every state path —
    /// the Session sidecars, the Terminal Run Records, the Terminal Log root,
    /// the pid record and the restart notice — is read from here.
    pub layout: crate::state::StateLayout,
    /// session_id → the session's one live card (accumulator + card id chain).
    pub cards: Arc<Mutex<HashMap<String, crate::bridge::turn::CardSession>>>,
    /// The card handles (ADR-0038, rule 2): `request_id → message_id` plus, for
    /// every card that shows a live interaction block, the card JSON as last
    /// rendered. Lets any card showing a block be repainted — a click's ack, a
    /// remote resolution, a sweep strip — whether or not it is still the
    /// accumulator's current card.
    pub card_handles: Arc<Mutex<crate::bridge::card_handles::CardHandles>>,
    /// The durable Chain Record (ADR-0069), beside the session mapping: the
    /// live card record (ADR-0063) and the Wake Watermark (ADR-0061) in one
    /// store, so a restart can reap the card it orphaned and never re-announce
    /// a Wake an earlier life already showed.
    pub chains: Arc<crate::bridge::chain::ChainRecords>,
    /// The Background Task State handle (ADR-0079, ticket #681), created beside
    /// the Chain Record it seeds from: the module's per-Session store of cells.
    pub tasks: crate::bridge::tasks::TasksHandle,
    /// Permission flow: owns `sent_cards`, polls pending requests, auto-accepts
    /// for `/autoaccept` sessions, and handles the "perm" card action.
    pub permission: Arc<crate::bridge::request::flow::RequestFlow>,
    /// Question flow: owns `sent_cards` + the question kind's request/partial
    /// state, polls pending questions, and handles the "question" card action.
    pub question: Arc<crate::bridge::request::flow::RequestFlow>,
    /// External-message flow: owns `last_user_msg_epoch`, notifies Feishu when
    /// another shared-store client posts while cola is idle, and arms the
    /// external-reply renderers (including the busy-adopt follow, ADR-0028).
    /// Shared (`Arc`) because the topic/command bundles carry it to settle a
    /// sent snapshot through `ExternalFlow::settle_snapshot_after_send`.
    pub external: Arc<crate::bridge::external::ExternalFlow>,
    /// ADR-0028 snapshot claim registry: which snapshot card hosts which
    /// adopt-time pending block, what each snapshot was built from, and the
    /// tombstones for late second clicks. One Mutex keeps claim/host/tombstone
    /// mutations atomic (they always change together). The poll loop treats a
    /// claimed id as already-surfaced (no standalone card, no re-inline); a
    /// claimed id leaving the pending list drops its block.
    pub snapshot_claims: Arc<Mutex<crate::bridge::snapshot_claims::SnapshotClaims>>,
    /// request ids already answered on the permission/question cards. Guards
    /// against double-click races (two card callbacks before the result card
    /// replaces the buttons): a second click on the same request is ignored
    /// server-side instead of re-replying. Also tells `mark_stale_cards` that
    /// cola's answer already settled a standalone card.
    pub answered_requests: Arc<Mutex<HashSet<String>>>,
    /// Request ids whose settlement cola has STARTED — the auto-accept
    /// approval's reply has landed (or is landing) but the mode receipt has
    /// not rendered yet. While an id sits here its disappearance from the
    /// pending list is cola's own doing, so the sweep's vanished passes must
    /// not read it as another client's resolution (`resolve_blocks` clears the
    /// claim once it settled). Kept apart from `answered_requests`: that one
    /// makes `mark_stale_cards` skip a card, and a standalone card approved by
    /// `/autoaccept on` still needs the sweep to repaint its buttons away.
    /// Each entry carries its claim time; a claim older than
    /// [`SETTLING_CLAIM_TTL`] is abandoned (the settlement task was cancelled
    /// between the approval and the receipt) and `claimed_requests` drops it,
    /// so the sweep can finish the request instead of suppressing it forever.
    pub settling_requests: Arc<Mutex<HashMap<String, std::time::Instant>>>,
    /// Session ids with a prompt currently in flight (serializes prompts per
    /// session so concurrent messages don't clobber each other's cards).
    pub inflight: Arc<Mutex<HashSet<String>>>,
    /// Session ids with an inbound user message being routed to them (#424):
    /// set at admission, cleared when the message is taken or merged. See
    /// [`crate::bridge::handles::WaitsHandle`].
    pub inbound: Arc<Mutex<HashMap<String, std::time::Instant>>>,
    /// Session ids whose run was interrupted by `/stop`. The post-prompt drain
    /// (ADR-0043) reads it so a stopped session finalizes promptly instead of
    /// waiting out its bound on a Supplement the abort left unanswered; a new
    /// Turn clears the marker when it starts.
    pub stopped_sessions: Arc<Mutex<HashSet<String>>>,
    /// A Turn's render poll cadence (ms): the incremental renderer while the
    /// prompt is in flight, and the post-prompt drain that keeps rendering a
    /// Supplement's new Turn (ADR-0043). Defaults to 1.5 s; tests store a small
    /// value so the drain's branches run without real seconds (the
    /// external-poller atomics pattern).
    pub turn_render_poll_ms: Arc<std::sync::atomic::AtomicU64>,
    /// The single grace on the post-prompt path (ms, #284/#386/#603): the
    /// merged unbounded Turn drain has NO total budget — a readable run may
    /// take as long as it takes, and a wait for the operator is unbounded (a
    /// pending permission holds indefinitely). This grace bounds only the
    /// failure modes where nobody can act: reads that stopped returning (lost
    /// contact), a live Tool Panel on an idle session that never settles (a
    /// crash orphan), and the delay before an unreceived submit shows its
    /// neutral waiting line. Defaults to 10 min; tests store a small value to
    /// exercise the fallback.
    pub turn_follow_grace_ms: Arc<std::sync::atomic::AtomicU64>,
    /// How long ONE follow read (transcript or status) may take before it is
    /// abandoned (ms, #386). Bounds each call so a hung Backend cannot delay
    /// the grace check by more than one read bound per tick; defaults to the
    /// shared 30 s request bound. Tests store a small value so a wedged read
    /// fails fast instead of eating the grace.
    pub turn_follow_read_timeout_ms: Arc<std::sync::atomic::AtomicU64>,
    /// session_id → the cover card's current title for topics created with a
    /// bot cover card as their root (ADR-0023). In-memory only: the post-turn
    /// hook compares the server title and patches the cover card in place when
    /// it changed; after a restart the next completed turn re-syncs the card
    /// once (same content, harmless).
    pub cover_titles: Arc<Mutex<HashMap<String, CoverTitle>>>,
    /// Default directory for new sessions (from `[bridge] work_dir`).
    pub work_dir: Option<String>,
    /// The configured base ref for `/diff` (`[review] base`, spec #803).
    pub review_base: Option<String>,
    /// The parent directory for `/worktree new` (`[worktree] root`, ADR-0088).
    pub worktree_root: std::path::PathBuf,
    /// `/diff`'s process-internal cooldown (spec #803): keyed by chat id,
    /// claimed before any git/render/upload work.
    pub diff_cooldown: Arc<std::sync::Mutex<crate::bridge::repo_diff::DiffCooldown>>,
    /// The **Terminal Command** slot registry (ADR-0086 decision 6): one run
    /// per **Chat**/**Topic**, claimed atomically before anything is spawned and
    /// released when the process group empties. Card actions reach the live run
    /// through the control channel this holds, so 终止 and 继续 cross tasks.
    pub terminal_runs: Arc<std::sync::Mutex<crate::bridge::terminal::TerminalRuns>>,
    /// The **Terminal Run Record** store (ADR-0086 decision 5): one small
    /// durable file per Chat/Topic, beside the session mapping. Written before
    /// a child is spawned, kept until the chat's next run replaces it, and read
    /// by the startup sweep and by 「↻ 重跑」's stale check.
    pub terminal_records: Arc<crate::bridge::terminal_record::TerminalRunRecords>,
    /// `[bridge] log_days`, the age bound the **Terminal Log** sweep shares with
    /// the application log's retention (ADR-0086 decision 10): swept by age,
    /// never wholesale, so a live orphan's file is not unlinked.
    pub terminal_log_days: u32,
    /// New sessions cola creates start with Auto-Accept on (from
    /// `[bridge] default_auto_accept`, #513). Adoptions ignore it and keep
    /// the session's own state.
    pub default_auto_accept: bool,
    /// Whether to send the group completion notice (from `[bridge] group_completion_notice`).
    pub group_completion_notice: bool,
    /// Whether to send the long-task completion notice in p2p (from `[bridge] long_task_notice`).
    pub long_task_notice: bool,
    /// The long-task notice threshold (ms): a p2p Turn that ran at least this
    /// long notifies on completion. Injectable for tests (the external
    /// poller's interval-atomics pattern).
    pub long_task_notice_ms: Arc<std::sync::atomic::AtomicU64>,
    /// The Completion Notice's repaint delay (ms, #650): how long the notice
    /// waits after its terminal write's carrier is accepted before it is sent,
    /// so the client has a chance to repaint the card first. Injectable for
    /// tests (the same atomics pattern as the threshold above).
    pub completion_notice_delay_ms: Arc<std::sync::atomic::AtomicU64>,
    /// The Instant Reminder lifecycle (ADR-0043, from `[bridge] instant_reminder`): pins a
    /// Chat/Topic while a Permission/Question is pending. Off means every
    /// method is a no-op — no reminder call is ever made.
    pub reminder: Arc<crate::bridge::reminder::ReminderState>,
    /// The waiting-card pin registry (ADR-0043 amendment): pins the exact card
    /// a pending Permission/Question lives on into its chat's pinned-message
    /// list, so the reminder's list-level nudge leads to the waiting message.
    /// Same `[bridge] instant_reminder` opt-in as the reminder itself.
    pub message_pins: Arc<crate::bridge::message_pins::MessagePins>,
    /// Cached session-list snapshot for `/switch`, `/attach`
    /// (30 s TTL; invalidated on create/adopt/rename). Private: the core's
    /// write wrappers and `invalidate_session_list_cache` own it.
    session_list_cache: Arc<Mutex<Option<SessionListCache>>>,
    /// The active agent client (spec #860): the [`crate::backend::Backend`]
    /// every bridge path talks to. Named `client`, not `opencode`: the seam is
    /// a client, and the active one becomes a choice (#872). Today it is always
    /// the OpenCode adapter.
    pub client: Arc<dyn crate::backend::Backend>,
    pub feishu: Arc<dyn feishu::Platform>,
    /// session_id → the lock serializing that session's card writes. A card
    /// write is a read-send-record sequence (the flush's build→PATCH→record, a
    /// resolution's mutate→ack/PATCH), and two of them interleaving land out of
    /// order: a stale flush PATCHes a resolution away, or a split's
    /// continuation content is written onto the card it just finalized. One
    /// writer per session at a time; entries are never evicted (bounded by the
    /// session store). Private: `card_write_lock` is the accessor.
    card_write_locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
    /// How long a preserved-ending card-view read is awaited before the ending
    /// degrades to the bare card (ms, spec #571 review). Shared with the
    /// [`CardsHandle`] the reap's collect and settle paths read it through; the
    /// Session Sync pass's own request bound is the default.
    preserved_view_timeout_ms: Arc<std::sync::atomic::AtomicU64>,
    /// The shared Background Task runtime reconciliation (#589): one verdict
    /// per Session per interval across Session Sync and the live loops.
    pub runtime_reconcile: Arc<crate::bridge::runtime::RuntimeReconcile>,
    /// The process-local File Content image cache (ADR-0076): a content hash →
    /// the single-flight resolution of that content (`Some(image_key)` when
    /// embedded, `None` when the content is not an embeddable image), so
    /// identical bytes upload once and every later PATCH reuses the key, and
    /// concurrent resolves of one hash await one upload. Never durable: an
    /// upload is cheap to repeat after a restart, and a stale key would be
    /// worse.
    pub file_images: crate::bridge::handles::FileImageCache,
    /// The process-local File Content file cache (ADR-0076, #649): a content
    /// hash → the single-flight resolution of that content as a File Message
    /// (`Some(file_key)` deliverable, `None` past Feishu's 30MB cap).
    pub file_uploads: crate::bridge::handles::FileUploadCache,
    /// The process-local File Message once-guard (ADR-0076, #649): the
    /// `(session_id, content hash)` entries whose File Message already went out,
    /// so a later poll never sends a second one. Single-flight and bounded.
    pub file_messages_sent: crate::bridge::handles::FileMessageSends,
    /// How long ONE network call of the **File Viewer**'s whole-bytes lane is
    /// awaited before it gives up (ms, ADR-0083 §5): the upload and the File
    /// Message send each get this bound, so the 「发为文件」 transfer that the
    /// click spawns off the card callback (ADR-0078's 3 s budget) can neither
    /// run forever nor leave the clicker without a stated outcome. Shared with
    /// the handler through the aggregate; a test shrinks it to observe a
    /// timeout.
    pub viewer_transfer_timeout_ms: Arc<std::sync::atomic::AtomicU64>,
    /// The File Viewer's bounded blocking-read budget, shared by `/cat` and
    /// every card action on this bridge instance.
    pub viewer_fs_read_slots: Arc<tokio::sync::Semaphore>,
    /// The **Terminal Log** root (ADR-0086 §7): unset in production, where a
    /// run writes under the per-user `~/.cola/terminal`; set once by a test so
    /// its runs never leave files in the real home. Internal only — deliberately
    /// not a config knob, since the location is part of the ADR's at-rest
    /// surface.
    pub terminal_log_root: Arc<std::sync::OnceLock<std::path::PathBuf>>,
    /// Test-only: variables the child would inherit BEFORE the credential scrub
    /// (ADR-0086 §8). Lets a test model cola's own environment without mutating
    /// the process environment, which is unsound under parallel `cargo test`.
    /// The field itself exists only in a test build, so production cannot set
    /// it.
    #[cfg(test)]
    pub terminal_test_env: Arc<std::sync::Mutex<Vec<(String, String)>>>,
    /// Test-only seam for one size-probe cycle (whole-branch review, finding 2):
    /// it lets a test park a cycle across the run's ending and prove no signal
    /// is sent after it. The field itself exists only in a test build.
    #[cfg(test)]
    pub terminal_probe_hook: Arc<std::sync::Mutex<Option<Arc<crate::bridge::terminal::TerminalProbeHook>>>>,
    /// Test-only: how many live-patch tasks a run's render loop spawned. A test
    /// holds the card write across many ticks and proves the count stays bounded.
    #[cfg(test)]
    pub terminal_live_patch_starts: Arc<std::sync::atomic::AtomicUsize>,
    /// Test-only: how many cap-announcing live patches to fail before letting one
    /// through; proves the cap state is retried until it lands.
    #[cfg(test)]
    pub terminal_fail_cap_patches: Arc<std::sync::atomic::AtomicUsize>,
    /// The **Terminal Log**'s **observed** stop threshold (ADR-0086 decision 7):
    /// the size probe kills the process group once the file has passed it. It
    /// is not a write limit, so a fast producer overshoots. Scaled down by a
    /// test, which cannot afford to write 29 MB; production uses
    /// [`crate::bridge::terminal::TERMINAL_LOG_CAP_BYTES`].
    pub terminal_log_cap_bytes: Arc<std::sync::atomic::AtomicU64>,
    /// How often that probe reads the log's metadata (ms) — its own short
    /// interval, never the slower render tick (ADR-0086 decision 7). A test
    /// shrinks it.
    pub terminal_size_probe_ms: Arc<std::sync::atomic::AtomicU64>,
}

impl SharedCore {
    pub fn new(
        cfg: &crate::config::Config,
        layout: crate::state::StateLayout,
        client: Arc<dyn crate::backend::Backend>,
        feishu: Arc<dyn feishu::Platform>,
    ) -> anyhow::Result<Self> {
        // ADR-0067: every card write goes through the delivery decorator, so a
        // failed one is remembered as a Pending Card Update and retried until
        // it lands. The raw platform stays the single Feishu transport; the
        // decorator is what the bridge (and the WS loop) holds.
        let feishu: Arc<dyn feishu::Platform> = Arc::new(crate::feishu::delivery::CardDelivery::new(feishu));
        let session_store = SessionStore::new(layout.session_file().to_path_buf())?;
        // The interactive surfaces a previous process persisted (ADR-0038
        // restart re-adoption), beside the session mapping like the reminder's
        // pin file. Loaded once here; the registry and both flows hydrate from
        // it and write their mutations back through it.
        let surfaces = Arc::new(crate::bridge::surfaces::Surfaces::load(
            layout.interactive_surfaces(),
        ));
        // The long-task notice threshold, created once: the Turn config and
        // Session Sync's quiet true-end settle (ADR-0060) must read the SAME
        // atomic, so a test's tiny injection moves both.
        let long_task_notice_ms = Arc::new(std::sync::atomic::AtomicU64::new(
            crate::bridge::turn::LONG_TASK_NOTICE_MS,
        ));
        // The Completion Notice's repaint delay (#650): created once beside the
        // threshold, for the same reason — the Turn's endings and Session Sync's
        // quiet true-end settle must read the SAME atomic.
        let completion_notice_delay_ms = Arc::new(std::sync::atomic::AtomicU64::new(
            crate::bridge::turn::COMPLETION_NOTICE_DELAY_MS,
        ));
        // The durable Chain Record, created once beside the Background Task
        // State handle that seeds from it (ADR-0069/0079).
        let chains = Arc::new(crate::bridge::chain::ChainRecords::load(layout.chain_records()));
        // The Background Task State handle is the process's ONE instance:
        // `src/main.rs` creates it and threads it into the adapter at
        // construction (#682), and the Bridge takes that same handle here rather
        // than building a second — so a retirement the Bridge records is what
        // every later adapter read applies. Its durable tier is attached now,
        // where the Chain Record is built: the handle existed before the record
        // did. Attach is idempotent and replaceable.
        let tasks = client.tasks().clone();
        tasks.attach_chains(Arc::clone(&chains));
        Ok(Self {
            sessions: Arc::new(Mutex::new(session_store)),
            cards: Arc::new(Mutex::new(HashMap::new())),
            card_handles: Arc::new(Mutex::new(
                crate::bridge::card_handles::CardHandles::with_surfaces(Arc::clone(&surfaces)),
            )),
            chains: Arc::clone(&chains),
            tasks,
            permission: Arc::new(crate::bridge::request::flow::RequestFlow::new(
                Box::new(crate::bridge::request::kind::PermissionKind),
                Arc::clone(&surfaces),
            )),
            question: Arc::new(crate::bridge::request::flow::RequestFlow::new(
                Box::new(crate::bridge::request::kind::QuestionKind),
                Arc::clone(&surfaces),
            )),
            external: Arc::new(crate::bridge::external::ExternalFlow::new(
                crate::bridge::handles::NoticeRules::new(
                    cfg.bridge.group_completion_notice,
                    cfg.bridge.long_task_notice,
                    Arc::clone(&long_task_notice_ms),
                    Arc::clone(&completion_notice_delay_ms),
                ),
            )),
            snapshot_claims: Arc::new(Mutex::new(
                crate::bridge::snapshot_claims::SnapshotClaims::default(),
            )),
            answered_requests: Arc::new(Mutex::new(HashSet::new())),
            settling_requests: Arc::new(Mutex::new(HashMap::new())),
            inflight: Arc::new(Mutex::new(HashSet::new())),
            inbound: Arc::new(Mutex::new(HashMap::new())),
            stopped_sessions: Arc::new(Mutex::new(HashSet::new())),
            turn_render_poll_ms: Arc::new(std::sync::atomic::AtomicU64::new(1_500)),
            turn_follow_grace_ms: Arc::new(std::sync::atomic::AtomicU64::new(600_000)),
            turn_follow_read_timeout_ms: Arc::new(std::sync::atomic::AtomicU64::new(30_000)),
            cover_titles: Arc::new(Mutex::new(HashMap::new())),
            work_dir: cfg
                .bridge
                .work_dir
                .clone()
                .map(|p| p.to_string_lossy().to_string()),
            review_base: cfg.review.base.clone(),
            worktree_root: cfg.worktree.root.clone(),
            diff_cooldown: Arc::new(std::sync::Mutex::new(
                crate::bridge::repo_diff::DiffCooldown::default(),
            )),
            terminal_runs: Arc::new(std::sync::Mutex::new(
                crate::bridge::terminal::TerminalRuns::default(),
            )),
            terminal_records: Arc::new(crate::bridge::terminal_record::TerminalRunRecords::new(
                layout.terminal_runs(),
            )),
            terminal_log_days: cfg.bridge.log_days,
            default_auto_accept: cfg.bridge.default_auto_accept,
            group_completion_notice: cfg.bridge.group_completion_notice,
            long_task_notice: cfg.bridge.long_task_notice,
            long_task_notice_ms,
            completion_notice_delay_ms,
            reminder: Arc::new(crate::bridge::reminder::ReminderState::new(
                cfg.bridge.instant_reminder,
                // The persisted pin set lives beside the session mapping
                // (#249), so startup can clear reminders a crash orphaned.
                Some(layout.pinned_chats()),
            )),
            message_pins: Arc::new(crate::bridge::message_pins::MessagePins::new(
                cfg.bridge.instant_reminder,
            )),
            session_list_cache: Arc::new(Mutex::new(None)),
            client,
            feishu,
            card_write_locks: Arc::new(Mutex::new(HashMap::new())),
            // The preserved-ending view read's bound: the Session Sync pass's
            // own request bound (spec #571 review).
            preserved_view_timeout_ms: Arc::new(std::sync::atomic::AtomicU64::new(30_000)),
            runtime_reconcile: Arc::new(crate::bridge::runtime::RuntimeReconcile::new()),
            file_images: Arc::new(Mutex::new(indexmap::IndexMap::new())),
            file_uploads: Arc::new(Mutex::new(indexmap::IndexMap::new())),
            file_messages_sent: Arc::new(Mutex::new(indexmap::IndexMap::new())),
            terminal_log_root: Arc::new(std::sync::OnceLock::new()),
            #[cfg(test)]
            terminal_test_env: Arc::new(std::sync::Mutex::new(Vec::new())),
            #[cfg(test)]
            terminal_probe_hook: Arc::new(std::sync::Mutex::new(None)),
            #[cfg(test)]
            terminal_live_patch_starts: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            #[cfg(test)]
            terminal_fail_cap_patches: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            terminal_log_cap_bytes: Arc::new(std::sync::atomic::AtomicU64::new(
                crate::bridge::terminal::TERMINAL_LOG_CAP_BYTES,
            )),
            terminal_size_probe_ms: Arc::new(std::sync::atomic::AtomicU64::new(
                crate::bridge::terminal::TERMINAL_SIZE_PROBE_MS,
            )),
            // The File Viewer transfer's per-network-call bound (ADR-0083 §5).
            viewer_transfer_timeout_ms: Arc::new(std::sync::atomic::AtomicU64::new(
                crate::bridge::viewer::VIEWER_TRANSFER_TIMEOUT_MS,
            )),
            viewer_fs_read_slots: Arc::new(tokio::sync::Semaphore::new(
                crate::bridge::viewer::MAX_CONCURRENT_VIEWER_FS_READS,
            )),
            layout,
        })
    }

    /// The narrow handles a Turn runs on (spec #298, A1): sessions, cards, the
    /// request and wait state, the backend, the platform and the turn config —
    /// the concerns [`crate::bridge::turn::Turn`] uses. The coordinator builds
    /// this and passes it to `Turn::run`; the Turn never sees the aggregate.
    pub(crate) fn turn_handles(&self) -> crate::bridge::handles::TurnHandles {
        crate::bridge::handles::TurnHandles {
            sessions: self.sessions_handle(),
            cards: self.cards_handle(),
            requests: self.requests_handle(),
            waits: self.waits_handle(),
            backend: Arc::clone(&self.client),
            platform: Arc::clone(&self.feishu),
            config: self.turn_config(),
            runtime_reconcile: Arc::clone(&self.runtime_reconcile),
            layout: self.layout.clone(),
        }
    }

    /// The common flow bundle (spec #298, B): the four per-concern handles plus
    /// the backend and the platform. The request and external flows run on it.
    pub(crate) fn flow_handles(&self) -> crate::bridge::handles::FlowHandles {
        crate::bridge::handles::FlowHandles {
            sessions: self.sessions_handle(),
            cards: self.cards_handle(),
            requests: self.requests_handle(),
            waits: self.waits_handle(),
            backend: Arc::clone(&self.client),
            platform: Arc::clone(&self.feishu),
            runtime_reconcile: Arc::clone(&self.runtime_reconcile),
            layout: self.layout.clone(),
        }
    }

    /// The read-side bundle a Session Snapshot gather needs.
    pub(crate) fn snapshot_handles(&self) -> crate::bridge::handles::SnapshotHandles {
        crate::bridge::handles::SnapshotHandles::from_flow(&self.flow_handles())
    }

    /// The topic-opening bundle.
    pub(crate) fn topic_handles(&self) -> crate::bridge::handles::TopicHandles {
        crate::bridge::handles::TopicHandles {
            flow: self.flow_handles(),
            external: Arc::clone(&self.external),
        }
    }

    /// The command bundle.
    pub(crate) fn command_handles(&self) -> crate::bridge::handles::CommandHandles {
        crate::bridge::handles::CommandHandles {
            flow: self.flow_handles(),
            config: self.turn_config(),
            external: Arc::clone(&self.external),
            diff_cooldown: Arc::clone(&self.diff_cooldown),
            terminal_runs: Arc::clone(&self.terminal_runs),
            terminal_records: Arc::clone(&self.terminal_records),
            terminal_log_root: Arc::clone(&self.terminal_log_root),
            terminal_log_cap_bytes: Arc::clone(&self.terminal_log_cap_bytes),
            terminal_size_probe_ms: Arc::clone(&self.terminal_size_probe_ms),
            #[cfg(test)]
            terminal_test_env: Arc::clone(&self.terminal_test_env),
            #[cfg(test)]
            terminal_probe_hook: Arc::clone(&self.terminal_probe_hook),
            #[cfg(test)]
            terminal_live_patch_starts: Arc::clone(&self.terminal_live_patch_starts),
            #[cfg(test)]
            terminal_fail_cap_patches: Arc::clone(&self.terminal_fail_cap_patches),
        }
    }

    /// The session map + list cache as a narrow handle.
    pub(crate) fn sessions_handle(&self) -> crate::bridge::handles::SessionsHandle {
        crate::bridge::handles::SessionsHandle::new(
            Arc::clone(&self.sessions),
            Arc::clone(&self.session_list_cache),
            self.default_auto_accept,
        )
    }

    /// The live cards, the card-handle registry, the card-write locks, the
    /// cover records and the platform as a narrow handle.
    pub(crate) fn cards_handle(&self) -> crate::bridge::handles::CardsHandle {
        crate::bridge::handles::CardsHandle::new(
            Arc::clone(&self.cards),
            Arc::clone(&self.card_handles),
            Arc::clone(&self.cover_titles),
            Arc::clone(&self.feishu),
            Arc::clone(&self.chains),
            self.tasks.clone(),
            Arc::clone(&self.card_write_locks),
            Arc::clone(&self.preserved_view_timeout_ms),
            Arc::clone(&self.file_images),
            Arc::clone(&self.file_uploads),
            Arc::clone(&self.file_messages_sent),
            Arc::clone(&self.viewer_transfer_timeout_ms),
            Arc::clone(&self.viewer_fs_read_slots),
        )
    }

    /// The two request flows and their claim state as a narrow handle.
    pub(crate) fn requests_handle(&self) -> crate::bridge::handles::RequestsHandle {
        crate::bridge::handles::RequestsHandle {
            permission: Arc::clone(&self.permission),
            question: Arc::clone(&self.question),
            answered_requests: Arc::clone(&self.answered_requests),
            settling_requests: Arc::clone(&self.settling_requests),
            snapshot_claims: Arc::clone(&self.snapshot_claims),
        }
    }

    /// The per-session wait state as a narrow handle.
    pub(crate) fn waits_handle(&self) -> crate::bridge::handles::WaitsHandle {
        crate::bridge::handles::WaitsHandle {
            inflight: Arc::clone(&self.inflight),
            inbound: Arc::clone(&self.inbound),
            stopped_sessions: Arc::clone(&self.stopped_sessions),
            reminder: Arc::clone(&self.reminder),
            message_pins: Arc::clone(&self.message_pins),
        }
    }

    /// The turn knobs as a narrow handle.
    pub(crate) fn turn_config(&self) -> crate::bridge::handles::TurnConfig {
        crate::bridge::handles::TurnConfig::new(
            self.group_completion_notice,
            self.long_task_notice,
            Arc::clone(&self.long_task_notice_ms),
            Arc::clone(&self.turn_render_poll_ms),
            Arc::clone(&self.turn_follow_grace_ms),
            Arc::clone(&self.turn_follow_read_timeout_ms),
            Arc::clone(&self.completion_notice_delay_ms),
            self.work_dir.clone(),
            self.review_base.clone(),
            self.worktree_root.clone(),
        )
    }

    /// The directory a brand-new session starts in (see
    /// [`crate::bridge::handles::TurnConfig::default_session_directory`]).
    pub fn default_session_directory(&self) -> String {
        self.turn_config().default_session_directory()
    }

    /// The per-session agent override set by `/agent` (from the persisted
    /// `SessionEntry`). `None` when the session has no override — the server
    /// then uses the session's own/default agent.
    pub async fn session_agent_override(&self, session_id: &str) -> Option<String> {
        self.sessions_handle().agent_override(session_id).await
    }

    /// The per-session model override set by `/model`, parsed from the
    /// persisted "provider/model" string. `None` when the session has no
    /// override (the client then falls back to the configured default model, or
    /// the server's own default if none is configured).
    pub async fn session_model_override(&self, session_id: &str) -> Option<crate::backend::types::ModelInfo> {
        self.sessions_handle().model_override(session_id).await
    }

    /// The per-session `/think` variant override (from the persisted
    /// `SessionEntry`). `None` when unset — the server's default for whatever
    /// model runs this turn.
    pub async fn session_variant_override(&self, session_id: &str) -> Option<String> {
        self.sessions_handle().variant_override(session_id).await
    }

    /// The model the NEXT turn will actually run, resolved generation-natively:
    /// the session's durable selection where the generation keeps one (V2),
    /// else settings override → configured default → server-recorded session
    /// model (`GET /session/{id}`). `None` only when every rung fails (no
    /// durable selection, no override, no config, server unreachable) — the
    /// `/think` card then tells the user to `/model` first, and the `/model`
    /// picker omits its current-model line. Returns `(provider, model)`. A
    /// Pending Session has no server-recorded rung (nothing exists on the
    /// server yet, ADR-0041).
    pub async fn effective_model(&self, settings: &SessionSettings) -> Option<(String, String)> {
        self.sessions_handle()
            .effective_model(&self.client, settings)
            .await
    }

    /// Apply a `/model` pick (ADR-0020 + V2's durable switch).
    pub async fn pick_model(
        &self,
        thread_key: &ThreadKey,
        model_spec: &str,
    ) -> crate::error::Result<crate::bridge::handles::PickOutcome> {
        self.sessions_handle()
            .pick_model(&self.client, thread_key, model_spec)
            .await
    }

    /// Apply a `/think` pick (`variant: None` clears).
    pub async fn pick_think(
        &self,
        thread_key: &ThreadKey,
        variant: Option<String>,
    ) -> crate::error::Result<crate::bridge::handles::PickOutcome> {
        self.sessions_handle()
            .pick_think(&self.client, thread_key, variant)
            .await
    }

    /// Apply an `/agent` pick (`agent: None` resets to the server default).
    pub async fn pick_agent(
        &self,
        thread_key: &ThreadKey,
        agent: Option<String>,
    ) -> crate::error::Result<crate::bridge::handles::PickOutcome> {
        self.sessions_handle()
            .pick_agent(&self.client, thread_key, agent)
            .await
    }

    /// The declared variants of a provider/model, per `GET /provider`.
    /// Best-effort: `None` when the model can't be found in the advertised
    /// catalog (callers then leave a stored variant in place rather than
    /// destroying it on an unknown).
    pub async fn model_variants(&self, provider: &str, model: &str) -> Option<Vec<String>> {
        self.sessions_handle()
            .model_variants(&self.client, provider, model)
            .await
    }

    /// The session mapped to a thread (if any).
    pub async fn get_session_id(&self, thread_key: &ThreadKey) -> Option<String> {
        self.sessions_handle().get_session_id(thread_key).await
    }

    /// The current `GET /session` snapshot, fetching (and caching for 30 s) when
    /// missing or stale. Used by `/switch` and `/attach` so rapid
    /// reuse stays off the wire.
    pub(crate) async fn cached_session_list(
        &self,
    ) -> crate::error::Result<Vec<crate::backend::types::SessionListInfo>> {
        self.sessions_handle().cached_session_list(&self.client).await
    }

    /// Drop the session-list cache. Called whenever cola creates, adopts, forgets or
    /// renames a session, so the next `/switch`/`/attach` is fresh.
    pub(crate) async fn invalidate_session_list_cache(&self) {
        self.sessions_handle().invalidate_cache().await;
    }

    /// Persist `entry` as its thread's active session and drop the session-list
    /// cache: creating or adopting a session changes what `/switch`
    /// should offer. The cache is dropped even when the save fails, because the
    /// in-memory mapping already changed. Goes through the collecting
    /// activation, so a waiting card of the Session this activation displaces
    /// is collected first (ADR-0059, spec #405) and a switch away never leaves
    /// a card on 「⏳ 等待后台任务」.
    pub(crate) async fn activate_session(&self, entry: SessionEntry) -> crate::error::Result<()> {
        self.flow_handles().activate(entry).await
    }

    /// The settings the conversation's next prompt will use (ADR-0041): the
    /// Pending Session's when one exists, else the active session's. `None`
    /// when the thread has neither. The four settings commands
    /// (`/agent` `/model` `/think` `/autoaccept`), their cards and the
    /// effective-model ladder all read through this one accessor.
    pub(crate) async fn session_settings(&self, thread_key: &ThreadKey) -> Option<SessionSettings> {
        self.sessions_handle().session_settings(thread_key).await
    }

    /// Write a [`SessionSettings`] snapshot back to its target (a real session
    /// by id, else the thread's Pending Session) and persist. `false` when the
    /// target is gone.
    pub(crate) async fn set_session_settings(
        &self,
        thread_key: &ThreadKey,
        settings: SessionSettings,
    ) -> crate::error::Result<bool> {
        self.sessions_handle()
            .set_session_settings(thread_key, settings)
            .await
    }

    /// After `/autoaccept on`: answer every permission request that is ALREADY
    /// pending for `session_id` (or one of its sub-task child sessions) with
    /// "once". The permission poller's `seen` set skips requests it has already
    /// surfaced, so enabling autoaccept would otherwise leave old cards hanging
    /// forever. Returns the ids of the requests that were approved.
    pub(crate) async fn approve_pending_for_session(&self, session_id: &str, directory: &str) -> Vec<String> {
        crate::bridge::request::kind::approve_pending_for_session(
            &self.sessions_handle(),
            &self.requests_handle(),
            &self.client,
            session_id,
            directory,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::test_support::{MockBackend, build_app, realistic_parts, test_config, test_work_dir};
    use std::sync::atomic::Ordering;

    /// The cache rule is part of the session write interface: creates and
    /// removes change what `/switch` should show, overrides do not.
    #[tokio::test]
    async fn activate_invalidates_list_cache_but_update_does_not() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let backend = MockBackend::new(realistic_parts());
        let sessions_fetches = backend.list_sessions_calls.clone();
        let (app, _platform) = build_app(cfg, backend).await;
        let key = ThreadKey::new("chat_1".into(), "chat_1".into());

        app.cached_session_list().await.unwrap();
        assert_eq!(sessions_fetches.load(Ordering::SeqCst), 1);
        app.cached_session_list().await.unwrap();
        assert_eq!(
            sessions_fetches.load(Ordering::SeqCst),
            1,
            "second read is cached"
        );

        app.activate_session(SessionEntry::new(key, "ses_x", "/work/x"))
            .await
            .unwrap();
        app.cached_session_list().await.unwrap();
        assert_eq!(sessions_fetches.load(Ordering::SeqCst), 2, "activate invalidates");

        let updated = app
            .sessions_handle()
            .update("ses_x", |e| e.agent = Some("build".into()))
            .await
            .unwrap()
            .expect("mapped session");
        assert_eq!(updated.agent.as_deref(), Some("build"));
        app.cached_session_list().await.unwrap();
        assert_eq!(
            sessions_fetches.load(Ordering::SeqCst),
            2,
            "update keeps the cache"
        );

        assert!(app.flow_handles().unmap("ses_x").await.unwrap().is_some());
        app.cached_session_list().await.unwrap();
        assert_eq!(sessions_fetches.load(Ordering::SeqCst), 3, "remove invalidates");
    }

    /// A settlement claim older than the TTL is abandoned: the sweep must not
    /// keep suppressing a request whose settlement task was cancelled between
    /// the reply and the receipt.
    #[tokio::test]
    async fn claimed_requests_drops_an_abandoned_settlement_claim() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

        let stale = std::time::Instant::now()
            .checked_sub(SETTLING_CLAIM_TTL + std::time::Duration::from_secs(1))
            .unwrap();
        app.settling_requests.lock().await.insert("per_old".into(), stale);
        let claimed = app.requests_handle().claimed_requests().await;

        assert!(!claimed.contains("per_old"));
        assert!(
            app.settling_requests.lock().await.is_empty(),
            "the abandoned claim is pruned, not re-read"
        );

        // A fresh claim is honoured.
        app.settling_requests
            .lock()
            .await
            .insert("per_new".into(), std::time::Instant::now());
        assert!(app.requests_handle().claimed_requests().await.contains("per_new"));
    }

    /// The override write path must not silently switch the Active Session:
    /// `set_auto_accept` targets any mapped session (it walks the parent
    /// chain), so a toggle on a non-active mapping updates it in place. The
    /// old clone-then-`set_active` shape promoted it.
    #[tokio::test]
    async fn set_auto_accept_does_not_promote_a_non_active_mapping() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
        let key = ThreadKey::new("chat_1".into(), "chat_1".into());

        // Two mappings for one thread; the last activated is the active one.
        app.activate_session(SessionEntry::new(key.clone(), "ses_b", "/work/b"))
            .await
            .unwrap();
        app.activate_session(SessionEntry::new(key.clone(), "ses_a", "/work/a"))
            .await
            .unwrap();
        assert_eq!(app.get_session_id(&key).await.as_deref(), Some("ses_a"));

        crate::bridge::request::kind::set_auto_accept(
            &app.sessions_handle(),
            &app.requests_handle(),
            &app.client,
            "ses_b",
            "/work/b",
            true,
        )
        .await;

        assert_eq!(
            app.get_session_id(&key).await.as_deref(),
            Some("ses_a"),
            "toggling auto-accept on a non-active mapping keeps the active session"
        );
        let sessions = app.sessions.lock().await;
        assert!(sessions.entry_for_session("ses_b").unwrap().auto_accept);
    }
}
