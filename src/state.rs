//! The single owner of cola's on-disk layout (ADR-0090 decision 6).
//!
//! Two roots make up the layout, and they are deliberately distinct:
//!
//! - **The state directory** is `[bridge] session_file`'s own directory. Every
//!   file keyed by a Session — the session mapping, the Chain Record, the
//!   interactive surfaces, the pin set, the Terminal Run Records — lives beside
//!   the mapping, so a configured `session_file` outside `~/.cola` takes its
//!   sidecars with it.
//! - **The home state directory** is `~/.cola`, where the files that belong to
//!   the process rather than to a Session live: the log, the singleton lock,
//!   the Owned Server's pid record, the Shared Service's log, the restart
//!   notice, and the Terminal Log root. `dirs::home_dir`'s absence keeps the
//!   historical `./.cola` fallback for those paths, but [`StateLayout::terminal_root`]
//!   stays `None`: command output must never land in an unrelated tree.
//!
//! Nothing outside this module may derive a state path with `with_file_name`
//! or by joining `.cola` by hand. Build the layout once (`StateLayout::from_config`,
//! or [`Default`] for the config-less CLI paths) and thread it to whoever needs
//! a path.

use std::path::{Path, PathBuf};

use crate::config::Config;

/// The single owner of cola's on-disk paths: the Session-keyed sidecars beside
/// `[bridge] session_file` plus the process-owned files under the home state
/// directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StateLayout {
    /// The configured session mapping (`[bridge] session_file`).
    session_file: PathBuf,
    /// Its directory — every Session-keyed sidecar lives here.
    state_dir: PathBuf,
    /// `~/.cola`, or `None` when the Host has no home directory.
    home_state_dir: Option<PathBuf>,
}

impl Default for StateLayout {
    /// The layout a default install gets: `~/.cola/sessions.json` plus the
    /// ambient home. Used by the config-less CLI paths (`cola logs`,
    /// `cola stop`, the config defaults), which cannot read `[bridge]`.
    fn default() -> Self {
        Self::with_home(dirs::home_dir())
    }
}

impl StateLayout {
    /// Build a layout from an explicit session file and home directory. The
    /// home is `None` on a Host that has none; the session-file directory is
    /// used verbatim, so a relative mapping keeps its relative sidecars.
    pub(crate) fn new(session_file: PathBuf, home: Option<PathBuf>) -> Self {
        let state_dir = session_file.parent().map(Path::to_path_buf).unwrap_or_default();
        let home_state_dir = home.map(|home| home.join(".cola"));
        Self {
            session_file,
            state_dir,
            home_state_dir,
        }
    }

    /// The default layout for a known home: `~/.cola/sessions.json`. The
    /// config lookup uses it for `~/.cola/cola.toml`; tests use it to pin the
    /// whole layout against a fixed home.
    pub(crate) fn for_home(home: &Path) -> Self {
        Self::new(home.join(".cola").join("sessions.json"), Some(home.to_path_buf()))
    }

    /// The configured layout: `[bridge] session_file`'s directory plus the
    /// ambient home. This is the one construction the daemon performs.
    pub(crate) fn from_config(cfg: &Config) -> Self {
        Self::new(cfg.bridge.session_file.clone(), dirs::home_dir())
    }

    fn with_home(home: Option<PathBuf>) -> Self {
        let state_dir = home.clone().unwrap_or_else(|| PathBuf::from(".")).join(".cola");
        Self::new(state_dir.join("sessions.json"), home)
    }

    /// The configured session mapping, unchanged.
    pub(crate) fn session_file(&self) -> &Path {
        &self.session_file
    }

    // ---- Session-keyed sidecars: beside `[bridge] session_file` ----

    /// The durable Chain Record (ADR-0069).
    pub(crate) fn chain_records(&self) -> PathBuf {
        self.state_dir.join("chain_records.json")
    }

    /// The interactive surfaces a previous process persisted (ADR-0038).
    pub(crate) fn interactive_surfaces(&self) -> PathBuf {
        self.state_dir.join("interactive_surfaces.json")
    }

    /// The persisted Instant Reminder pin set (#249).
    pub(crate) fn pinned_chats(&self) -> PathBuf {
        self.state_dir.join("pinned_chats.json")
    }

    /// The Terminal Run Record directory (ADR-0086 decision 5).
    pub(crate) fn terminal_runs(&self) -> PathBuf {
        self.state_dir.join("terminal-runs")
    }

    // ---- Process-owned files: under `~/.cola` ----

    /// The default append log (`~/.cola/cola.log`); `--log-file` overrides it.
    pub(crate) fn log(&self) -> PathBuf {
        self.home_state_dir_or_cwd().join("cola.log")
    }

