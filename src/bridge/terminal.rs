//! **Terminal Commands** (ADR-0086): the Host-capability module that runs one
//! shell command for `/run` and for a `!`-prefixed message. Backend- and
//! generation-neutral, like the **File Viewer**'s `bridge::viewer`: this module
//! owns execution (spawn, wait, read the **Terminal Log**), while
//! `feishu::card::terminal` renders the **Terminal Card**. It never contacts the
//! OpenCode server — no `ensure_server`, no prompt, no session read — so it works
//! with `start_server = "never"` and while the server is down.
//!
//! Shape (ADR-0086 §2, §3, §13):
//! - one `sh -c` child, cwd on the **Path Scope**'s base, stdin closed, merged
//!   stdout/stderr written **straight into** the Terminal Log — never read
//!   through a pipe, so the command's bytes are on disk even if cola dies;
//! - the child leaves cola's process group with the safe `process_group(0)`
//!   route, so a terminal hangup does not reach it;
//! - the log lives in a 0700 directory as a 0600 file under
//!   `~/.cola/terminal/`;
//! - the card is posted in 「启动中」, then patched on the render cadence while
//!   the run is live; the settling write is last and the loop stops on it
//!   (#823, decision 4);
//! - one run per **Chat**/**Topic** is claimed atomically, and the run is live
//!   while its **process group** has members — not while `sh` is alive — so a
//!   background child keeps the claim, the tail and 「⏹ 终止」 until the group
//!   empties (decision 6);
//! - `--timeout` caps the live tail, never the wait: 「继续」 extends it by the
//!   same amount while the run is past the cap and live; 终止 and a failed size
//!   probe terminate the whole group through #826's `terminate_process_group`;
//! - a long run sends the existing chat-type Completion Notice once
//!   (decision 12);
//! - Unix only: Windows refuses with a stated reason and spawns nothing
//!   (decision 13). The durable **Terminal Run Record** and its restart sweep
//!   remain #827's slice.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::mpsc;

use crate::bridge::command::RunSpec;
use crate::bridge::handles::CommandHandles;
use crate::config::ThreadKey;
use crate::feishu::Platform;
use crate::feishu::card::terminal::{
    TERMINAL_TAIL_CHARS, TerminalCard, TerminalFullOutput, TerminalState, build_terminal_card, elapsed_label,
};

/// The **Terminal Log**'s **observed** stop threshold (ADR-0086 decision 7):
/// 29 MB. It is deliberately NOT a write limit — no portable safe primitive
/// bounds one descriptor's size (`RLIMIT_FSIZE` is per process and would cap
/// the command's own build artifacts, and truncating the live file does not
/// move the child's offset back). So a dedicated probe reads the file's
/// metadata and kills the process group once the file has passed this size;
/// a fast producer can overshoot it before the probe looks, and that is
/// stated rather than hidden. The threshold sits below the platform's 30 MB
/// delivery cap so ordinary runs get their whole output.
pub(crate) const TERMINAL_LOG_CAP_BYTES: u64 = 29 * 1024 * 1024;

/// How often the size probe reads the log's metadata — its own short interval,
/// never the slower render tick (ADR-0086 decision 7). A metadata read is
/// cheap; this is the interval that decides how far a fast producer can
/// overshoot.
pub(crate) const TERMINAL_SIZE_PROBE_MS: u64 = 250;

/// How long the probe's `SIGTERM` is given before the process group is
/// `SIGKILL`ed: enough for a well-behaved child to die, short enough that a
/// stopped runaway never lingers.
#[cfg(unix)]
const KILL_GRACE: Duration = Duration::from_millis(500);

/// How often the termination sequence asks whether the group is empty yet.
#[cfg(unix)]
const GROUP_POLL: Duration = Duration::from_millis(10);

/// How long a `SIGKILL`ed group is given to disappear before the confirmation
/// gives up (a member in uninterruptible sleep, or a zombie awaiting its
/// reaper, cannot leave promptly). Reported, never silently ignored.
#[cfg(unix)]
const KILL_CONFIRM: Duration = Duration::from_secs(2);

/// The bound on ONE live-card PATCH. `Feishu::update_message` has no request
/// timeout of its own, so an unanswered write would otherwise pin the render
/// loop forever (the probe is independent, but the settle must stay
/// reachable).
const TERMINAL_PATCH_TIMEOUT_MS: u64 = 10_000;

/// The environment names cola's child must NOT inherit (ADR-0086 §8): the
/// server credentials cola itself holds. The same three names `discovery`
/// reads in the other direction; everything else is the Host's own shell.
#[cfg(unix)]
const SCRUBBED_ENV: [&str; 3] = [
    "OPENCODE_SERVER_PASSWORD",
    "OPENCODE_PASSWORD",
    "OPENCODE_SERVER_USERNAME",
];

/// The bare `/run` / refused-flag answer. It names the grammar, the Path Scope
/// base and the two practical limits the command's user meets first; `/help
/// run` (ticket #824) carries the long form.
pub(crate) const RUN_USAGE: &str = "\
用法：`/run [--timeout <n>[smh]] [--] <命令>` —— 在宿主机上执行一条 shell 命令，并把输出回复成终端卡片。
- 命令在**当前会话目录**执行；会话还没创建时用当前项目目录，都没有则用默认目录。
- `--timeout` 是本次运行的时限（默认 10 分钟，最大 24 小时）；`--` 结束选项解析，命令本身就可以以 `-` 开头。
- 没有终端：需要 TTY 的程序会快速失败，请用非交互参数或自己用 tmux。
例：`/run echo hi`、`/run --timeout 30s cargo test`、`/run -- --version`";

/// Why a Windows Host is refused (ADR-0086 §13): the hard half is terminating
/// the whole process tree, not the shell spelling, so V1 says so instead of
/// running a half-implemented lane.
#[cfg(not(unix))]
pub(crate) const WINDOWS_REFUSAL: &str = "\
⚠️ Windows 上暂不支持 /run：无法安全地终止整棵进程树（ADR-0086），因此不会执行这条命令。";

/// Whether this Host can run a Terminal Command; the reason when it cannot.
#[cfg(unix)]
fn unsupported_reason() -> Option<&'static str> {
    None
}

#[cfg(not(unix))]
fn unsupported_reason() -> Option<&'static str> {
    Some(WINDOWS_REFUSAL)
}

/// The command message's own facts (ADR-0086 decision 12): the reply target,
/// the requester a Completion Notice must name, and whether the chat is a
/// group. A Terminal Command has no **Turn**, so it captures these at send.
#[derive(Debug, Clone, Copy)]
pub(crate) struct TerminalRequest<'a> {
    pub(crate) message_id: &'a str,
    pub(crate) requester_open_id: Option<&'a str>,
    pub(crate) is_group: bool,
}

