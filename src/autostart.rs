//! Boot-time autostart registration: `cola autostart enable|disable|status`.
//!
//! cola is a long-lived daemon; `autostart` registers the OS launcher that
//! starts it at boot/login. The launcher runs the `cola` binary itself (Lazy
//! Start handles server attachment/spawning — there is no separate `serve`
//! subcommand), so the ExecStart/ProgramArguments/registry value is just the
//! resolved executable path. Logs and config resolve from `~/.cola`, so the
//! launcher needs no flags.
//!
//! Per platform (ADR-0013):
//! - Linux: a systemd **user** unit (`~/.config/systemd/user/cola.service`),
//!   enabled with `systemctl --user`, plus a `loginctl enable-linger` hint so
//!   the service runs without a logged-in desktop session. The installer's PATH
//!   is snapshotted into the unit because systemd's default PATH omits
//!   `~/.cargo/bin`, `~/.bun/bin`, etc. where `opencode` may live.
//! - macOS: a LaunchAgent (`~/Library/LaunchAgents/com.cola.bot.plist`).
//! - Windows: an `HKCU\...\Run` registry value pointing at the cola binary.

use anyhow::Context;

/// The subcommands of `cola autostart`.
#[derive(Debug, Clone, Copy, clap::Subcommand)]
pub enum AutostartAction {
    /// Register cola to start at boot/login.
    Enable,
    /// Remove the boot-time registration.
    Disable,
    /// Show whether cola is registered to start at boot.
    Status,
}

pub fn run(action: AutostartAction) -> anyhow::Result<()> {
    match action {
        AutostartAction::Enable => enable(),
        AutostartAction::Disable => disable(),
        AutostartAction::Status => status(),
    }
}