    /// The singleton lock (`~/.cola/cola.lock`).
    pub(crate) fn lock(&self) -> PathBuf {
        self.home_state_dir_or_cwd().join("cola.lock")
    }

    /// The pid of the OpenCode server cola itself spawned.
    pub(crate) fn self_spawned_pid(&self) -> PathBuf {
        self.home_state_dir_or_cwd().join("self-opencode.pid")
    }

    /// The detached Shared Service child's stdout/stderr.
    pub(crate) fn service_log(&self) -> PathBuf {
        self.home_state_dir_or_cwd().join("opencode-service.log")
    }

    /// The announcement `/restart` and `/update` leave for the successor.
    pub(crate) fn restart_notify(&self) -> PathBuf {
        self.home_state_dir_or_cwd().join("restart-notify.json")
    }

    /// The default Access List (`~/.cola/access.json`), used when `[bridge]
    /// access_file` is not set.
    pub(crate) fn access_file(&self) -> PathBuf {
        self.home_state_dir_or_cwd().join("access.json")
    }

    /// The config lookup's `~/.cola/cola.toml` fallback.
    pub(crate) fn config_file(&self) -> PathBuf {
        self.home_state_dir_or_cwd().join("cola.toml")
    }

    /// The production **Terminal Log** root, `~/.cola/terminal` (ADR-0086 §7).
    /// `None` without a home: the caller refuses rather than inventing a
    /// relative path that could place command output in an unrelated tree.
    pub(crate) fn terminal_root(&self) -> Option<PathBuf> {
        self.home_state_dir
            .as_ref()
            .map(|state_dir| state_dir.join("terminal"))
    }

    // ---- Naming helpers for paths received from elsewhere ----

    /// Test-only: place a sidecar beside a path the caller already owns, such
    /// as a fixture's access file. This is **not** a second derivation route —
    /// production paths come from the layout's named methods; constructing a
    /// layout here would invent a home for a path the fixture already holds.
    #[cfg(test)]
    pub(crate) fn beside(path: &Path, name: &str) -> PathBuf {
        path.with_file_name(name)
    }

    /// The pre-ADR-0069 sidecars beside a Chain Record file: `live_cards.json`
    /// and `wake_watermarks.json`. A test fixture may hand `ChainRecords` its
    /// own chain path, so the naming lives here rather than in the store.
    pub(crate) fn legacy_chain_sidecars(chain_records: &Path) -> (PathBuf, PathBuf) {
        (
            chain_records.with_file_name("live_cards.json"),
            chain_records.with_file_name("wake_watermarks.json"),
        )
    }

