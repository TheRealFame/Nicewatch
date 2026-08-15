//! Curated "known games" list, embedded in the binary.
//!
//! The Steam-env/DRM heuristic only catches games that run under Steam or
//! hold a `/dev/dri` fd.  Games launched outside Steam (native binaries,
//! Flatpak apps, emulators) match nothing, so we ship a list of known game
//! process names and Flatpak ids (`known_games.json`, built from the Flathub
//! game catalog + curated comm names + Steam's native-Linux title list).
//!
//! A process is "known" when any of these hit (case-insensitive):
//!   * its `comm` matches a listed process token,
//!   * its `exe` basename matches a listed process token,
//!   * any `/proc/<pid>/cmdline` path segment matches a listed token,
//!   * its cgroup scope contains a listed Flatpak app id
//!     (`app-flatpak-<appid>-<hash>.scope`).
//!
//! Known games never trigger the confirmation prompt and receive the
//! auto-game tier immediately (explicit rules still take precedence).
//! Reverse-DNS tokens (`org.example.app`) are treated as Flatpak ids; every
//! other token is a process-name token.

use serde::Deserialize;

use crate::proc_scan::ProcEntry;

/// A Flatpak cgroup scope looks like
/// `…/app-flatpak-org.virtually_compatible.Sober-0a1b2c3d.scope`.
const FLATPAK_MARKER: &str = "app-flatpak-";

#[derive(Deserialize)]
struct Entry {
    #[serde(rename = "t")]
    token: String,
    #[serde(rename = "n")]
    name: String,
}

#[derive(Default)]
pub struct KnownGames {
    /// Process-name tokens (comm / exe basename / cmdline segments).
    names: std::collections::HashSet<String>,
    /// Flatpak app ids (reverse-DNS), matched against the cgroup scope.
    flatpak: std::collections::HashSet<String>,
}

impl KnownGames {
    /// Parse the embedded catalog.  Never fails: on a malformed table we
    /// degrade to an empty list (the heuristic still works).
    pub fn embedded() -> Self {
        let raw = include_str!("known_games.json");
        let Ok(entries) = serde_json::from_str::<Vec<Entry>>(raw) else {
            return Self::default();
        };
        let mut out = Self::default();
        for e in entries {
            let token = e.token.trim().to_ascii_lowercase();
            if token.is_empty() {
                continue;
            }
            if is_flatpak_id(&token) {
                out.flatpak.insert(token);
            } else {
                out.names.insert(token);
            }
        }
        out
    }

    /// Quick `comm`-name check (used to skip prompts for known games).
    pub fn is_known_name(&self, comm: &str) -> bool {
        !comm.is_empty() && self.names.contains(&comm.to_ascii_lowercase())
    }

    /// Full detection: comm, exe basename, cmdline path segments, Flatpak
    /// cgroup scope.
    pub fn is_known_entry(&self, entry: &ProcEntry) -> bool {
        if self.is_known_name(&entry.name) {
            return true;
        }
        if let Some(exe) = entry.exe.as_deref() {
            if let Some(base) = basename(exe) {
                if self.is_known_name(base) {
                    return true;
                }
            }
        }
        if let Some(cmdline) = entry.cmdline.as_deref() {
            for segment in cmdline
                .split(|&b| b == b'/' || b == b'\\' || b == b':' || b == b' ')
                .filter(|s| !s.is_empty())
            {
                let s = String::from_utf8_lossy(segment);
                let clean = s.trim_end_matches(".exe").trim_end_matches(".EXE");
                if !clean.is_empty() && self.is_known_name(clean) {
                    return true;
                }
            }
        }
        if let Some(cgroup) = entry.cgroup.as_deref() {
            if let Some(id) = flatpak_id_from_cgroup(cgroup) {
                if self.flatpak.contains(&id) {
                    return true;
                }
            }
        }
        false
    }

    pub fn len(&self) -> usize {
        self.names.len() + self.flatpak.len()
    }
}

/// A token with at least two dots is a reverse-DNS Flatpak app id.
fn is_flatpak_id(token: &str) -> bool {
    token.bytes().filter(|&b| b == b'.').count() >= 2
}