/// The refusal a second command gets while the Chat/Topic's slot is claimed
/// (ADR-0086 decision 6): it names the running card's 「⏹ 终止」, the one act
/// that releases the slot.
pub(crate) const TERMINAL_BUSY: &str =
    "⚠️ 这个会话已有终端命令在运行，请先点击运行卡片的「⏹ 终止」后再执行。";

/// What a card button asks the live run to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TerminalOp {
    /// 「⏹ 终止」 — signal the whole process group and settle the card.
    Kill,
    /// 「继续」 — extend the cap by the same amount and resume the tail.
    Continue,
}

impl TerminalOp {
    pub(crate) fn parse(raw: &str) -> Option<Self> {
        match raw {
            "kill" => Some(Self::Kill),
            "continue" => Some(Self::Continue),
            _ => None,
        }
    }
}

/// One message to the run loop. A click only ever enqueues; the loop owns the
/// process group and the card.
#[derive(Debug)]
enum TerminalControl {
    Kill,
    Continue,
}

/// What a card action found (ADR-0086 decision 4): the click is refused by
/// state, not hidden, so a stale click is a no-op rather than an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TerminalControlOutcome {
    /// The signal reached the live run.
    Accepted,
    /// The run it named is no longer the Chat/Topic's current one (or is gone):
    /// a stale click.
    Stale,
    /// The run is live but has not reached its cap: 继续 does not apply yet.
    NotPastCap,
}

/// The live half of one Terminal Command, shared by the run loop and the
/// card-action handler. The run id names the card's buttons; liveness and the
/// cap are atomics so a click never waits on the loop.
#[derive(Debug)]
pub(crate) struct TerminalRun {
    run_id: String,
    /// True from claim until the process group empties (or the run is killed).
    live: AtomicBool,
    /// True exactly while the run is past its cap and still live — the one
    /// state in which 「继续」 is available.
    past_cap: AtomicBool,
    control: mpsc::UnboundedSender<TerminalControl>,
}

/// One **Terminal Command** slot per **Chat**/**Topic** (ADR-0086 decision 6).
/// The map is the atomic claim: it is inserted before anything is spawned, so
/// two inbound tasks cannot both start a command in one conversation.
#[derive(Default)]
pub(crate) struct TerminalRuns {
    runs: HashMap<ThreadKey, Arc<TerminalRun>>,
}

impl TerminalRuns {
    /// Claim `key` for `run`, or report the current run so the refusal can name
    /// its 「⏹ 终止」. The lock is held only for the map operation.
    pub(crate) fn claim(&mut self, key: &ThreadKey, run: Arc<TerminalRun>) -> Result<(), Arc<TerminalRun>> {
        if let Some(current) = self.runs.get(key) {
            return Err(Arc::clone(current));
        }
        self.runs.insert(key.clone(), run);
        Ok(())
    }

    /// Release `key` only if `run_id` is still its current run: a late settling
    /// path can never delete a successor's claim.
    pub(crate) fn release(&mut self, key: &ThreadKey, run_id: &str) {
        if self.runs.get(key).is_some_and(|run| run.run_id == run_id) {
            self.runs.remove(key);
        }
    }

    /// Route a card action to the Chat/Topic's current run (ADR-0086 decision
    /// 4's run-id check): a button for any other run id is stale.
    pub(crate) fn control(&self, key: &ThreadKey, run_id: &str, op: TerminalOp) -> TerminalControlOutcome {
        let Some(run) = self.runs.get(key) else {
            return TerminalControlOutcome::Stale;
        };
        if run.run_id != run_id || !run.live.load(Ordering::SeqCst) {
            return TerminalControlOutcome::Stale;
        }
        match op {
            TerminalOp::Kill => {}
            TerminalOp::Continue if !run.past_cap.load(Ordering::SeqCst) => {
                return TerminalControlOutcome::NotPastCap;
            }
            TerminalOp::Continue => {}
        }
        let control = match op {
            TerminalOp::Kill => TerminalControl::Kill,
            TerminalOp::Continue => TerminalControl::Continue,
        };
        if run.control.send(control).is_ok() {
            TerminalControlOutcome::Accepted
        } else {
            TerminalControlOutcome::Stale
        }
    }
}

/// The `!` entry point (ADR-0086 §1): a message whose first character is `!`
/// and whose remainder is non-empty runs that remainder as the command. The
/// text is already mention-stripped, and it is trimmed here, so `@cola !ls`
/// and `  !ls` qualify. `!` is a message prefix, not a **Command**: `!` alone,
/// `! ` and a `!` anywhere but the first character stay prose. The form takes
/// no flags — everything after `!` is the command — so `!ls #caveman` runs the
/// literal command and loads no **Skill**. `None` means prose.
pub(crate) fn parse_bang(text: &str) -> Option<String> {
    let command = text.trim().strip_prefix('!')?.trim();
    (!command.is_empty()).then(|| command.to_string())
}