    /// The owed-settlement directory beside a Terminal Run Record directory.
    pub(crate) fn terminal_owed_beside(runs: &Path) -> PathBuf {
        let name = runs
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "terminal-runs".to_string());
        runs.with_file_name(format!("{name}-owed"))
    }

    /// A rotated daily log beside its base (`cola-YYYY-MM-DD.log`). The base is
    /// whatever `--log-file` resolved, so this is the one naming rule the
    /// layout owns for a path it did not derive itself.
    pub(crate) fn rotated_log_path(base: &Path, day: &str) -> PathBuf {
        let stem = base
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| "cola".to_string());
        base.with_file_name(format!("{stem}-{day}.log"))
    }

    fn home_state_dir_or_cwd(&self) -> PathBuf {
        self.home_state_dir
            .clone()
            .unwrap_or_else(|| PathBuf::from(".").join(".cola"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A future path change must be a visible change here, not a silent
    /// migration: every path is pinned against a fixed home and session file.
    #[test]
    fn every_path_is_pinned_against_a_fixed_home_and_session_file() {
        let layout = StateLayout::for_home(Path::new("/home/u"));

        assert_eq!(layout.session_file(), Path::new("/home/u/.cola/sessions.json"));
        assert_eq!(
            layout.chain_records(),
            Path::new("/home/u/.cola/chain_records.json")
        );
        assert_eq!(
            layout.interactive_surfaces(),
            Path::new("/home/u/.cola/interactive_surfaces.json")
        );
        assert_eq!(
            layout.pinned_chats(),
            Path::new("/home/u/.cola/pinned_chats.json")
        );
        assert_eq!(layout.terminal_runs(), Path::new("/home/u/.cola/terminal-runs"));
        assert_eq!(
            StateLayout::terminal_owed_beside(&layout.terminal_runs()),
            Path::new("/home/u/.cola/terminal-runs-owed")
        );
        assert_eq!(
            StateLayout::legacy_chain_sidecars(&layout.chain_records()).0,
            Path::new("/home/u/.cola/live_cards.json")
        );
        assert_eq!(
            StateLayout::legacy_chain_sidecars(&layout.chain_records()).1,
            Path::new("/home/u/.cola/wake_watermarks.json")
        );

        assert_eq!(layout.log(), Path::new("/home/u/.cola/cola.log"));
        assert_eq!(layout.lock(), Path::new("/home/u/.cola/cola.lock"));
        assert_eq!(
            layout.self_spawned_pid(),
            Path::new("/home/u/.cola/self-opencode.pid")
        );
        assert_eq!(
            layout.service_log(),
            Path::new("/home/u/.cola/opencode-service.log")
        );
        assert_eq!(
            layout.restart_notify(),
            Path::new("/home/u/.cola/restart-notify.json")
        );
        assert_eq!(layout.access_file(), Path::new("/home/u/.cola/access.json"));
        assert_eq!(layout.config_file(), Path::new("/home/u/.cola/cola.toml"));
        assert_eq!(
            layout.terminal_root().as_deref(),
            Some(Path::new("/home/u/.cola/terminal"))
        );
    }

    /// A `session_file` outside `~/.cola` takes every Session-keyed sidecar
    /// with it; the process-owned files stay in the home state directory.
    #[test]
    fn a_session_file_outside_the_home_places_every_sidecar_beside_it() {
        let layout = StateLayout::new(
            PathBuf::from("/srv/cola/sessions.json"),
            Some(PathBuf::from("/home/u")),
        );

        assert_eq!(layout.chain_records(), Path::new("/srv/cola/chain_records.json"));
        assert_eq!(
            layout.interactive_surfaces(),
            Path::new("/srv/cola/interactive_surfaces.json")
        );
        assert_eq!(layout.pinned_chats(), Path::new("/srv/cola/pinned_chats.json"));
        assert_eq!(layout.terminal_runs(), Path::new("/srv/cola/terminal-runs"));
        assert_eq!(
            StateLayout::terminal_owed_beside(&layout.terminal_runs()),
            Path::new("/srv/cola/terminal-runs-owed")
        );
        assert_eq!(
            StateLayout::legacy_chain_sidecars(&layout.chain_records()).0,
            Path::new("/srv/cola/live_cards.json")
        );
        assert_eq!(
            StateLayout::legacy_chain_sidecars(&layout.chain_records()).1,
            Path::new("/srv/cola/wake_watermarks.json")
        );

        // Process-owned paths do not follow the session mapping.
        assert_eq!(layout.log(), Path::new("/home/u/.cola/cola.log"));
        assert_eq!(layout.lock(), Path::new("/home/u/.cola/cola.lock"));
        assert_eq!(
            layout.self_spawned_pid(),
            Path::new("/home/u/.cola/self-opencode.pid")
        );
        assert_eq!(
            layout.service_log(),
            Path::new("/home/u/.cola/opencode-service.log")
        );
        assert_eq!(
            layout.terminal_root().as_deref(),
            Some(Path::new("/home/u/.cola/terminal"))
        );
    }

    /// Without a home the historical `./.cola` fallback stays for the paths that
    /// always had it, and the Terminal Log root stays absent.
    #[test]
    fn without_a_home_the_relative_fallback_holds_but_the_terminal_root_refuses() {
        let layout = StateLayout::new(PathBuf::from("sessions.json"), None);

        assert_eq!(layout.session_file(), Path::new("sessions.json"));
        assert_eq!(layout.chain_records(), Path::new("chain_records.json"));
        assert_eq!(layout.log(), Path::new("./.cola/cola.log"));
        assert_eq!(layout.lock(), Path::new("./.cola/cola.lock"));
        assert_eq!(layout.access_file(), Path::new("./.cola/access.json"));
        assert_eq!(layout.terminal_root(), None);
    }

    /// A relative mapping keeps relative sidecars (the old `with_file_name`
    /// behaviour), so a launcher without a notion of a state directory behaves
    /// exactly as before.
    #[test]
    fn a_relative_session_file_keeps_its_sidecars_relative() {
        let layout = StateLayout::for_home(Path::new("/home/u"));
        assert_eq!(
            layout.chain_records(),
            Path::new("/home/u/.cola/chain_records.json")
        );

        let relative = StateLayout::new(PathBuf::from("sessions.json"), None);
        assert_eq!(relative.terminal_runs(), Path::new("terminal-runs"));
        assert_eq!(
            StateLayout::terminal_owed_beside(&relative.terminal_runs()),
            Path::new("terminal-runs-owed")
        );
    }

    #[test]
    fn the_rotated_log_keeps_the_base_stem_beside_it() {
        assert_eq!(
            StateLayout::rotated_log_path(Path::new("/var/log/cola.log"), "2026-10-23"),
            Path::new("/var/log/cola-2026-10-23.log")
        );
        assert_eq!(
            StateLayout::rotated_log_path(Path::new("cola.log"), "2026-10-23"),
            Path::new("cola-2026-10-23.log")
        );
    }
}