/// `app-flatpak-<appid>-<hash>.scope` → `<appid>` (lowercased).
fn flatpak_id_from_cgroup(cgroup: &str) -> Option<String> {
    let lower = cgroup.to_ascii_lowercase();
    let start = lower.find(FLATPAK_MARKER)? + FLATPAK_MARKER.len();
    let end = lower[start..].find(".scope")? + start;
    let inner = &lower[start..end];
    // Strip the trailing `-<hash>` when present.
    Some(match inner.rfind('-') {
        Some(idx) if idx > 0 => inner[..idx].to_string(),
        _ => inner.to_string(),
    })
}

fn basename(path: &str) -> Option<&str> {
    path.rsplit('/').next().filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, exe: Option<&str>, cmdline: Option<&[u8]>, cgroup: Option<&str>) -> ProcEntry {
        ProcEntry {
            pid: 1,
            ppid: 1,
            name: name.into(),
            state: 'S',
            utime: 0,
            stime: 0,
            nice: 0,
            rss_kb: 0,
            uid: 1000,
            start_secs: 0,
            exe: exe.map(str::to_string),
            environ: None,
            cmdline: cmdline.map(|c| c.to_vec()),
            has_dri_fd: false,
            cgroup: cgroup.map(str::to_string),
        }
    }

    #[test]
    fn embedded_list_loads_and_is_big() {
        let kg = KnownGames::embedded();
        // The shipped catalog is multi-thousand entries; guard against a
        // regression where it silently shrinks to nothing.
        assert!(kg.len() >= 2000, "catalog shrank to {}", kg.len());
    }

    #[test]
    fn comm_matches_known_game() {
        let kg = KnownGames::embedded();
        // Sober (Roblox) is the canonical known-game case.
        assert!(kg.is_known_name("sober"));
        let e = entry("sober", None, None, None);
        assert!(kg.is_known_entry(&e));
    }

    #[test]
    fn flatpak_cgroup_scope_matches() {
        let kg = KnownGames::embedded();
        assert!(kg.flatpak.contains("org.virtually_compatible.sober"));
        let e = entry(
            "bwrap",
            None,
            None,
            Some("/system.slice/app-flatpak-org.virtually_compatible.Sober-a1b2c3.scope"),
        );
        assert!(kg.is_known_entry(&e));
    }

    #[test]
    fn cmdline_segment_matches() {
        let kg = KnownGames::embedded();
        // A wine-style argv names the real module.
        let e = entry(
            "wine64-preloader",
            Some("/usr/lib/wine/wine64-preloader"),
            Some(b"Z:\\home\\u\\Steam\\steamapps\\common\\Deltarune\\DELTARUNE.exe\0-data".as_slice()),
            None,
        );
        assert!(kg.is_known_entry(&e));
    }

    #[test]
    fn random_processes_do_not_match() {
        let kg = KnownGames::embedded();
        for (name, exe, cmd, cg) in [
            ("kwin_wayland", Some("/usr/bin/kwin_wayland"), None, None),
            ("node", Some("/usr/bin/node"), Some(b"/usr/bin/node\0server.js".as_slice()), None),
            (
                "app-flatpak-org.example.notagame-abc123",
                None,
                None,
                Some("/system.slice/app-flatpak-org.example.notagame-abc123.scope"),
            ),
        ] {
            let e = entry(name, exe, cmd, cg);
            assert!(!kg.is_known_entry(&e), "{name} must not be known");
        }
    }

    #[test]
    fn flatpak_id_extraction() {
        assert_eq!(
            flatpak_id_from_cgroup(
                "/system.slice/app-flatpak-org.virtually_compatible.Sober-1a2b3c.scope"
            ),
            Some("org.virtually_compatible.sober".to_string())
        );
        assert_eq!(
            flatpak_id_from_cgroup("/user.slice/app-flatpak-io.itch.itch-abc.scope"),
            Some("io.itch.itch".to_string())
        );
        assert_eq!(flatpak_id_from_cgroup("/system.slice/kde.service"), None);
    }
}