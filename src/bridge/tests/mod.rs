//! Integration tests, split by feature out of the `test_support` harness
//! (ticket 05). The harness itself (MockBackend / RecordingPlatform /
//! build_app / incoming / seed_session / ...) lives in
//! [`crate::bridge::test_support`].

pub(crate) mod access;
pub(crate) mod card_handles;
pub(crate) mod config_commands;
pub(crate) mod dir;
pub(crate) mod external;
pub(crate) mod leftovers;
pub(crate) mod misc;
pub(crate) mod pending;
pub(crate) mod permission;
pub(crate) mod pin;
pub(crate) mod prompt_render;
pub(crate) mod question;
pub(crate) mod session_routing;
pub(crate) mod snapshot_follow;
pub(crate) mod subtask;
pub(crate) mod supplement;
pub(crate) mod switch;
pub(crate) mod topic;