/// Run one Terminal Command (ADR-0086 §2, §6) — the `/run` and `!` entry points
/// alike. `None` is the bare or refused `/run` form: answer usage and spawn
/// nothing.
///
/// The Chat/Topic's slot is claimed **before** the card is sent and the child is
/// spawned, and released only after the process group empties (or 终止 killed
/// it). The loop owns the process group: a card action only enqueues a control
/// message, so a click never waits on the run and a run never waits on a click
/// to notice its cap.
pub(crate) async fn execute(
    handles: &CommandHandles,
    thread_key: &ThreadKey,
    request: TerminalRequest<'_>,
    spec: Option<RunSpec>,
) -> crate::error::Result<()> {
    let platform = handles.flow.platform.as_ref();
    let Some(spec) = spec else {
        platform.reply_text(request.message_id, RUN_USAGE).await?;
        return Ok(());
    };
    if let Some(reason) = unsupported_reason() {
        platform.reply_text(request.message_id, reason).await?;
        return Ok(());
    }
    // One command per Chat/Topic (ADR-0086 decision 6): the claim is taken
    // here, not as a side effect of dispatch, because every inbound message is
    // its own task. A refusal names the running card's 「⏹ 终止」.
    let run = run_id();
    let (control, mut controls) = mpsc::unbounded_channel();
    let slot = Arc::new(TerminalRun {
        run_id: run.clone(),
        live: AtomicBool::new(false),
        past_cap: AtomicBool::new(false),
        control,
    });
    let claimed = {
        let mut runs = handles
            .terminal_runs
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        runs.claim(thread_key, Arc::clone(&slot)).is_ok()
    };
    if !claimed {
        platform.reply_text(request.message_id, TERMINAL_BUSY).await?;
        return Ok(());
    }
    let mut claim = TerminalClaim {
        runs: Arc::clone(&handles.terminal_runs),
        thread_key: thread_key.clone(),
        run_id: run.clone(),
        pgid: None,
        handed_off: false,
    };
    // The Path Scope (ADR-0083 §4, ADR-0086 §2): `current_project_directory` is
    // pending-first — the Pending Session's directory wins, else the active
    // session's, else the default — and the card names exactly this spelling.
    let cwd = handles.current_project_directory(thread_key).await;
    // The log root is `~/.cola/terminal` (ADR-0086 §7). No home means nowhere
    // for the output to go: refuse BEFORE spawning, never fall back to a
    // relative directory that could put command output in an unrelated tree.
    let Some(log_root) = handles
        .terminal_log_root
        .get()
        .cloned()
        .or_else(default_terminal_root)
    else {
        platform
            .reply_text(request.message_id, "⚠️ 无法创建终端日志：找不到 home 目录。")
            .await?;
        return Ok(());
    };
    let log = match TerminalLog::create(&log_root) {
        Ok(log) => log,
        Err(error) => {
            // A log that cannot be created is the one failure that must not
            // spawn: there would be nowhere for the output to go.
            platform
                .reply_text(request.message_id, &format!("⚠️ 无法创建终端日志：{error}"))
                .await?;
            return Ok(());
        }
    };
    // The test build can model environment entries cola's process would carry;
    // production passes none and simply inherits (then scrubs) the real one.
    #[cfg(test)]
    let inherited_env = handles
        .terminal_test_env
        .lock()
        .map(|env| env.clone())
        .unwrap_or_default();
    #[cfg(not(test))]
    let inherited_env: Vec<(String, String)> = Vec::new();

    // The live card goes out before the child exists (ADR-0086 decision 5's
    // card → record → spawn order, with the record a later slice): the run
    // always has a surface, and 「运行中」 is the state the loop patches. Its
    // buttons carry this run's id, so a click after a successor run is stale.
    let live = build_terminal_card(TerminalCard {
        cwd: &cwd,
        command: &spec.command,
        output: "",
        output_truncated: false,
        output_over_limit: false,
        full_output: None,
        notice: None,
        state: TerminalState::Starting,
        run_id: &run,
        chat_id: &thread_key.chat_id,
        thread_id: &thread_key.thread_id,
    });
    let card_id = platform.reply_card(request.message_id, &live).await?;
    // Every live PATCH goes through one write lock, and `settled` marks the
    // point after which no live PATCH may be issued. A parked/late live write
    // can then never land after the settling write: the settle takes the same
    // lock, and a task that only starts afterwards sees the flag and returns.
    let settled = Arc::new(AtomicBool::new(false));
    let card_write = Arc::new(tokio::sync::Mutex::new(()));
    let patch_facts = Arc::new(LivePatchFacts {
        platform: Arc::clone(&handles.flow.platform),
        card_id: card_id.clone(),
        cwd: cwd.clone(),
        command: spec.command.clone(),
        log: log.path.clone(),
        run_id: run.clone(),
        chat_id: thread_key.chat_id.clone(),
        thread_id: thread_key.thread_id.clone(),
    });

    let started = Instant::now();
    let (child, spawn_error) = match spawn_in(&log, &cwd, &spec.command, &inherited_env) {
        Ok(child) => (Some(child), None),
        Err(error) => {
            // No child means no exit code; the card says why instead of
            // freezing on 「运行中」.
            tracing::debug!("terminal: spawn failed: {error}");
            (None, Some(error.to_string()))
        }
    };
    let Some(mut child) = child else {
        settle_run(
            platform,
            &card_id,
            &cwd,
            &spec.command,
            &log.path,
            thread_key,
            &settled,
            &card_write,
            TerminalOutcome {
                exit_code: None,
                spawn_error: spawn_error.as_deref(),
                elapsed: started.elapsed(),
                stopped: false,
                survivors: false,
                output_over_limit: false,
            },
        )
        .await;
        claim.finish();
        return Ok(());
    };
    let pgid = child.id();
    claim.pgid = pgid;
    slot.live.store(true, Ordering::SeqCst);

    // #826's size probe runs on its own task; it owns the runaway kill and
    // returns only once the whole group is gone. This loop owns the claim,
    // liveness, the cap and 终止, and never reimplements that termination.
    let over_limit = Arc::new(AtomicBool::new(false));
    let finished = Arc::new(AtomicBool::new(false));
    let probe_ms = handles.terminal_size_probe_ms.load(Ordering::Relaxed).max(1);
    let log_cap_bytes = handles.terminal_log_cap_bytes.load(Ordering::Relaxed);
    let (stop_probe, stop_rx) = tokio::sync::oneshot::channel();
    let probe = tokio::spawn(run_size_probe(
        log.path.clone(),
        log_cap_bytes,
        pgid,
        Duration::from_millis(probe_ms),
        Arc::clone(&finished),
        Arc::clone(&over_limit),
        stop_rx,
    ));

    // The card was posted in 「启动中」 with no controls; as soon as the child
    // exists, patch it live so 「⏹ 终止」 is available without waiting a render
    // tick. `slot.live` is already true, so a click on it is actionable.
    spawn_live_patch(
        Arc::clone(&patch_facts),
        false,
        false,
        Arc::clone(&settled),
        Arc::clone(&card_write),
    );

    let cap = spec.timeout;
    let mut deadline = started + cap;
    let mut past_cap = false;
    let mut stopping = false;
    let mut stop_by_user = false;
    let mut child_exit: Option<std::io::Result<std::process::ExitStatus>> = None;
    let mut shown_survivors = false;
    let render_ms = handles.config.render_poll_ms().max(1);
    let mut render = tokio::time::interval(Duration::from_millis(render_ms));
    // A second ticker for the post-cap state: the two render arms cannot share
    // one `interval`, and past the cap only the survivor line is refreshed,
    // never the tail.
    let mut survivor_render = tokio::time::interval(Duration::from_millis(render_ms));
    render.tick().await;
    survivor_render.tick().await;

    loop {
        let alive = group_has_members(pgid);
        // The run is live while its process group has members — not while `sh`
        // is alive (ADR-0086 decision 6): a background child keeps the slot.
        if child_exit.is_some() && !alive {
            break;
        }
        if stopping {
            // `terminate_process_group` is #826's helper, not a second killer:
            // it SIGTERMs, escalates to SIGKILL and returns only once the group
            // is empty. Calling it even when `sh` already exited is what kills
            // a surviving background child.
            terminate_process_group(pgid).await;
            if child_exit.is_none() {
                child_exit = Some(child.wait().await);
            }
            break;
        }
        let survivors = child_exit.is_some() && alive;
        let cap_at = deadline;
        tokio::select! {
            control = controls.recv() => match control {
                Some(TerminalControl::Kill) => {
                    stopping = true;
                    stop_by_user = true;
                }
                Some(TerminalControl::Continue) => {
                    // 「继续」 extends the cap by the same amount and resumes
                    // the tail; the slot stays claimed until exit/kill.
                    if past_cap {
                        past_cap = false;
                        slot.past_cap.store(false, Ordering::SeqCst);
                        deadline = Instant::now() + cap;
                    }
                }
                None => break,
            },
            status = child.wait(), if child_exit.is_none() => {
                child_exit = Some(status);
            }
            _ = render.tick(), if !past_cap => {
                spawn_live_patch(
                    Arc::clone(&patch_facts),
                    false,
                    survivors,
                    Arc::clone(&settled),
                    Arc::clone(&card_write),
                );
                shown_survivors = survivors;
            }
            _ = survivor_render.tick(), if past_cap => {
                // The cap stopped the tail, but a process group that outlives
                // `sh` still gets its survivor line: that is state, not output.
                let survivors_now = child_exit.is_some() && group_has_members(pgid);
                if survivors_now != shown_survivors {
                    spawn_live_patch(
                        Arc::clone(&patch_facts),
                        true,
                        survivors_now,
                        Arc::clone(&settled),
                        Arc::clone(&card_write),
                    );
                    shown_survivors = survivors_now;
                }
            }
            () = tokio::time::sleep_until(cap_at.into()), if !past_cap => {
                // The cap stops the live tail, never the wait (ADR-0086
                // decision 6): the card offers 继续 and 终止 and the run keeps
                // waiting for its real exit code.
                past_cap = true;
                slot.past_cap.store(true, Ordering::SeqCst);
                spawn_live_patch(
                    Arc::clone(&patch_facts),
                    true,
                    survivors,
                    Arc::clone(&settled),
                    Arc::clone(&card_write),
                );
                shown_survivors = survivors;
            }
        }
    }

    // Nothing may observe the log as a runaway after the run itself ended; the
    // probe's own contract is what makes a fired probe's settle wait for the
    // whole group to be gone.
    finished.store(true, Ordering::SeqCst);
    let _ = stop_probe.send(());
    let _ = probe.await;
    let output_over_limit = over_limit.load(Ordering::SeqCst);

    slot.live.store(false, Ordering::SeqCst);
    slot.past_cap.store(false, Ordering::SeqCst);
    let elapsed = started.elapsed();
    let (exit_code, spawn_error) = match child_exit {
        Some(Ok(status)) => (status.code(), spawn_error),
        Some(Err(error)) => (None, Some(error.to_string())),
        // The loop only breaks once the group is empty; a still-unwaited child
        // is reaped here so the real exit code is never lost.
        None => (
            child.wait().await.ok().and_then(|status| status.code()),
            spawn_error,
        ),
    };
    let survivors = group_has_members(pgid);
    settle_run(
        platform,
        &card_id,
        &cwd,
        &spec.command,
        &log.path,
        thread_key,
        &settled,
        &card_write,
        TerminalOutcome {
            exit_code,
            spawn_error: spawn_error.as_deref(),
            elapsed,
            stopped: stop_by_user && !output_over_limit,
            survivors,
            output_over_limit,
        },
    )
    .await;
    // Release before the notice: the command has ended, so a new one may start
    // while the notice is in flight. The notice is this run's own and captures
    // its request identity (ADR-0086 decision 12).
    claim.finish();
    announce_completion(handles, platform, request, elapsed, stop_by_user, survivors).await;
    Ok(())
}

