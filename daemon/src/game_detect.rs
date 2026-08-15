//! Game-detection heuristic.
//!
//! A process "looks like a game" when it holds an open file descriptor
//! under `/dev/dri/*` **and** its environment block (from
//! `/proc/<pid>/environ`) contains Steam variables (`STEAM_COMPAT_DATA_PATH`,
//! `SteamAppId`, `SteamGameId`).
//!
//! DRM usage is always required: Steam env alone also matches launcher
//! scaffolding (Proton wrapper scripts, the Steam-runtime `reaper`,
//! pressure-vessel children) — the actual game process is the renderer, the
//! only one holding the `/dev/dri` fd.  Wine-internal helpers (processes
//! running from a prefix's `drive_c/` or with `\windows\system32\` argv)
//! are never games even though they inherit the Steam environment of the
//! game they serve.
//!
//! Any explicit rule (including a `software` tier rule) takes precedence
//! over these flags — see `rules.rs`.
//!
//! NOTE: there is deliberately no compositor interaction here (no KWin
//! D-Bus, no X11 round-trips).  Calling the compositor from the daemon is
//! what caused the stuck-cursor bug, and it must never come back.

use crate::proc_scan::ProcEntry;

const STEAM_ENV_PREFIXES: [&str; 3] = [
    "STEAM_COMPAT_DATA_PATH=",
    "SteamAppId=",
    "SteamGameId=",
];

/// External fullscreen-state provider.  Checked once per poll cycle via
/// [`FullscreenDetector::refresh`], then queried per process.
pub trait FullscreenDetector: Send + Sync {
    /// Called once per poll before the process scan; stateful backends
    /// (D-Bus round trips) refresh their cached picture here.
    fn refresh(&self);
    /// Is the active fullscreen window owned by this process?
    fn is_fullscreen(&self, entry: &ProcEntry) -> bool;
}

/// Best-effort, non-blocking default: we never block polling on a slow X
/// server round-trip, so this trait always returns without doing I/O.
/// The daemon always runs this — see the module note on why there is no
/// KWin backend anymore.
#[allow(dead_code)]
#[derive(Default)]
pub struct NoopFullscreenDetector;

impl FullscreenDetector for NoopFullscreenDetector {
    fn refresh(&self) {}
    fn is_fullscreen(&self, _entry: &ProcEntry) -> bool {
        false
    }
}

/// Desktop-environment/compositor-critical processes that must never be
/// treated as games, no matter how they look (the compositor holds `/dev/dri`
/// fds and its window can be fullscreen — e.g. Plasma's overview).  Exact
/// comm match, or prefix match for entries ending in `*`.
const DE_CRITICAL: &[&str] = &[
    "kwin_wayland",
    "kwin_x11",
    "kwin_wayland_wrapper",
    "plasmashell",
    "kded6",
    "kded5",
    "kglobalaccel",
    "ksmserver",
    "kscreenlocker",
    "krunner",
    "Xwayland",
    "xdg-desktop-portal",
    "xdg-desktop-portal-kde",
    "org_kde_*",
];

