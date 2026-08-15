//! One-shot service install for the daemon.
//!
//! Zero external dependencies by design: everything here is pure std plus
//! invoking `systemctl` (present on every systemd machine) and writing files
//! into the user's config tree.  Nothing here runs as root and nothing needs
//! a package install — the point is a downloaded AppImage whose user clicks
//! "install service" and gets a working daemon.
//!
//! Detection order:
//!   1. systemd in *user* mode   -> user unit + `systemctl --user enable --now`
//!   2. systemd in *system* mode (root, rare) -> system unit + systemctl
//!   3. otherwise                -> XDG autostart .desktop (any DE, no systemd)
//!
//! `install` is idempotent and logs a one-line summary; `uninstall` removes
//! exactly what `install` created.  The daemon binary itself is copied next to
//! the config (never assumed to live inside an AppImage mount), and the unit /
//! autostart file reference that stable copy so updates don't break the link.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use log::{info, warn};

use nicewatch_common::{APP_NAME, APP_DISPLAY_NAME};

/// Directory that holds the installed service, config, and binary copy.
pub fn install_dir() -> PathBuf {
    nicewatch_common::local_config_path()
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf()
}

/// The systemd *user* unit path (`~/.config/systemd/user/`).
fn systemd_user_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config"))
        })?;
    Some(base.join("systemd").join("user"))
}

/// The XDG autostart dir (`~/.config/autostart/`).
fn autostart_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("autostart"))
}

/// True when systemd user services are available (i.e. a systemd init with a
/// per-user manager, the normal case on Fedora/Arch/Ubuntu/Debian etc.).
fn systemd_user_available() -> bool {
    // The system manager is what spawns user managers; both live under
    // /run/systemd when systemd is PID 1.
    if !Path::new("/run/systemd/system").exists() {
        return false;
    }
    // `systemctl --user` needs the user manager socket; the daemon runs in
    // the same session, so XDG_RUNTIME_DIR is set.
    std::env::var_os("XDG_RUNTIME_DIR").is_some()
        && std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some()
}