/// Whether the child's process group still has members (ADR-0086 decision 6).
/// The run stays live while this is true, even after `sh` itself exited.
fn group_has_members(pgid: Option<u32>) -> bool {
    #[cfg(unix)]
    {
        group_pid(pgid).is_some_and(group_alive)
    }
    #[cfg(not(unix))]
    {
        let _ = pgid;
        false
    }
}

/// Owns the Chat/Topic slot for exactly as long as the process group can still
/// act. Its rule is the module's invariant: **the slot is released only when the
/// group has no members**. Every exit path — settle, probe kill, 终止, spawn
/// failure, task abort, panic — either sees an empty group (release) or hands
/// the claim to a reaper that keeps it until the group empties.
struct TerminalClaim {
    runs: Arc<Mutex<TerminalRuns>>,
    thread_key: ThreadKey,
    run_id: String,
    /// The group the claim protects, once the child has spawned.
    pgid: Option<u32>,
    /// Set when `finish`/`drop` handed the entry to a reaper: the entry must
    /// stay claimed, so `Drop` must not remove it.
    handed_off: bool,
}

impl TerminalClaim {
    /// The run settled normally. Release now when the group is empty; if a
    /// termination could not confirm that, keep the slot claimed and hand it to
    /// a waiter that releases when the group finally empties — never before.
    fn finish(mut self) {
        if let Some(pgid) = self.pgid
            && group_has_members(Some(pgid))
        {
            tracing::warn!(
                "terminal: process group {pgid} still has members after settle; keeping the chat slot claimed"
            );
            self.handed_off = true;
            spawn_group_reaper(
                Arc::clone(&self.runs),
                self.thread_key.clone(),
                self.run_id.clone(),
                pgid,
                false,
            );
        }
        // `Drop` releases when `handed_off` is false.
    }
}

impl Drop for TerminalClaim {
    fn drop(&mut self) {
        if self.handed_off {
            return;
        }
        // An aborted or panicking run never reaches `finish`: the child would
        // otherwise keep running while its slot is released. Signal it here and
        // keep the slot claimed until the reaper confirms the group is gone.
        if let Some(pgid) = self.pgid
            && group_has_members(Some(pgid))
        {
            tracing::warn!("terminal: run ended without settling; terminating process group {pgid}");
            self.handed_off = true;
            spawn_group_reaper(
                Arc::clone(&self.runs),
                self.thread_key.clone(),
                self.run_id.clone(),
                pgid,
                true,
            );
            return;
        }
        self.runs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .release(&self.thread_key, &self.run_id);
    }
}

/// Keep the slot claimed until `pgid` has no members, then release it.
/// `kill` is for an abort/panic path that never ran the normal termination;
/// a settle that already tried to kill only waits. A group that never empties
/// keeps the slot claimed — the stated invariant — rather than opening the chat
/// to an overlapping command.
fn spawn_group_reaper(
    runs: Arc<Mutex<TerminalRuns>>,
    thread_key: ThreadKey,
    run_id: String,
    pgid: u32,
    kill: bool,
) {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        // No runtime left to reap with: keep the entry rather than release a
        // slot whose group may still be alive.
        tracing::warn!("terminal: no runtime to reap process group {pgid}; keeping the chat slot claimed");
        return;
    };
    handle.spawn(async move {
        if kill {
            terminate_process_group(Some(pgid)).await;
        }
        if group_has_members(Some(pgid)) {
            tracing::warn!(
                "terminal: process group {pgid} survived termination; keeping the chat slot claimed"
            );
        }
        while group_has_members(Some(pgid)) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        runs.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .release(&thread_key, &run_id);
        tracing::info!("terminal: process group {pgid} emptied; released the chat slot");
    });
}