pub fn is_de_critical(name: &str) -> bool {
    DE_CRITICAL.iter().any(|pat| match pat.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => name == *pat,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GameFlags {
    pub steam_env: bool,
    pub dri_fd: bool,
    pub fullscreen: bool,
    /// Name matched the embedded known-games catalog (comm/exe/cmdline/
    /// flatpak cgroup scope).  Set by the caller; the heuristic functions
    /// here leave it false.
    pub known: bool,
}

impl GameFlags {
    /// A process is a game when it is on the known-games list, or when it
    /// actually renders via DRM: Steam env alone also matches launcher
    /// scaffolding (Proton wrapper scripts, the Steam-runtime `reaper`,
    /// pressure-vessel children) and fullscreen alone is no signal at all —
    /// the client of the fullscreen window is always the renderer, which
    /// holds the DRM fd.
    pub fn is_game(&self) -> bool {
        self.known || (self.dri_fd && (self.steam_env || self.fullscreen))
    }
}

/// Is this process a Wine-internal helper rather than a game proper?
///
/// Wine helpers (`services.exe`, `explorer.exe`, `winedevice.exe`, ...) run
/// from inside a prefix and inherit the Steam environment of the game they
/// serve, so the Steam-env heuristic alone would flag the whole tree.  The
/// game itself never runs from `drive_c/` and never has a `\windows\system32\`
/// argv — under Proton the `exe` link resolves to `wine64-preloader`, so the
/// cmdline (which names the real module) is the reliable signal.
pub fn is_wine_internal(entry: &ProcEntry) -> bool {
    if entry
        .exe
        .as_deref()
        .map(|p| p.contains("/drive_c/"))
        .unwrap_or(false)
    {
        return true;
    }
    let Some(cmdline) = entry.cmdline.as_deref() else {
        return false;
    };
    let lower = cmdline.to_ascii_lowercase();
    lower.windows(18).any(|w| w == b"\\windows\\system32\\")
        || lower.windows(15).any(|w| w == b"pressure-vessel")
}

/// Compute the heuristic flags for one process.  Pure function over the scan
/// data, so it is trivially testable with fixture trees.
pub fn detect_game(entry: &ProcEntry, fullscreen: &dyn FullscreenDetector) -> GameFlags {
    if is_de_critical(&entry.name) || is_wine_internal(entry) {
        return GameFlags {
            steam_env: false,
            dri_fd: false,
            fullscreen: false,
            known: false,
        };
    }
    GameFlags {
        steam_env: environ_has_steam(entry.environ.as_deref()),
        dri_fd: entry.has_dri_fd,
        fullscreen: fullscreen.is_fullscreen(entry),
        known: false,
    }
}

/// Environment is NUL-delimited `KEY=value` pairs.  Vulkan/Steam both lower
/// the value side, but the keys are exactly as listed.
fn environ_has_steam(environ: Option<&[u8]>) -> bool {
    let Some(env) = environ else { return false };
    for entry in env.split(|&b| b == 0) {
        for prefix in STEAM_ENV_PREFIXES {
            if entry.starts_with(prefix.as_bytes()) {
                return true;
            }
        }
    }
    false
}

/// Is this `/proc/<pid>/comm` one of our own binaries?  The GUI renders via
/// WebKit/GL and therefore holds a `/dev/dri` fd; without this guard the
/// heuristic would flag it (and the daemon itself) as a game and throw up a
/// confirmation dialog asking about our own software.
pub fn is_self_binary(name: &str) -> bool {
    nicewatch_common::SELF_BINARY_NAMES.iter().any(|n| *n == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn steam_appid_from_resource(resource: &str) -> Option<&str> {
        resource
            .strip_prefix("steam_app_")
            .filter(|id| !id.is_empty())
    }

    fn environ_has_steam_appid(environ: Option<&[u8]>, appid: &str) -> bool {
        let Some(env) = environ else {
            return false;
        };
        let want = format!("SteamAppId={appid}");
        env.split(|&b| b == 0)
            .any(|e| e.starts_with(want.as_bytes()))
    }

    fn entry(pid: u32, env: Option<&[u8]>, dri: bool) -> ProcEntry {
        ProcEntry {
            pid,
            ppid: 1,
            name: "x".into(),
            state: 'S',
            utime: 0,
            stime: 0,
            nice: 0,
            rss_kb: 0,
            uid: 1000,
            start_secs: 0,
            exe: None,
            environ: env.map(|e| e.to_vec()),
            cmdline: None,
            has_dri_fd: dri,
            cgroup: None,
        }
    }

    #[test]
    fn steam_env_vars_flag_a_game() {
        let e = entry(
            1,
            Some(b"\x00HOME=/home\x00SteamGameId=892970\x00".as_slice()),
            false,
        );
        assert!(detect_game(&e, &NoopFullscreenDetector).steam_env);
        let e2 = entry(2, Some(b"SteamAppId=123400".as_slice()), false);
        assert!(detect_game(&e2, &NoopFullscreenDetector).steam_env);
        let e3 = entry(
            3,
            Some(b"STEAM_COMPAT_DATA_PATH=/home/x/.steam/steam/steamapps/compatdata/1234".as_slice()),
            false,
        );
        assert!(detect_game(&e3, &NoopFullscreenDetector).steam_env);
        // launcher scaffolding (Proton wrapper, runtime reaper) also carries
        // the env but never renders — only the DRM-rendering process is the
        // game.
        assert!(!detect_game(&e, &NoopFullscreenDetector).is_game());
        assert!(detect_game(&entry(4, Some(b"SteamGameId=892970".as_slice()), true), &NoopFullscreenDetector).is_game());
    }

    #[test]
    fn no_steam_vars_does_not_flag() {
        let e = entry(1, Some(b"HOME=/home\x00PATH=/usr/bin".as_slice()), false);
        let flags = detect_game(&e, &NoopFullscreenDetector);
        assert!(!flags.steam_env);
        assert!(!flags.is_game());
        // Unreadable environment also doesn't flag.
        assert!(!detect_game(&entry(2, None, false), &NoopFullscreenDetector).is_game());
    }

    #[test]
    fn dri_fd_alone_does_not_flag_without_fullscreen() {
        // Regression: the DRI check used to flag anything with a `/dev/dri`
        // fd, which on a desktop is most GUI applications (browsers, KDE
        // services...).  Without a fullscreen signal it must not count.
        assert!(!detect_game(&entry(1, None, true), &NoopFullscreenDetector).is_game());
    }

    #[test]
    fn dri_fd_plus_fullscreen_flags_a_game() {
        struct Always;
        impl FullscreenDetector for Always {
            fn refresh(&self) {}
            fn is_fullscreen(&self, _entry: &ProcEntry) -> bool {
                true
            }
        }
        assert!(detect_game(&entry(1, None, true), &Always).is_game());
        assert!(detect_game(&entry(1, None, false), &Always).fullscreen);
        assert!(!detect_game(&entry(1, None, false), &Always).is_game());
    }

    #[test]
    fn fullscreen_detector_is_injected() {
        struct Always;
        impl FullscreenDetector for Always {
            fn refresh(&self) {}
            fn is_fullscreen(&self, _entry: &ProcEntry) -> bool {
                true
            }
        }
        let flags = detect_game(&entry(1, None, false), &Always);
        assert!(flags.fullscreen);
        // Fullscreen alone is not a game: needs a DRM fd too.
        assert!(!flags.is_game());
    }

    #[test]
    fn steam_resource_matches_own_environ_appid() {
        struct Stub {
            state: (String, bool),
        }
        impl FullscreenDetector for Stub {
            fn refresh(&self) {}
            fn is_fullscreen(&self, entry: &ProcEntry) -> bool {
                let (resource, fullscreen) = &self.state;
                if !fullscreen {
                    return false;
                }
                if resource == &entry.name {
                    return true;
                }
                if let Some(appid) = steam_appid_from_resource(resource) {
                    return environ_has_steam_appid(entry.environ.as_deref(), appid);
                }
                false
            }
        }

        let game = entry(
            1,
            Some(b"SteamAppId=1690940\x00SteamGameId=1690940".as_slice()),
            true,
        );
        let det = Stub {
            state: ("steam_app_1690940".to_string(), true),
        };
        assert!(det.is_fullscreen(&game));
        assert!(detect_game(&game, &det).is_game());

        // A different appid does not match — and with no Steam env of its
        // own the process stays undetected (dri alone + no fullscreen).
        let other = entry(2, None, true);
        assert!(!det.is_fullscreen(&other));
        assert!(!detect_game(&other, &det).is_game());

        // Not fullscreen => no match.
        let det = Stub {
            state: ("steam_app_1690940".to_string(), false),
        };
        assert!(!det.is_fullscreen(&game));
    }

    #[test]
    fn wine_internal_processes_never_flag() {
        // Wine helpers inherit the Steam env of the game they serve but run
        // from the prefix's drive_c — they must never count as games.
        let mut e = entry(1, Some(b"SteamAppId=1690940".as_slice()), true);
        e.exe = Some("/home/user/.steam/steam/steamapps/compatdata/1690940/pfx/drive_c/windows/system32/explorer.exe".into());
        e.name = "explorer.exe".into();
        assert!(!detect_game(&e, &NoopFullscreenDetector).is_game());
        assert!(is_wine_internal(&e));

        // Under Proton the exe link resolves to wine64-preloader for every
        // process, so the cmdline (which names the real module) must carry
        // the check: `C:\windows\system32\<helper>` and pressure-vessel
        // runtime scaffolding are both wine-internal.
        let mut w = entry(2, Some(b"SteamAppId=1690940".as_slice()), true);
        w.name = "winedevice.exe".into();
        w.exe = Some("/home/user/.local/share/Steam/compatibilitytools.d/GE-Proton8-20/files/bin/wine64-preloader".into());
        w.cmdline = Some(b"C:\\windows\\system32\\winedevice.exe\0".to_vec());
        assert!(is_wine_internal(&w));
        let flags = detect_game(&w, &NoopFullscreenDetector);
        assert!(!flags.steam_env && !flags.dri_fd && !flags.fullscreen && !flags.is_game());

        let mut pv = entry(3, Some(b"SteamAppId=1690940".as_slice()), true);
        pv.name = "srt-bwrap".into();
        pv.cmdline = Some(b"steam-runtime-tools/srt-bwrap-pressure-vessel\0...".to_vec());
        assert!(is_wine_internal(&pv));

        // The game itself runs from steamapps/common with a Z:\ style argv:
        // not wine-internal, still detectable.
        let mut g = entry(4, Some(b"SteamAppId=1690940".as_slice()), true);
        g.name = "DELTARUNE.exe".into();
        g.exe = Some("/home/user/.local/share/Steam/compatibilitytools.d/GE-Proton8-20/files/bin/wine64-preloader".into());
        g.cmdline = Some(b"Z:\\home\\user\\.local\\share\\Steam\\steamapps\\common\\DELTARUNEdemo\\DELTARUNE.exe\0-game\0data.win".to_vec());
        assert!(!is_wine_internal(&g));
        assert!(detect_game(&g, &NoopFullscreenDetector).steam_env);
    }

    #[test]
    fn de_critical_processes_never_flag_as_games() {
        struct Always;
        impl FullscreenDetector for Always {
            fn refresh(&self) {}
            fn is_fullscreen(&self, _entry: &ProcEntry) -> bool {
                true
            }
        }
        // The compositor holds DRI fds and can be "fullscreen" (overview,
        // lock screen) — it must never receive game scheduling.
        for name in ["kwin_wayland", "plasmashell", "kded6", "Xwayland", "org_kde_powerdevil"] {
            assert!(is_de_critical(name), "{name} must be DE-critical");
            let mut e = entry(1, None, true);
            e.name = name.into();
            let flags = detect_game(&e, &Always);
            assert!(!flags.is_game(), "{name} must not be a game candidate");
            assert!(!flags.fullscreen && !flags.steam_env, "{name} flags must be neutral");
        }
        // User apps stay detectable.
        assert!(!is_de_critical("DELTARUNE.exe"));
        assert!(!is_de_critical("antigravity"));
    }

    #[test]
    fn own_binaries_are_never_detectable() {
        let mine = nicewatch_common::APP_NAME;
        assert!(is_self_binary(mine));
        assert!(is_self_binary(&format!("{mine}-gui")));
        assert!(!is_self_binary("steam"));
        assert!(!is_self_binary("nicewatch-hamster"));
    }
}