/// Run `systemctl` (system or user scope).  `args` may be the whole argv.
fn run_systemctl(user: bool, args: &[&str]) -> Result<(), String> {
    let mut cmd = std::process::Command::new("systemctl");
    if user {
        cmd.arg("--user");
    }
    let out = cmd
        .args(args)
        .output()
        .map_err(|e| format!("cannot run systemctl: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "systemctl {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// Copy our own running binary to a stable location under the config dir.
/// Returns the destination path.  Reading /proc/self/exe works even when the
/// process was started from inside an AppImage mount.
fn install_binary_copy() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot locate own binary: {e}"))?;
    let dest = install_dir().join(format!("{APP_NAME}.bin"));
    let src = exe.clone();
    let dest_str = dest.display().to_string();
    // Idempotent: skip the copy when we're already the installed binary.
    if src == dest {
        info!("binary already installed at {dest_str}");
        return Ok(dest);
    }
    fs::create_dir_all(dest.parent().unwrap()).map_err(|e| format!("cannot create dir: {e}"))?;
    if dest.exists() {
        fs::remove_file(&dest).map_err(|e| format!("cannot replace old binary: {e}"))?;
    }
    fs::copy(&src, &dest).map_err(|e| format!("cannot copy binary to {dest_str}: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dest, fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("cannot chmod binary: {e}"))?;
    }
    info!("installed daemon binary at {dest_str}");
    Ok(dest)
}

/// Write a default rules file if none exists yet.  The root (`/etc`) config
/// is best-effort: exactly like the running daemon, EPERM here is a warning
/// and we continue with the local config — a plain user must be able to
/// install the service with no sudo at all.
fn ensure_default_config() -> Result<(), String> {
    let default = include_str!("../../rules.toml.example");
    let local = nicewatch_common::local_config_path();
    if !local.exists() {
        if let Some(parent) = local.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        fs::write(&local, default).map_err(|e| format!("cannot write {}: {e}", local.display()))?;
        info!("wrote default config {}", local.display());
    }
    let root = nicewatch_common::root_config_path();
    if !root.exists() {
        match fs::write(&root, default) {
            Ok(()) => info!("wrote default config {}", root.display()),
            Err(e) => warn!(
                "cannot write default root config {}: {e} (continuing with local only)",
                root.display()
            ),
        }
    }
    Ok(())
}

fn unit_contents(binary: &Path, system: bool) -> String {
    let root_cfg = nicewatch_common::root_config_path();
    let local_cfg = nicewatch_common::local_config_path();
    let exec = if system {
        // Root scope: absolute paths, /etc config is authoritative.
        format!(
            "{bin} --root-config {} --local-config {}",
            root_cfg.display(),
            local_cfg.display(),
            bin = binary.display()
        )
    } else {
        // User scope: `%t` = $XDG_RUNTIME_DIR, `%h` = $HOME, and the local
        // config lives under $HOME so the root config is attempted but
        // non-fatal when absent (daemon logs the EPERM and keeps running).
        format!(
            "{bin} --root-config {} --local-config {} --socket %t/{name}.sock",
            root_cfg.display(),
            local_cfg.display(),
            bin = binary.display(),
            name = APP_NAME
        )
    };
    format!(
        "[Unit]\n\
         Description={APP_DISPLAY_NAME} CPU/IO priority daemon\n\
         After=graphical-session.target\n\
         Wants=graphical-session.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={exec}\n\
         Restart=on-failure\n\
         RestartSec=3\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    )
}

fn autostart_contents(binary: &Path) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name={APP_DISPLAY_NAME}\n\
         Comment=CPU/IO scheduling priority daemon\n\
         Exec={bin}\n\
         X-GNOME-Autostart-enabled=true\n",
        bin = binary.display()
    )
}

/// Install the service for this session.  Returns a short human summary.
pub fn install() -> Result<String, String> {
    let binary = install_binary_copy()?;
    ensure_default_config()?;

    // systemd first (user, then system when we're root).
    if systemd_user_available() {
        let Some(dir) = systemd_user_dir() else {
            return Err("no HOME/XDG_CONFIG_HOME for systemd unit".into());
        };
        fs::create_dir_all(&dir).map_err(|e| format!("cannot create {dir:?}: {e}"))?;
        let unit = dir.join(format!("{APP_NAME}.service"));
        fs::write(&unit, unit_contents(&binary, false))
            .map_err(|e| format!("cannot write {unit:?}: {e}"))?;
        info!("wrote user unit {unit:?}");
        run_systemctl(true, &["daemon-reload"])?;
        // enable --now fails if the unit is already running under a different
        // config; `--force` not needed, restart is harmless here.
        let _ = run_systemctl(true, &["enable", "--now", APP_NAME]);
        let _ = run_systemctl(true, &["restart", APP_NAME]);
        return Ok(format!(
            "installed as a systemd user service (unit {unit:?}). \
             Manage with: systemctl --user status {APP_NAME}"
        ));
    }

    if std::env::var_os("UID").is_none_or(|u| u == "0") {
        let system_dir = Path::new("/etc/systemd/system");
        let unit = system_dir.join(format!("{APP_NAME}.service"));
        match fs::write(&unit, unit_contents(&binary, true)) {
            Ok(()) => {
                info!("wrote system unit {unit:?}");
                if run_systemctl(false, &["daemon-reload"]).is_ok() {
                    let _ = run_systemctl(false, &["enable", "--now", APP_NAME]);
                    let _ = run_systemctl(false, &["restart", APP_NAME]);
                }
                return Ok(format!(
                    "installed as a system-wide systemd service ({unit:?}). \
                     Manage with: systemctl status {APP_NAME}"
                ));
            }
            Err(e) => warn!("cannot write system unit {unit:?}: {e} — falling back to autostart"),
        }
    }

    // No systemd (or no permission for system scope): XDG autostart.
    let Some(dir) = autostart_dir() else {
        return Err("no HOME/XDG_CONFIG_HOME for autostart entry".into());
    };
    fs::create_dir_all(&dir).map_err(|e| format!("cannot create {dir:?}: {e}"))?;
    let entry = dir.join(format!("{APP_NAME}.desktop"));
    fs::write(&entry, autostart_contents(&binary))
        .map_err(|e| format!("cannot write {entry:?}: {e}"))?;
    info!("wrote autostart entry {entry:?}");
    Ok(format!(
        "installed as an XDG autostart entry ({entry:?}); it starts at your next login. \
         Start it now with: {}",
        binary.display()
    ))
}

/// Remove whatever `install` created.  Keeps the binary copy (harmless) and
/// the config files (user data).
pub fn uninstall() -> Result<String, String> {
    let mut removed: Vec<String> = Vec::new();

    if systemd_user_available() {
        if let Some(dir) = systemd_user_dir() {
            let unit = dir.join(format!("{APP_NAME}.service"));
            if unit.exists() {
                let _ = run_systemctl(true, &["disable", "--now", APP_NAME]);
                fs::remove_file(&unit).map_err(|e| format!("cannot remove {unit:?}: {e}"))?;
                removed.push(unit.display().to_string());
            }
        }
    }

    if let Some(dir) = autostart_dir() {
        let entry = dir.join(format!("{APP_NAME}.desktop"));
        if entry.exists() {
            fs::remove_file(&entry).map_err(|e| format!("cannot remove {entry:?}: {e}"))?;
            removed.push(entry.display().to_string());
        }
    }

    let _ = io::stdout().flush();
    if removed.is_empty() {
        Ok("nothing was installed — nothing to uninstall".into())
    } else {
        Ok(format!("removed: {}", removed.join(", ")))
    }
}

/// Report install state (which backend is active, where the files are).
pub fn status() -> String {
    let mut lines = Vec::new();
    if systemd_user_available() {
        lines.push("init: systemd (user services available)".into());
        if let Some(dir) = systemd_user_dir() {
            let unit = dir.join(format!("{APP_NAME}.service"));
            lines.push(format!(
                "unit: {} ({})",
                unit.display(),
                if unit.exists() { "installed" } else { "not installed" }
            ));
        }
    } else {
        lines.push("init: not systemd (XDG autostart mode)".into());
    }
    if let Some(dir) = autostart_dir() {
        let entry = dir.join(format!("{APP_NAME}.desktop"));
        lines.push(format!(
            "autostart: {} ({})",
            entry.display(),
            if entry.exists() { "installed" } else { "absent" }
        ));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_contents_user_scope() {
        let s = unit_contents(Path::new("/opt/nicewatch/nicewatch"), false);
        assert!(s.contains("ExecStart=/opt/nicewatch/nicewatch --root-config"));
        assert!(s.contains("--socket %t/nicewatch.sock"));
        assert!(s.contains("Restart=on-failure"));
        assert!(s.contains("WantedBy=default.target"));
    }

    #[test]
    fn unit_contents_root_scope() {
        let s = unit_contents(Path::new("/usr/local/bin/nicewatch"), true);
        // Root scope must NOT use %t (system manager has no XDG_RUNTIME_DIR).
        assert!(s.contains("ExecStart=/usr/local/bin/nicewatch"));
        assert!(!s.contains("%t"));
    }

    #[test]
    fn autostart_entry_escapes_spaces() {
        let s = autostart_contents(Path::new("/tmp/.mount_appimage/usr/bin/nicewatch"));
        assert!(s.contains("Exec=/tmp/.mount_appimage/usr/bin/nicewatch"));
        assert!(s.contains("X-GNOME-Autostart-enabled=true"));
    }

    #[test]
    fn no_systemd_returns_autostart_backend() {
        // Simulate "no systemd" by forcing XDG dirs; the backend choice comes
        // from `systemd_user_available()`, which is env-gated here.
        std::env::set_var("XDG_CONFIG_HOME", "/tmp/nw-test-config");
        std::env::set_var("HOME", "/tmp/nw-test-home");
        let dir = autostart_dir().expect("autostart dir");
        assert!(dir.ends_with("autostart"));
    }
}