/// What settles the card: the real exit code, a spawn failure, the elapsed
/// time, whether 终止 ended it, whether any group member outlived `sh`, and
/// whether #826's size probe stopped it.
struct TerminalOutcome<'a> {
    exit_code: Option<i32>,
    spawn_error: Option<&'a str>,
    elapsed: Duration,
    stopped: bool,
    survivors: bool,
    output_over_limit: bool,
}

/// The one settling write: the loop has already stopped, and nothing is written
/// after this. A settled run carries the 「📄 完整输出」 payload (ADR-0086
/// decision 7) unless it never started.
async fn settle(
    platform: &dyn Platform,
    card_id: &str,
    cwd: &str,
    command: &str,
    log: &Path,
    thread_key: &ThreadKey,
    outcome: TerminalOutcome<'_>,
) {
    let (output, output_truncated) = read_tail(log, TERMINAL_TAIL_CHARS).unwrap_or_default();
    let log_path = log.to_string_lossy().to_string();
    let full_output = outcome.spawn_error.is_none().then(|| TerminalFullOutput {
        log_path: &log_path,
        chat_id: &thread_key.chat_id,
        thread_id: &thread_key.thread_id,
        exit_code: outcome.exit_code,
        elapsed: outcome.elapsed,
        output_over_limit: outcome.output_over_limit,
    });
    let settled = build_terminal_card(TerminalCard {
        cwd,
        command,
        output: &output,
        output_truncated,
        output_over_limit: outcome.output_over_limit,
        full_output,
        notice: None,
        state: TerminalState::Settled {
            exit_code: outcome.exit_code,
            spawn_error: outcome.spawn_error,
            elapsed: outcome.elapsed,
            stopped: outcome.stopped,
            survivors: outcome.survivors,
        },
        // A settled card carries no run controls; only the full-output button.
        run_id: "",
        chat_id: &thread_key.chat_id,
        thread_id: &thread_key.thread_id,
    });
    if let Err(error) = platform.update_message(card_id, &settled).await {
        tracing::warn!("terminal: settling patch failed: {error}");
    }
}

/// Patch the live card from the log's newest bytes. A failed live write is not
/// fatal: the run continues and the settling write still lands.
#[allow(clippy::too_many_arguments)] // the card's own facts plus its run identity
async fn patch_live(
    platform: &dyn Platform,
    card_id: &str,
    cwd: &str,
    command: &str,
    log: &Path,
    run_id: &str,
    chat_id: &str,
    thread_id: &str,
    past_cap: bool,
    survivors: bool,
) {
    let (output, output_truncated) = read_tail(log, TERMINAL_TAIL_CHARS).unwrap_or_default();
    let card = build_terminal_card(TerminalCard {
        cwd,
        command,
        output: &output,
        output_truncated,
        output_over_limit: false,
        full_output: None,
        notice: None,
        state: TerminalState::Live { past_cap, survivors },
        run_id,
        chat_id,
        thread_id,
    });
    if let Err(error) = platform.update_message(card_id, &card).await {
        tracing::warn!("terminal: live patch failed: {error}");
    }
}

/// The immutable facts one live PATCH needs, shared by every spawned patch.
struct LivePatchFacts {
    platform: Arc<dyn Platform>,
    card_id: String,
    cwd: String,
    command: String,
    log: PathBuf,
    run_id: String,
    chat_id: String,
    thread_id: String,
}

/// Issue one live PATCH on its own task: a stalled card write must never block
/// the loop that owns the cap, 终止 and the settle. The task checks `settled`
/// before it starts and again under the card's write lock, so a PATCH queued
/// behind a slow one cannot resurrect 「运行中」 after the settle.
fn spawn_live_patch(
    facts: Arc<LivePatchFacts>,
    past_cap: bool,
    survivors: bool,
    settled: Arc<AtomicBool>,
    card_write: Arc<tokio::sync::Mutex<()>>,
) {
    tokio::spawn(async move {
        if settled.load(Ordering::SeqCst) {
            return;
        }
        let _guard = card_write.lock().await;
        if settled.load(Ordering::SeqCst) {
            return;
        }
        if tokio::time::timeout(
            Duration::from_millis(TERMINAL_PATCH_TIMEOUT_MS),
            patch_live(
                &*facts.platform,
                &facts.card_id,
                &facts.cwd,
                &facts.command,
                &facts.log,
                &facts.run_id,
                &facts.chat_id,
                &facts.thread_id,
                past_cap,
                survivors,
            ),
        )
        .await
        .is_err()
        {
            tracing::warn!("terminal: live patch timed out");
        }
    });
}

/// The settling write, bounded like every live PATCH (a stalled Feishu request
/// must not leave the card showing 「运行中」 forever). `settled` is set first,
/// and the same write lock is taken, so the settle is the card's LAST write:
/// no live patch can be issued after it, and an already-issued one finishes
/// first. A timed-out settle still releases the claim — a dead card must not
/// wedge the chat.
#[allow(clippy::too_many_arguments)]
async fn settle_run(
    platform: &dyn Platform,
    card_id: &str,
    cwd: &str,
    command: &str,
    log: &Path,
    thread_key: &ThreadKey,
    settled: &AtomicBool,
    card_write: &tokio::sync::Mutex<()>,
    outcome: TerminalOutcome<'_>,
) {
    settled.store(true, Ordering::SeqCst);
    let _guard = card_write.lock().await;
    if tokio::time::timeout(
        Duration::from_millis(TERMINAL_PATCH_TIMEOUT_MS),
        settle(platform, card_id, cwd, command, log, thread_key, outcome),
    )
    .await
    .is_err()
    {
        tracing::warn!("terminal: settling patch timed out");
    }
}

/// The Completion Notice's copy: an unconfirmed termination must not read as a
/// successful one, the same rule the settled card follows.
fn completion_notice_text(stopped: bool, survivors: bool, elapsed: Duration) -> String {
    if stopped && survivors {
        format!(
            "⚠️ 终端命令已发送终止，但进程组可能仍在运行 · 耗时 {}",
            elapsed_label(elapsed)
        )
    } else if stopped {
        format!("🖥️ 终端命令已终止 · 耗时 {}", elapsed_label(elapsed))
    } else {
        format!("🖥️ 终端命令已完成 · 耗时 {}", elapsed_label(elapsed))
    }
}

