//! The `/dir` picker's clicks: the Recent Directories card and its filesystem
//! browse sub-card (ADR-0025, ADR-0051, ADR-0078; ticket #1122).
//!
//! One registry entry, because the browse card multiplexes through the SAME
//! `action: "dir"` tag as the Recent Directories card, distinguished by
//! `view: "browse"` (ADR-0078) — the body keeps that split exactly as the
//! dispatch did. Recent Directories ops: `pick`, `topic` (+ `search`, `page`,
//! `browse`, `recent`); browse ops: `go`, `filter`, `restore`, `page`,
//! `show_hidden`, `pick`, `topic` (+ `recent` swapping back).
//!
//! `pick`/`topic` act on the directory you stand in and declare a Pending
//! Session (ADR-0041) or open a topic around one (ADR-0016); every pending
//! declaration and refresh rides the click's ONE `deadline` (ADR-0089), which
//! the dispatch derived — this module never mints one. The browse base path is
//! unchanged: a typed `path` wins over the payload's `dir`, and the current
//! project is the fallback when a name-only form submit lost both. Lazy-start
//! rules and the pitfall `card-inputs` layout are the card builders', untouched
//! here.

use std::sync::Arc;

use tokio::time::Instant;

use crate::bridge::card_action::VerbFuture;
use crate::bridge::core::SharedCore;
use crate::bridge::handler::{
    App, CARD_STORE_TIMEOUT, CardActionResult, build_dir_browse_card_for, card_before, declare_pending_dir,
    parse_show_hidden, reject_nested_topic, resolve_browse_dir, thread_key_from_value,
};
use crate::config::ThreadKey;
use crate::feishu::card::session::BrowseView;

/// The `/dir` card's callback: the Recent Directories ops, or — when the
/// payload says `view: "browse"` — the filesystem browse card's ops. Both ride
/// the click's one `deadline`.
pub(crate) fn run<'a>(
    app: &'a Arc<App>,
    value: &'a serde_json::Value,
    deadline: Instant,
    _callback_started: Instant,
) -> VerbFuture<'a> {
    Box::pin(async move { app.handle_dir_card_action(&app.core, value, deadline).await })
}

impl App {
    /// Rebuild the `/dir` Recent Directories card for a thread, narrowed by
    /// `keyword` and paged to `page` (ADR-0051, ADR-0052).
    async fn build_dir_card_for(
        self: &Arc<Self>,
        core: &Arc<SharedCore>,
        thread_key: &ThreadKey,
        keyword: &str,
        page: usize,
    ) -> serde_json::Value {
        let (dirs, current_dir) =
            crate::feishu::card::command::dir_card_data(&core.command_handles(), thread_key, keyword).await;
        crate::feishu::card::session::build_dir_card(thread_key, &dirs, current_dir.as_deref(), keyword, page)
    }