/// The command that restarts a running cola through its OS supervisor, when one
/// is registered for this platform (systemd user unit / launchd agent).
/// `None` when there is no supervisor (Windows' `Run` key has no restart
/// facility) or none is installed. Used by the self-update CLI (ADR-0015) so
/// the restarted daemon stays supervised instead of dying with the terminal.
pub(crate) fn supervisor_restart_command() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        linux::supervisor_restart_command()
    }
    #[cfg(target_os = "macos")]
    {
        macos::supervisor_restart_command()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

/// The executable path a registered supervisor would start cola from — the
/// systemd unit's `ExecStart` / the launchd agent's `ProgramArguments`.
/// `None` when no supervisor is installed (or its binary cannot be parsed).
/// The self-update flow compares this against the binary it just replaced so an
/// ExecStart path mismatch surfaces as a warning instead of a silent stale
/// daemon after the supervisor restart.
pub(crate) fn supervisor_binary_path() -> Option<std::path::PathBuf> {
    #[cfg(target_os = "linux")]
    {
        linux::supervisor_binary_path()
    }
    #[cfg(target_os = "macos")]
    {
        macos::supervisor_binary_path()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

/// The absolute path of the running cola binary — what the launcher starts.
fn exe_path() -> anyhow::Result<String> {
    let exe = std::env::current_exe().context("cannot resolve own executable path")?;
    let s = exe.to_string_lossy().into_owned();
    anyhow::ensure!(!s.trim().is_empty(), "empty executable path");
    Ok(s)
}

/// Read a supervisor config file and map its executable entry to a path
/// (systemd `ExecStart=` / launchd `ProgramArguments`), shared by the
/// platform-specific parsers.
fn parse_supervisor_binary(
    path: &std::path::Path,
    parse: fn(&str) -> Option<String>,
) -> Option<std::path::PathBuf> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|content| parse(&content).map(std::path::PathBuf::from))
}

// ---------------------------------------------------------------------------
// Linux: systemd user unit
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::path::PathBuf;

    const UNIT_NAME: &str = "cola.service";

    fn unit_path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".config")
            .join("systemd")
            .join("user")
            .join(UNIT_NAME)
    }

    /// systemd ExecStart accepts a path unquoted; quote only when it contains
    /// spaces so a relocated binary still parses.
    fn systemd_quote(path: &str) -> String {
        if path.contains(char::is_whitespace) {
            format!("\"{}\"", path)
        } else {
            path.to_string()
        }
    }

    fn unit_content(exe: &str, path_snapshot: &str) -> String {
        format!(
            "[Unit]\n\
             Description=cola bridge bot (OpenCode to Feishu)\n\
             After=network.target\n\n\
             [Service]\n\
             Type=simple\n\
             ExecStart={}\n\
             Restart=on-failure\n\
             RestartSec=5\n\
             Environment=PATH={}\n\n\
             [Install]\n\
             WantedBy=default.target\n",
            systemd_quote(exe),
            path_snapshot
        )
    }

    pub(super) fn enable() -> anyhow::Result<()> {
        let exe = exe_path()?;
        let path = unit_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).context("create systemd user dir")?;
        }
        let path_snapshot = std::env::var("PATH").unwrap_or_default();
        std::fs::write(&path, unit_content(&exe, &path_snapshot))
            .with_context(|| format!("write {}", path.display()))?;
        run_ok("systemctl", &["--user", "daemon-reload"], "daemon-reload")?;
        run_ok("systemctl", &["--user", "enable", UNIT_NAME], "enable")?;
        println!(
            "cola autostart enabled: {} (ExecStart={})\n\
             提示: 无头环境（无桌面登录）请运行 `loginctl enable-linger $USER` 让用户服务开机即起。",
            path.display(),
            exe
        );
        Ok(())
    }

    pub(super) fn disable() -> anyhow::Result<()> {
        let path = unit_path();
        let _ = std::process::Command::new("systemctl")
            .args(["--user", "disable", UNIT_NAME])
            .status();
        let _ = std::fs::remove_file(&path);
        if !path.exists() {
            println!("cola autostart disabled: {}", path.display());
        } else {
            anyhow::bail!("failed to remove {}", path.display());
        }
        Ok(())
    }

    pub(super) fn status() -> anyhow::Result<()> {
        let path = unit_path();
        if !path.exists() {
            println!("cola autostart: not installed ({} absent)", path.display());
            return Ok(());
        }
        println!("cola autostart: installed at {}", path.display());
        let out = std::process::Command::new("systemctl")
            .args(["--user", "is-enabled", UNIT_NAME])
            .output();
        match out {
            Ok(o) if o.status.success() => {
                println!(
                    "  systemd: enabled ({})",
                    String::from_utf8_lossy(&o.stdout).trim()
                );
            }
            _ => println!(
                "  systemd: could not query `systemctl --user is-enabled` (no systemd user session?)"
            ),
        }
        Ok(())
    }

    fn run_ok(prog: &str, args: &[&str], what: &str) -> anyhow::Result<()> {
        let status = std::process::Command::new(prog)
            .args(args)
            .status()
            .with_context(|| format!("cannot run `{prog}` — is systemd user session available? ({what})"))?;
        anyhow::ensure!(status.success(), "`{prog} {what}` failed with {}", status);
        Ok(())
    }

    /// The command that restarts a running cola via systemd, when the user unit
    /// is installed (`cola autostart enable` wrote it).
    pub(super) fn supervisor_restart_command() -> Option<String> {
        unit_path()
            .exists()
            .then(|| format!("systemctl --user restart {UNIT_NAME}"))
    }

    /// Parse the executable from a systemd unit's `ExecStart=` line. The
    /// generated unit quotes paths with spaces (`systemd_quote`); an unquoted
    /// value may carry arguments, so only the first word is taken.
    fn parse_exec_start(content: &str) -> Option<String> {
        let line = content
            .lines()
            .find_map(|l| l.trim().strip_prefix("ExecStart="))?;
        let trimmed = line.trim();
        if let Some(q) = trimmed.strip_prefix('"') {
            q.strip_suffix('"').map(str::to_string)
        } else {
            Some(trimmed.split_whitespace().next()?.to_string())
        }
    }

    /// The binary path the installed systemd unit would start.
    pub(super) fn supervisor_binary_path() -> Option<std::path::PathBuf> {
        parse_supervisor_binary(&unit_path(), parse_exec_start)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn sample_unit(exe: &str) -> String {
            format!(
                "[Unit]\n\
                 Description=cola bridge bot (OpenCode to Feishu)\n\n\
                 [Service]\n\
                 Type=simple\n\
                 ExecStart={}\n\
                 Restart=on-failure\n",
                exe
            )
        }

        #[test]
        fn parse_exec_start_plain_path() {
            assert_eq!(
                parse_exec_start(&sample_unit("/home/u/.local/bin/cola")).as_deref(),
                Some("/home/u/.local/bin/cola")
            );
        }

        #[test]
        fn parse_exec_start_quoted_path_with_spaces() {
            assert_eq!(
                parse_exec_start(&sample_unit("\"/home/u/my apps/cola\"")).as_deref(),
                Some("/home/u/my apps/cola")
            );
        }

        #[test]
        fn parse_exec_start_takes_first_word_of_args() {
            assert_eq!(
                parse_exec_start(&sample_unit("/usr/bin/cola --config /etc/cola.toml")).as_deref(),
                Some("/usr/bin/cola")
            );
        }

        #[test]
        fn parse_exec_start_missing_line_is_none() {
            assert_eq!(parse_exec_start("[Unit]\nDescription=no exec\n"), None);
        }
    }
}