/// The Completion Notice a long Terminal Command owes (ADR-0086 decision 12):
/// exactly one, only when the run passed the same long-task threshold a
/// **Turn** uses, under the same chat-type opt-in. It has no **Turn** to reuse,
/// so the requester, reply target and card identity were captured at send; the
/// send calls the platform primitive directly.
async fn announce_completion(
    handles: &CommandHandles,
    platform: &dyn Platform,
    request: TerminalRequest<'_>,
    elapsed: Duration,
    stopped: bool,
    survivors: bool,
) {
    let threshold = Duration::from_millis(handles.config.notice_rules().long_task_notice_ms());
    if elapsed < threshold {
        return;
    }
    let opted_in = if request.is_group {
        handles.config.group_completion_notice
    } else {
        handles.config.long_task_notice
    };
    if !opted_in {
        return;
    }
    let Some(requester) = request.requester_open_id else {
        return;
    };
    let name = if request.is_group {
        platform.user_name(requester).await.unwrap_or(None)
    } else {
        None
    };
    let text = completion_notice_text(stopped, survivors, elapsed);
    if let Err(error) = platform
        .reply_completion_notice(request.message_id, requester, name.as_deref(), &text)
        .await
    {
        tracing::warn!("terminal: completion notice failed: {error}");
    }
}

/// Create the run's Terminal Log, spawn the child into it. `Err` means the
/// process could not start (missing cwd, no `sh`, …) — the card then settles
/// with that reason.
#[cfg(unix)]
fn spawn_in(
    log: &TerminalLog,
    cwd: &str,
    command: &str,
    inherited_env: &[(String, String)],
) -> std::io::Result<tokio::process::Child> {
    let stdout = log.writer.try_clone()?;
    let stderr = log.writer.try_clone()?;
    let mut child = tokio::process::Command::new("sh");
    child
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        // Closed stdin: a command that reads it sees EOF immediately instead
        // of waiting on cola's terminal (ADR-0086 §2, the no-terminal rule).
        .stdin(Stdio::null())
        // Both streams go straight into the log (decision 3): no pipe whose
        // read end cola's death would close under the child's next write.
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        // The safe route ADR-0085 established in place of `setsid`'s
        // `pre_exec`: the child leads its own process group, so a terminal
        // hangup does not reach it and 终止 will be able to signal the tree.
        .process_group(0);
    // ADR-0086 §8: the environment is inherited MINUS cola's own credentials.
    // The child runs as the Host and may read anything the Host can, but the
    // server password cola holds is not part of "the Host's shell". A test
    // injects values here first, so the scrub is exercised against a child that
    // would otherwise have inherited them — without mutating the test process's
    // environment.
    for (name, value) in inherited_env {
        child.env(name, value);
    }
    for name in SCRUBBED_ENV {
        child.env_remove(name);
    }
    child.spawn()
}

/// Non-Unix hosts never reach here — [`unsupported_reason`] refuses first — but
/// the module still has to compile there (ADR-0086 decision 13).
#[cfg(not(unix))]
fn spawn_in(
    _log: &TerminalLog,
    _cwd: &str,
    _command: &str,
    _inherited_env: &[(String, String)],
) -> std::io::Result<tokio::process::Child> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        WINDOWS_REFUSAL,
    ))
}

/// The one **Terminal Log** a run writes: `<root>/<run-id>.log`, in a 0700
/// directory, created 0600 (ADR-0086 §7, §12). It is command output, where
/// credentials leak, so the directory is cola's own and the file is private.
struct TerminalLog {
    path: PathBuf,
    #[cfg_attr(not(unix), allow(dead_code))]
    writer: std::fs::File,
}

impl TerminalLog {
    fn create(root: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(root)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Set explicitly: `create_dir_all` under the usual umask makes a
            // 0755 directory, and this one holds command output.
            std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700))?;
        }
        let path = root.join(format!("{}.log", run_id()));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let writer = options.open(&path)?;
        Ok(Self { path, writer })
    }
}

/// The production **Terminal Log** root: `~/.cola/terminal` (glossary). `None`
/// when the Host has no home directory — [`execute`] then refuses rather than
/// inventing a relative path, which could place command output in an unrelated
/// working tree.
fn default_terminal_root() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".cola").join("terminal"))
}