    /// Handle a `/dir` Recent Directories card button (ADR-0025): `op:
    /// "pick"` re-roots the thread into the picked directory by declaring a
    /// Pending Session there (the card form of `/dir <path>`, ADR-0041 — the
    /// first message materialises it); `op: "topic"` wraps a NEW session in a
    /// brand-new Feishu topic instead — the card equivalent of `/topic <dir>`.
    /// Clicking the current directory's `pick` is a no-op that just toasts.
    /// `op: "search"` rebuilds the card narrowed by the typed keyword at page
    /// 1 (ADR-0051); `op: "page"` flips to the payload's page. Every rebuild
    /// keeps the active keyword and page (ADR-0052) — a row action no longer
    /// resets the filter. Refreshes the card in place so the pending's
    /// directory shows as `当前`.
    async fn handle_dir_card_action(
        self: &Arc<Self>,
        core: &Arc<SharedCore>,
        value: &serde_json::Value,
        deadline: tokio::time::Instant,
    ) -> Option<CardActionResult> {
        // ADR-0078: the browse card multiplexes through the same `dir` action,
        // distinguished by `view: "browse"`, so one dispatch arm serves both.
        if value.get("view").and_then(|v| v.as_str()) == Some("browse") {
            return self.handle_dir_browse_action(core, value, deadline).await;
        }
        let op = value.get("op").and_then(|v| v.as_str()).unwrap_or("");
        let thread_key = thread_key_from_value(value);
        // The card's stateless filter (ADR-0052): the active keyword and page
        // ride every button. A missing/garbage page reads as 1; the builder
        // clamps an out-of-range page to the last page.
        let keyword = value
            .get("keyword")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let page = value.get("page").and_then(|v| v.as_u64()).unwrap_or(1).max(1) as usize;
        // The search submit and a page flip carry no directory, so they are
        // handled before the directory guard below. A search always lands on
        // page 1; a flip rebuilds the payload's page.
        if op == "search" {
            return Some(CardActionResult {
                card: Some(self.build_dir_card_for(core, &thread_key, &keyword, 1).await),
                toast: None,
            });
        }
        if op == "page" {
            return Some(CardActionResult {
                card: Some(self.build_dir_card_for(core, &thread_key, &keyword, page).await),
                toast: None,
            });
        }
        // ADR-0078: the Recent Directories card's 浏览目录 button opens the
        // filesystem browse card in place.
        if op == "browse" {
            return Some(CardActionResult {
                card: Some(self.build_dir_browse_for_start(core, &thread_key).await),
                toast: None,
            });
        }
        // ADR-0078: ← 最近目录 on the browse card swaps back in place.
        if op == "recent" {
            return Some(CardActionResult {
                card: Some(self.build_dir_card_for(core, &thread_key, "", 1).await),
                toast: None,
            });
        }
        let directory = value
            .get("directory")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if directory.is_empty() {
            return Some(CardActionResult {
                card: None,
                toast: Some("无效操作".to_string()),
            });
        }
        match op {
            "pick" => {
                // Pending-first (ADR-0041): 当前 is the pending's directory
                // when one exists, else the active session's. The equality rule
                // is the browse card's own ✅ 当前 rule (`command::same_dir`), so
                // the two pick surfaces agree on what "current" means.
                let current_dir = tokio::time::timeout_at(deadline, core.sessions.lock())
                    .await
                    .ok()
                    .and_then(|store| store.current_directory(&thread_key));
                if current_dir.is_some_and(|current| crate::bridge::command::same_dir(&current, &directory)) {
                    return Some(CardActionResult {
                        card: card_before(
                            deadline,
                            self.build_dir_card_for(core, &thread_key, &keyword, page),
                        )
                        .await,
                        toast: Some("已在当前目录".to_string()),
                    });
                }
                Some(
                    match declare_pending_dir(core, &thread_key, &directory, deadline).await {
                        Some(toast) => CardActionResult {
                            card: card_before(
                                deadline,
                                self.build_dir_card_for(core, &thread_key, &keyword, page),
                            )
                            .await,
                            toast: Some(toast),
                        },
                        None => CardActionResult {
                            card: None,
                            toast: Some(CARD_STORE_TIMEOUT.to_string()),
                        },
                    },
                )
            }
            "topic" => {
                // A topic can only be created from a non-topic message
                // (ADR-0006, ADR-0025): a `/dir` card opened inside a
                // never-bound topic — which may legally bind its single
                // session via `pick` — must not nest another topic. The guard
                // is location-based, so it also covers a topic that bound
                // itself via the row's left button on this same card. The
                // topic anchors on the card's own message (`open_message_id`,
                // threaded by `extract_card_action_value`), so fallback cards
                // can reply inside it (ADR-0016).
                let open_message_id = value.get("open_message_id").and_then(|v| v.as_str());
                match open_topic_from_card(core, &thread_key, open_message_id, directory).await {
                    Ok(display) => Some(CardActionResult {
                        card: Some(self.build_dir_card_for(core, &thread_key, &keyword, page).await),
                        toast: Some(format!("已建话题（{display}）——下一条消息创建会话")),
                    }),
                    Err(rejection) => Some(rejection),
                }
            }
            _ => Some(CardActionResult {
                card: None,
                toast: Some("无效操作".to_string()),
            }),
        }
    }

    /// Open the `/dir` browse card (ADR-0078) at the conversation's current
    /// project directory.
    async fn build_dir_browse_for_start(
        self: &Arc<Self>,
        core: &Arc<SharedCore>,
        thread_key: &ThreadKey,
    ) -> serde_json::Value {
        let start = core.command_handles().current_project_directory(thread_key).await;
        self.build_dir_browse_for(
            core,
            thread_key,
            BrowseView {
                dir: &start,
                show_hidden: false,
                keyword: "",
                page: 1,
            },
        )
        .await
    }

    /// Rebuild the `/dir` browse card (ADR-0078) for `view.dir`, clamping a
    /// vanished path to its nearest existing ancestor and marking the
    /// conversation's current directory.
    pub(crate) async fn build_dir_browse_for(
        self: &Arc<Self>,
        core: &Arc<SharedCore>,
        thread_key: &ThreadKey,
        view: BrowseView<'_>,
    ) -> serde_json::Value {
        let sessions = core.command_handles().flow.sessions;
        build_dir_browse_card_for(&sessions, thread_key, view).await
    }