// ---------------------------------------------------------------------------
// macOS: LaunchAgent
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use std::path::PathBuf;

    const LABEL: &str = "com.cola.bot";
    const PLIST: &str = "com.cola.bot.plist";

    fn plist_path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("Library")
            .join("LaunchAgents")
            .join(PLIST)
    }

    fn plist_content(exe: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>ProcessType</key>
    <string>Interactive</string>
</dict>
</plist>
"#
        )
    }

    fn uid() -> String {
        std::process::Command::new("id")
            .arg("-u")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    }

    pub(super) fn enable() -> anyhow::Result<()> {
        let exe = exe_path()?;
        let path = plist_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).context("create LaunchAgents dir")?;
        }
        std::fs::write(&path, plist_content(&exe)).with_context(|| format!("write {}", path.display()))?;
        let target = format!("gui/{}", uid());
        let plist = path.to_string_lossy().into_owned();
        let status = std::process::Command::new("launchctl")
            .arg("bootstrap")
            .arg(&target)
            .arg(&plist)
            .status();
        match status {
            Ok(s) if s.success() => println!("cola autostart enabled: {}", path.display()),
            Ok(_) => println!(
                "cola autostart: written {}; `launchctl bootstrap` failed — try `launchctl load {}`",
                path.display(),
                path.display()
            ),
            Err(e) => anyhow::bail!("cannot run launchctl: {e}"),
        }
        Ok(())
    }

    pub(super) fn disable() -> anyhow::Result<()> {
        let path = plist_path();
        let target = format!("gui/{}/{}", uid(), LABEL);
        let _ = std::process::Command::new("launchctl")
            .arg("bootout")
            .arg(&target)
            .status();
        let _ = std::fs::remove_file(&path);
        if !path.exists() {
            println!("cola autostart disabled: {}", path.display());
        } else {
            anyhow::bail!("failed to remove {}", path.display());
        }
        Ok(())
    }

    pub(super) fn status() -> anyhow::Result<()> {
        let path = plist_path();
        if !path.exists() {
            println!("cola autostart: not installed ({} absent)", path.display());
            return Ok(());
        }
        println!("cola autostart: installed at {}", path.display());
        let target = format!("gui/{}/{}", uid(), LABEL);
        let out = std::process::Command::new("launchctl")
            .arg("print")
            .arg(&target)
            .output();
        match out {
            Ok(o) if o.status.success() => println!("  launchd: loaded"),
            _ => println!("  launchd: not loaded — run `cola autostart enable`"),
        }
        Ok(())
    }

    /// The command that restarts a running cola via launchd, when the agent is
    /// installed (`cola autostart enable` wrote the plist).
    pub(super) fn supervisor_restart_command() -> Option<String> {
        plist_path()
            .exists()
            .then(|| format!("launchctl kickstart -k gui/{}/{}", uid(), LABEL))
    }

    /// Parse the first `ProgramArguments` string (the executable) from the
    /// generated plist XML. The plist is machine-written by `plist_content`,
    /// so a light scan for the block after the `<key>` is enough.
    fn parse_program_arguments(content: &str) -> Option<String> {
        let start = content.find("<key>ProgramArguments</key>")?;
        let rest = &content[start..];
        let open = rest.find("<string>")? + "<string>".len();
        let close = rest[open..].find("</string>")? + open;
        Some(rest[open..close].to_string())
    }

    /// The binary path the installed launchd agent would start.
    pub(super) fn supervisor_binary_path() -> Option<std::path::PathBuf> {
        parse_supervisor_binary(&plist_path(), parse_program_arguments)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn parse_program_arguments_returns_executable() {
            let plist = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.cola.bot</string>
    <key>ProgramArguments</key>
    <array>
        <string>/usr/local/bin/cola</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
</dict>
</plist>
"#;
            assert_eq!(
                parse_program_arguments(plist).as_deref(),
                Some("/usr/local/bin/cola")
            );
        }

        #[test]
        fn parse_program_arguments_missing_key_is_none() {
            assert_eq!(parse_program_arguments("<plist><dict></dict></plist>"), None);
        }
    }
}