/// A process-unique run id for the log's name: the wall clock plus a counter,
/// so two commands in the same nanosecond cannot collide. The durable **Terminal
/// Run Record** (#827) will carry the same id.
fn run_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    format!(
        "{}-{:09}-{}",
        now.as_secs(),
        now.subsec_nanos(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// The log's current size, or 0 when it cannot be read: a probe that fails to
/// stat has nothing to stop, and a run is never killed on a read error.
fn observed_log_len(path: &Path) -> u64 {
    std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0)
}

/// The size probe's own task (ADR-0086 decision 7): it watches the log on its
/// own short interval and, once the file has passed the **observed** threshold,
/// kills the whole process group and waits for that group to be gone. It does
/// not share a task with the render loop, so a stalled card PATCH cannot blind
/// it; `finished` is set the moment the run ends on its own, so a completed
/// command is never relabelled as a runaway.
async fn run_size_probe(
    path: PathBuf,
    cap: u64,
    pgid: Option<u32>,
    interval: Duration,
    finished: std::sync::Arc<std::sync::atomic::AtomicBool>,
    over_limit: std::sync::Arc<std::sync::atomic::AtomicBool>,
    mut stop: tokio::sync::oneshot::Receiver<()>,
) {
    let mut tick = tokio::time::interval(interval);
    tick.tick().await;
    loop {
        tokio::select! {
            // Checked first: once the run ended there is nothing to stop.
            biased;
            _ = &mut stop => return,
            _ = tick.tick() => {
                if finished.load(Ordering::SeqCst) {
                    return;
                }
                if observed_log_len(&path) <= cap {
                    continue;
                }
                // `>` is the ADR's "passes 29 MB". Whatever the single write
                // that crossed it did is already on disk: this is an observed
                // threshold, not a write limit.
                over_limit.store(true, Ordering::SeqCst);
                terminate_process_group(pgid).await;
                return;
            }
        }
    }
}

/// Stop the run's whole process group and return only once no member remains
/// (ADR-0086 decisions 6 and 7): `SIGTERM` first, then `SIGKILL` after
/// [`KILL_GRACE`], and in both phases wait for the GROUP to empty — not merely
/// for `sh` to exit. A descendant that ignores `SIGTERM` must not keep writing
/// after the shell dies, or the observed threshold means nothing.
#[cfg(unix)]
async fn terminate_process_group(pgid: Option<u32>) {
    let Some(group) = group_pid(pgid) else {
        return;
    };
    signal_group(pgid, rustix::process::Signal::TERM);
    let grace = Instant::now() + KILL_GRACE;
    while group_alive(group) {
        if Instant::now() >= grace {
            signal_group(pgid, rustix::process::Signal::KILL);
            let confirm = Instant::now() + KILL_CONFIRM;
            while group_alive(group) && Instant::now() < confirm {
                tokio::time::sleep(GROUP_POLL).await;
            }
            if group_alive(group) {
                // SIGKILL cannot be ignored; a group still visible here is a
                // member in uninterruptible sleep (or a zombie awaiting its
                // reaper). Nothing can write after SIGKILL, so this is
                // reported, never silently swallowed.
                tracing::warn!("terminal: process group {pgid:?} still has members after SIGKILL");
            }
            return;
        }
        tokio::time::sleep(GROUP_POLL).await;
    }
}

/// Non-Unix hosts refuse before spawning (ADR-0086 decision 13), so the probe
/// never runs there and the module still has to compile.
#[cfg(not(unix))]
async fn terminate_process_group(_pgid: Option<u32>) {}

/// The `Pid` a process group is signalled through. `process_group(0)` makes the
/// child its own group leader, so its pid IS the group id. Pid 0/1 would
/// address cola's own group or init; `child.id()` is never either, but a guard
/// here is cheaper than trusting that forever.
#[cfg(unix)]
fn group_pid(pgid: Option<u32>) -> Option<rustix::process::Pid> {
    let pgid = pgid.filter(|pid| *pid > 1)?;
    let pid = i32::try_from(pgid).ok()?;
    rustix::process::Pid::from_raw(pid)
}

/// Whether any process still belongs to the group (`kill(-pgid, 0)`).
#[cfg(unix)]
fn group_alive(group: rustix::process::Pid) -> bool {
    match rustix::process::test_kill_process_group(group) {
        Ok(()) => true,
        Err(error) => error != rustix::io::Errno::SRCH,
    }
}

/// `kill(-pgid, signal)` through rustix's safe wrapper. A failed signal is
/// logged, never fatal: the run still settles on the wait.
#[cfg(unix)]
fn signal_group(pgid: Option<u32>, signal: rustix::process::Signal) {
    let Some(group) = group_pid(pgid) else {
        return;
    };
    if let Err(error) = rustix::process::kill_process_group(group, signal) {
        tracing::warn!("terminal: signalling process group {pgid:?} failed: {error}");
    }
}

/// The copy 「📄 完整输出」 sends. Never a truncation of the log itself: the
/// child's descriptor offset does not move back (ADR-0086 decision 7), so a
/// log past the platform's cap is sent as a bounded *copy* of its first bytes
/// with a truncation line, and the file on disk is left exactly as the run
/// left it.
pub(crate) struct FullOutput {
    /// The bytes a File Message carries (`<= MAX_FILE_BYTES`).
    pub(crate) bytes: Vec<u8>,
    /// The log's own size when the copy was taken.
    pub(crate) total_size: u64,
    /// Whether the copy omits bytes past the platform's upload cap.
    pub(crate) truncated: bool,
}

/// Read the log for delivery, bounded by Feishu's File Message cap.
pub(crate) fn read_full_output(path: &Path) -> std::io::Result<FullOutput> {
    read_full_output_capped(path, crate::feishu::file::MAX_FILE_BYTES)
}

/// [`read_full_output`] with the cap spelled out: production always uses the
/// platform's 30 MB, while a unit test proves the bounded-copy arithmetic
/// without writing 30 MB.
fn read_full_output_capped(path: &Path, cap: u64) -> std::io::Result<FullOutput> {
    use std::io::Read;

    // Whether the file is longer is decided by the bytes actually present, not
    // by an earlier stat: an orphaned group member may still be writing the
    // log, and a copy must never hand the platform more than its cap.
    let mut file = std::fs::File::open(path)?;
    let mut bytes = Vec::new();
    (&mut file).take(cap).read_to_end(&mut bytes)?;
    let mut extra = [0u8; 1];
    if file.read(&mut extra)? == 0 {
        return Ok(FullOutput {
            total_size: bytes.len() as u64,
            bytes,
            truncated: false,
        });
    }
    // The log is longer than the cap. The truncation line is part of the
    // delivered bytes, so it is charged against the same cap: room is made by
    // trimming the head, never by overrunning the platform's ceiling. At the
    // real 30 MB the line always fits; a degenerate cap smaller than the line
    // (a unit test only) sends the head without it.
    let total_size = std::fs::metadata(path)
        .map(|meta| meta.len())
        .unwrap_or_default()
        .max(bytes.len() as u64);
    let line = truncation_line(total_size, cap, path);
    if (line.len() as u64) <= cap {
        bytes.truncate((cap - line.len() as u64) as usize);
        bytes.extend_from_slice(line.as_bytes());
    }
    Ok(FullOutput {
        bytes,
        total_size,
        truncated: true,
    })
}

/// The last line of a bounded delivery copy: it says the file is not the whole
/// log, its true size, and where the rest lives.
fn truncation_line(total_size: u64, cap: u64, path: &Path) -> String {
    format!(
        "\n…（已截断：日志共 {}，本文件仅含前 {}；完整输出在 {}）\n",
        crate::backend::file_content::human_size(total_size),
        crate::backend::file_content::human_size(cap),
        path.display()
    )
}

/// The newest window of `path`, and whether the log is longer than that window.
/// UTF-8 is at most four bytes per character, so a window of `4 * max_chars`
/// bytes always holds enough for the display tail's own budget; lossy decoding
/// keeps a non-UTF-8 log readable (ADR-0086 §4). The window is returned
/// **raw**: `\r` collapsing, line caps and ANSI stripping are the card's
/// display concerns, so the per-line cap can still see a whole long line.
///
/// The read is **size-bounded**, not `read_to_end` on the file: a live command
/// is still appending, so EOF keeps moving and a continuous producer such as
/// `/run yes` would grow this allocation without limit. Seeking to the last
/// `window` bytes and then taking at most that many is what makes one tick cost
/// a fixed amount of memory no matter how fast the child writes.
pub(crate) fn read_tail(path: &Path, max_chars: usize) -> std::io::Result<(String, bool)> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let window = (max_chars as u64).saturating_mul(4).saturating_add(4);
    let truncated = len > window;
    if truncated {
        // From the END, so the window is the newest bytes at the moment of the
        // seek rather than a start computed from an already-stale length.
        let offset = i64::try_from(window).unwrap_or(i64::MAX);
        file.seek(SeekFrom::End(-offset))?;
    }
    let mut bytes = Vec::with_capacity(window.min(1_048_576) as usize);
    (&mut file).take(window).read_to_end(&mut bytes)?;
    Ok((String::from_utf8_lossy(&bytes).into_owned(), truncated))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::AtomicBool;

    /// A command's log is written while cola reads it. The read must take a
    /// fixed-size suffix, never follow EOF into however much the producer keeps
    /// appending: `read_to_end` on a growing file allocates without bound and a
    /// `/run yes` would exhaust cola's memory. This drives a real append loop
    /// and asserts one read returns — promptly — with at most the window.
    #[test]
    fn a_continuously_growing_log_reads_a_bounded_suffix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("growth.log");
        // Pre-fill past the window so the read definitely seeks and truncates,
        // and the producer starts from a known state.
        std::fs::write(&path, vec![b'y'; 64 * 1024]).unwrap();

        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let writer = {
            let path = path.clone();
            let stop = std::sync::Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut file = std::fs::OpenOptions::new()
                    .append(true)
                    .open(&path)
                    .expect("open the growing log");
                let chunk = [b'y'; 64 * 1024];
                // Bound the writer too, so even the unfixed read cannot run
                // away with the disk while the test waits out its timeout.
                for _ in 0..64 {
                    if stop.load(Ordering::Relaxed) || file.write_all(&chunk).is_err() {
                        break;
                    }
                }
            })
        };

        let max_chars = 2_500;
        let window = (max_chars as u64).saturating_mul(4).saturating_add(4);
        let (sender, receiver) = std::sync::mpsc::channel();
        let reader = {
            let path = path.clone();
            std::thread::spawn(move || {
                sender.send(read_tail(&path, max_chars)).ok();
            })
        };
        let result = receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("read_tail must return on a continuously growing log");
        stop.store(true, Ordering::Relaxed);
        writer.join().unwrap();
        reader.join().unwrap();

        let (output, truncated) = result.expect("read the growing log");
        assert!(truncated, "a log larger than the window is truncated");
        assert!(
            output.len() as u64 <= window,
            "the read stays inside the {window}-byte window, got {}",
            output.len()
        );
    }

    /// The `!` prefix (ADR-0086 §1): the entry point opens only at the FIRST
    /// character, and only when a command remains. `!` alone, `! ` and a
    /// mid-message `!` stay prose; the `!` form takes no flags, so everything
    /// after it — a `#<id>` Skill token included — is the literal command.
    #[test]
    fn bang_opens_only_at_the_first_character_and_needs_a_remainder() {
        assert_eq!(parse_bang("!ls"), Some("ls".to_string()));
        assert_eq!(
            parse_bang("!ls -la /tmp"),
            Some("ls -la /tmp".to_string()),
            "the whole remainder is the command"
        );
        assert_eq!(
            parse_bang("  !ls  "),
            Some("ls".to_string()),
            "leading whitespace is trimmed before the check"
        );
        assert_eq!(
            parse_bang("!ls #caveman"),
            Some("ls #caveman".to_string()),
            "the `!` form accepts no flags and no Skill scan: the token is literal"
        );
        assert_eq!(
            parse_bang("!--timeout 5s echo hi"),
            Some("--timeout 5s echo hi".to_string()),
            "everything after `!` is the command, flags-looking words included"
        );
        assert_eq!(parse_bang("!"), None, "`!` alone is prose");
        assert_eq!(parse_bang("! "), None, "`! ` with only spaces is empty");
        assert_eq!(parse_bang("!   \t "), None, "only spaces means empty");
        assert_eq!(parse_bang("hello !ls"), None, "a mid-message `!` is prose");
        assert_eq!(parse_bang("ls!"), None, "a trailing `!` is prose");
        assert_eq!(parse_bang(""), None);
    }

    /// #825 blocking review: the slot is released only once the process group
    /// has no members. A normal settle that cannot confirm termination hands
    /// the claim to a reaper instead of opening the chat to an overlap.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_claim_is_not_released_while_its_group_is_alive() {
        use std::os::unix::process::CommandExt;

        let mut child = std::process::Command::new("sleep")
            .arg("0.4")
            .process_group(0)
            .spawn()
            .expect("spawn a live group");
        let pgid = child.id();
        let key = ThreadKey::new("chat".into(), "chat".into());
        let (control, _rx) = mpsc::unbounded_channel();
        let run = Arc::new(TerminalRun {
            run_id: "run-1".into(),
            live: AtomicBool::new(true),
            past_cap: AtomicBool::new(false),
            control,
        });
        let runs = Arc::new(Mutex::new(TerminalRuns::default()));
        runs.lock().unwrap().claim(&key, run).expect("the slot is free");
        let claim = TerminalClaim {
            runs: Arc::clone(&runs),
            thread_key: key.clone(),
            run_id: "run-1".into(),
            pgid: Some(pgid),
            handed_off: false,
        };

        claim.finish();
        assert!(
            runs.lock().unwrap().runs.contains_key(&key),
            "a settle with a live group must keep the slot claimed"
        );

        // Reaping the child empties the group; the reaper may now release it.
        child.wait().expect("reap the child");
        for _ in 0..100 {
            if !runs.lock().unwrap().runs.contains_key(&key) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !runs.lock().unwrap().runs.contains_key(&key),
            "the reaper releases the slot once the group is empty"
        );
    }

    /// #825 review: the notice's copy follows the card's rule — an
    /// unconfirmed termination is not announced as a clean one.
    #[test]
    fn an_unconfirmed_termination_notice_says_the_group_may_still_run() {
        let elapsed = Duration::from_secs(601);
        let text = completion_notice_text(true, true, elapsed);
        assert!(text.contains("可能仍在运行"), "{text}");
        let text = completion_notice_text(true, false, elapsed);
        assert!(text.contains("已终止"), "{text}");
        assert!(!text.contains("可能仍在运行"), "{text}");
    }

    /// #826: the delivery copy is bounded at the platform's cap and carries a
    /// truncation line naming the real size and where the rest lives, while the
    /// log itself is left untouched — truncating the log would not move the
    /// child's descriptor offset back (ADR-0086 decision 7). The cap is spelled
    /// out because production's 30 MB is too much to write in a unit test.
    #[test]
    fn the_delivery_copy_is_bounded_and_never_touches_the_log() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.log");
        std::fs::write(&path, vec![b'a'; 5_000]).unwrap();

        let copy = read_full_output_capped(&path, 1_024).unwrap();
        assert!(copy.truncated, "the copy admits it is not the whole log");
        assert_eq!(copy.total_size, 5_000, "the log's real size is reported");
        assert_eq!(copy.bytes.len(), 1_024, "the copy never exceeds the cap");
        let text = String::from_utf8_lossy(&copy.bytes).into_owned();
        assert!(text.starts_with("aaaa"), "the copy starts at the log's head");
        assert!(
            text.contains("已截断") && text.contains(&*path.display().to_string()),
            "the truncation line says what was cut and where the rest lives: {text}"
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            5_000,
            "the Terminal Log is never truncated for delivery"
        );

        // Under the cap the whole log goes out, with no truncation line.
        let small = dir.path().join("small.log");
        std::fs::write(&small, b"short log\n").unwrap();
        let copy = read_full_output_capped(&small, 1_024).unwrap();
        assert!(!copy.truncated);
        assert_eq!(copy.bytes, b"short log\n");
        assert_eq!(copy.total_size, 10);
    }
}