    /// Handle a `/dir` browse-card click (ADR-0078). `go` drills down (a child
    /// row, a breadcrumb hop, or the form's typed path); `pick`/`topic` act on
    /// the directory you stand in (or the typed path), mirroring the Recent
    /// Directories card's two ops; `filter`/`page`/`show_hidden` rebuild the
    /// view; `restore` rebuilds it untouched (a viewer's 「← 返回目录」);
    /// `recent` swaps back to the Recent Directories card. A typed path
    /// wins over the payload's `dir`, and the current project is the fallback
    /// when a name-only form submit lost the dir.
    async fn handle_dir_browse_action(
        self: &Arc<Self>,
        core: &Arc<SharedCore>,
        value: &serde_json::Value,
        deadline: tokio::time::Instant,
    ) -> Option<CardActionResult> {
        let op = value.get("op").and_then(|v| v.as_str()).unwrap_or("");
        let thread_key = thread_key_from_value(value);
        let show_hidden = parse_show_hidden(value);
        let keyword = value
            .get("keyword")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let page = value.get("page").and_then(|v| v.as_u64()).unwrap_or(1).max(1) as usize;
        let dir = value
            .get("dir")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let path = value
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        // The typed path wins over the payload's dir on 前往/选择; a name-only
        // submit of 过滤 lost both, so it falls back to the current project.
        let target = if !path.is_empty() {
            path
        } else if !dir.is_empty() {
            dir
        } else {
            core.command_handles()
                .current_project_directory(&thread_key)
                .await
        };
        let base = BrowseView {
            dir: &target,
            show_hidden,
            keyword: &keyword,
            page,
        };
        match op {
            "go" => Some(CardActionResult {
                card: Some(
                    self.build_dir_browse_for(
                        core,
                        &thread_key,
                        BrowseView {
                            keyword: "",
                            page: 1,
                            ..base
                        },
                    )
                    .await,
                ),
                toast: None,
            }),
            "filter" => Some(CardActionResult {
                card: Some(
                    self.build_dir_browse_for(core, &thread_key, BrowseView { page: 1, ..base })
                        .await,
                ),
                toast: None,
            }),
            // ADR-0083 §3: a File Viewer's 「← 返回目录」 restores the listing
            // it was opened from — a pure rebuild of the payload's own state
            // (directory, filter, hidden toggle, page), so `base` is used
            // verbatim. `go` cannot serve it: a drill-down deliberately clears
            // the filter and returns to page 1.
            "restore" => Some(CardActionResult {
                card: Some(self.build_dir_browse_for(core, &thread_key, base).await),
                toast: None,
            }),
            "page" => Some(CardActionResult {
                card: Some(self.build_dir_browse_for(core, &thread_key, base).await),
                toast: None,
            }),
            "show_hidden" => Some(CardActionResult {
                card: Some(
                    self.build_dir_browse_for(
                        core,
                        &thread_key,
                        BrowseView {
                            show_hidden: !base.show_hidden,
                            page: 1,
                            ..base
                        },
                    )
                    .await,
                ),
                toast: None,
            }),
            "pick" => self.browse_pick(core, &thread_key, base, deadline).await,
            "topic" => {
                let Some(directory) = resolve_browse_dir(&target) else {
                    return Some(CardActionResult {
                        card: None,
                        toast: Some(format!("目录不存在：{target}")),
                    });
                };
                let open_message_id = value.get("open_message_id").and_then(|v| v.as_str());
                match open_topic_from_card(core, &thread_key, open_message_id, directory.clone()).await {
                    Ok(display) => Some(CardActionResult {
                        card: Some(
                            self.build_dir_browse_for(
                                core,
                                &thread_key,
                                BrowseView {
                                    dir: &directory,
                                    ..base
                                },
                            )
                            .await,
                        ),
                        toast: Some(format!("已建话题（{display}）——下一条消息创建会话")),
                    }),
                    Err(rejection) => Some(rejection),
                }
            }
            _ => Some(CardActionResult {
                card: None,
                toast: Some("无效操作".to_string()),
            }),
        }
    }