// ---------------------------------------------------------------------------
// Windows: HKCU\...\Run registry value
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
mod windows {
    use super::*;

    const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
    const VALUE: &str = "cola";

    pub(super) fn enable() -> anyhow::Result<()> {
        let exe = exe_path()?;
        // Registry REG_SZ data for a path with spaces is double-quoted.
        let data = format!("\"{exe}\"");
        let status = std::process::Command::new("reg")
            .arg("add")
            .arg(RUN_KEY)
            .arg("/v")
            .arg(VALUE)
            .arg("/t")
            .arg("REG_SZ")
            .arg("/d")
            .arg(&data)
            .arg("/f")
            .status()
            .with_context(|| "cannot run `reg` (Windows registry tool)")?;
        anyhow::ensure!(status.success(), "`reg add` failed with {}", status);
        println!("cola autostart enabled: {VALUE} at {RUN_KEY}");
        Ok(())
    }

    pub(super) fn disable() -> anyhow::Result<()> {
        let _ = std::process::Command::new("reg")
            .args(["delete", RUN_KEY, "/v", VALUE, "/f"])
            .status();
        println!("cola autostart disabled: {VALUE} at {RUN_KEY}");
        Ok(())
    }

    pub(super) fn status() -> anyhow::Result<()> {
        let out = std::process::Command::new("reg")
            .args(["query", RUN_KEY, "/v", VALUE])
            .output();
        match out {
            Ok(o) if o.status.success() => {
                println!("cola autostart: enabled at {RUN_KEY}");
                println!("  {}", String::from_utf8_lossy(&o.stdout).trim());
            }
            _ => println!("cola autostart: not installed ({VALUE} absent from {RUN_KEY})"),
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
use linux as platform;
#[cfg(target_os = "macos")]
use macos as platform;
#[cfg(target_os = "windows")]
use windows as platform;

fn enable() -> anyhow::Result<()> {
    platform::enable()
}

fn disable() -> anyhow::Result<()> {
    platform::disable()
}

fn status() -> anyhow::Result<()> {
    platform::status()
}