    /// The browse card's 选择: declare a Pending Session in `view.dir` (Lazy
    /// Session Creation, ADR-0041) — the card form of `/dir <path>`. A target
    /// that is not a directory is refused with a toast instead.
    async fn browse_pick(
        self: &Arc<Self>,
        core: &Arc<SharedCore>,
        thread_key: &ThreadKey,
        view: BrowseView<'_>,
        deadline: tokio::time::Instant,
    ) -> Option<CardActionResult> {
        // The whole click shares ONE deadline (ADR-0089), started by
        // `handle_card_action`: the topic guard's read, the current-directory
        // read, the pending write and the refresh below all ride it, so a click
        // can never commit a pending after Feishu has stopped listening.
        // ADR-0007: a bound topic owns exactly one session. `/dir` and the
        // `/dir` card are refused there at command entry
        // (`Command::topic_rejection`), but `/cat <directory>` legitimately
        // renders the same browse card inside a topic (a read surface, never
        // gated — spec #669 story 41). Its 选择/切换到这里 must therefore
        // refuse the same way here, or a click would supersede the topic's
        // session through the pending declaration below.
        if thread_key.thread_id != thread_key.chat_id {
            match tokio::time::timeout_at(deadline, core.sessions.lock()).await {
                Ok(store) if store.get_active(thread_key).is_some() => {
                    return Some(CardActionResult {
                        card: None,
                        toast: Some(crate::bridge::command::TOPIC_SELECTION_REJECTION.to_string()),
                    });
                }
                // A guard that cannot answer in time also refuses: the topic's
                // one-session rule must not be bypassed by a slow store.
                Err(_) => {
                    return Some(CardActionResult {
                        card: None,
                        toast: Some(CARD_STORE_TIMEOUT.to_string()),
                    });
                }
                Ok(_) => {}
            }
        }
        // Resolving the target is filesystem work (`canonicalize` + `is_dir`),
        // and a synchronous call blocks the executor thread where no deadline
        // can reach it — so it runs on the blocking pool, bounded by the click's
        // deadline. Past that, the click refuses like any other overrun.
        let target = view.dir.to_string();
        let resolution = tokio::time::timeout_at(
            deadline,
            tokio::task::spawn_blocking(move || resolve_browse_dir(&target)),
        )
        .await;
        let directory = match resolution {
            Ok(Ok(Some(directory))) => directory,
            // The scan itself failed (a join error): report the path as
            // unusable, exactly as an absent directory reads.
            Ok(Err(_)) | Ok(Ok(None)) => {
                return Some(CardActionResult {
                    card: None,
                    toast: Some(format!("目录不存在：{}", view.dir)),
                });
            }
            Err(_) => {
                return Some(CardActionResult {
                    card: None,
                    toast: Some(CARD_STORE_TIMEOUT.to_string()),
                });
            }
        };
        // Pending-first (ADR-0041): 当前 is the pending's directory when one
        // exists, else the active session's. The comparison is the SAME one the
        // card's ✅ 当前 mark uses (`command::same_dir`), so a directory the card
        // marks current is never re-declared by a click.
        let current_dir = tokio::time::timeout_at(deadline, core.sessions.lock())
            .await
            .ok()
            .and_then(|store| store.current_directory(thread_key));
        let settled = BrowseView {
            dir: &directory,
            ..view
        };
        if current_dir.is_some_and(|current| crate::bridge::command::same_dir(&current, &directory)) {
            return Some(CardActionResult {
                card: card_before(deadline, self.build_dir_browse_for(core, thread_key, settled)).await,
                toast: Some("已在当前目录".to_string()),
            });
        }
        Some(
            match declare_pending_dir(core, thread_key, &directory, deadline).await {
                Some(toast) => CardActionResult {
                    card: card_before(deadline, self.build_dir_browse_for(core, thread_key, settled)).await,
                    toast: Some(toast),
                },
                None => CardActionResult {
                    card: None,
                    toast: Some(CARD_STORE_TIMEOUT.to_string()),
                },
            },
        )
    }
}

/// Open a NEW topic for `directory`, anchored on the card's own message
/// (`open_message_id`) — the shared body of the `/dir` and browse cards' 建话题
/// (ADR-0016, ADR-0025). `Ok(display)` carries the directory basename for the
/// success toast; `Err(result)` is the exact refusal to return as-is.
async fn open_topic_from_card(
    core: &Arc<SharedCore>,
    thread_key: &ThreadKey,
    open_message_id: Option<&str>,
    directory: String,
) -> std::result::Result<String, CardActionResult> {
    if let Some(rejection) = reject_nested_topic(thread_key) {
        return Err(rejection);
    }
    let Some(open_message_id) = open_message_id else {
        return Err(CardActionResult {
            card: None,
            toast: Some("无法创建话题（缺少卡片消息引用）".to_string()),
        });
    };
    // Creation title policy (ADR-0007): unnamed, like `/topic <dir>`; the cover
    // shows the directory basename until the server auto-generates a title.
    let display = crate::bridge::display::dir_basename(&directory);
    match crate::bridge::topic::open_topic(
        &core.topic_handles(),
        &thread_key.chat_id,
        open_message_id,
        crate::bridge::topic::TopicOpening::Fresh {
            directory,
            name: None,
            worktree: None,
        },
    )
    .await
    {
        Ok(_opened) => Ok(display),
        Err(crate::bridge::topic::OpenTopicError::NoThreadId) => Err(CardActionResult {
            card: None,
            toast: Some(
                "当前会话不支持创建话题（未返回 thread_id）。请改用 `/dir <目录>` 或在飞书里手动创建话题。"
                    .to_string(),
            ),
        }),
        Err(crate::bridge::topic::OpenTopicError::Failed(e)) => {
            tracing::warn!("dir card topic: create topic failed: {}", e);
            Err(CardActionResult {
                card: None,
                toast: Some("创建话题失败".to_string()),
            })
        }
    }
}
