//! Application metadata resolution: map a process `exe` path to a display
//! name and a themed icon (freedesktop `.desktop` + icon-theme lookup).
//!
//! Best-effort and cached: the table scans thousands of processes, but the
//! number of distinct executables is small, so results are cached per exe
//! path and the desktop files are scanned once.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::Serialize;

#[derive(Debug, Clone, Default, Serialize)]
pub struct AppMeta {
    /// Human-friendly application name (from the desktop entry, or a
    /// prettified executable basename when none matches).
    pub name: String,
    /// Icon as a `data:` URL (base64 PNG/SVG), when one could be resolved.
    pub icon: Option<String>,
}

struct DesktopEntry {
    name: String,
    exec: String,
    try_exec: Option<String>,
    icon: Option<String>,
}

struct Resolver {
    entries: Vec<DesktopEntry>,
    cache: HashMap<String, AppMeta>,
}

static RESOLVER: Mutex<Option<Resolver>> = Mutex::new(None);

fn resolver() -> &'static Mutex<Option<Resolver>> {
    &RESOLVER
}

fn scan_desktop_files() -> Vec<DesktopEntry> {
    let mut dirs: Vec<PathBuf> = vec![
        PathBuf::from("/usr/share/applications"),
        PathBuf::from("/usr/local/share/applications"),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        dirs.push(home.join(".local/share/applications"));
        dirs.push(home.join(".local/share/flatpak/exports/share/applications"));
        dirs.push(home.join(".var/app"));
    }
    dirs.push(PathBuf::from("/var/lib/flatpak/exports/share/applications"));

    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for dir in dirs {
        let Ok(rd) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("desktop") {
                continue;
            }
            if !seen.insert(path.clone()) {
                continue;
            }
            if let Some(e) = parse_desktop(&path) {
                out.push(e);
            }
        }
    }
    out
}

fn parse_desktop(path: &Path) -> Option<DesktopEntry> {
    let text = fs::read_to_string(path).ok()?;
    let mut name = None;
    let mut exec = None;
    let mut try_exec = None;
    let mut icon = None;
    let mut no_display = false;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('[') {
            continue;
        }
        if let Some(v) = line.strip_prefix("Name=") {
            // Prefer the localized variant (Name[xx]=) if present; in the
            // common case there is only the plain Name=, so overwriting is
            // fine as long as we keep the LAST occurrence of the same key.
            name = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("Exec=") {
            exec = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("TryExec=") {
            try_exec = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("Icon=") {
            icon = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("NoDisplay=") {
            no_display = v == "true";
        }
    }
    let (name, exec) = (name?, exec?);
    if no_display {
        return None;
    }
    Some(DesktopEntry {
        name,
        exec,
        try_exec,
        icon,
    })
}

/// First whitespace-separated token of an Exec= line, minus any env-prefix
/// (`env FOO=bar cmd ...`) and path.
fn exec_program(exec: &str) -> Option<String> {
    let toks: Vec<&str> = exec.split_whitespace().collect();
    let mut tok = toks.first()?.to_string();
    if tok == "env" {
        // env VAR=x VAR2=y cmd ... -> the first non-VAR token is the program.
        for t in toks.iter().skip(1) {
            if !t.contains('=') {
                tok = t.to_string();
                break;
            }
        }
    }
    if tok.starts_with('/') {
        tok = Path::new(&tok)
            .file_name()?
            .to_string_lossy()
            .into_owned();
    }
    Some(tok)
}

fn icon_lookup(name: &str) -> Option<String> {
    if name.is_empty() {
        return None;
    }
    let file = if name.starts_with('/') {
        Path::new(name).is_file().then(|| name.to_string())
    } else {
        icon_theme_path(name)
    }?;
    // Read it back as a data URL so the webview can render it without any
    // filesystem/asset-protocol permissions.
    let bytes = fs::read(&file).ok()?;
    let mime = if file.ends_with(".svg") {
        "image/svg+xml"
    } else {
        "image/png"
    };
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
    Some(format!("data:{mime};base64,{b64}"))
}

fn icon_theme_path(name: &str) -> Option<String> {
    let mut dirs: Vec<PathBuf> = vec![
        PathBuf::from("/usr/share/icons/hicolor/128x128/apps"),
        PathBuf::from("/usr/share/icons/hicolor/256x256/apps"),
        PathBuf::from("/usr/share/icons/hicolor/64x64/apps"),
        PathBuf::from("/usr/share/icons/hicolor/48x48/apps"),
        PathBuf::from("/usr/share/icons/Adwaita/128x128/apps"),
        PathBuf::from("/usr/share/pixmaps"),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        dirs.push(home.join(".local/share/icons/hicolor/128x128/apps"));
        dirs.push(home.join(".local/share/icons/hicolor/256x256/apps"));
        dirs.push(home.join(".local/share/flatpak/exports/share/icons/hicolor/128x128/apps"));
        dirs.push(home.join(".var/app"));
    }
    dirs.push(PathBuf::from("/var/lib/flatpak/exports/share/icons/hicolor/128x128/apps"));

    for dir in dirs {
        for cand in [format!("{name}.png"), format!("{name}.svg"), name.to_string()] {
            let p = dir.join(&cand);
            if p.is_file() {
                return Some(p.to_string_lossy().into_owned());
            }
        }
    }
    None
}

fn prettify(basename: &str) -> String {
    basename
        .split(['-', '_', '+', '.'])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Resolve metadata for one executable path (cached).
pub fn app_meta(exe: &str) -> AppMeta {
    let mut guard = resolver().lock().unwrap_or_else(|p| p.into_inner());
    let res = guard.get_or_insert_with(|| Resolver {
        entries: scan_desktop_files(),
        cache: HashMap::new(),
    });
    if let Some(m) = res.cache.get(exe) {
        return m.clone();
    }
    let meta = resolve(&res.entries, exe);
    res.cache.insert(exe.to_string(), meta.clone());
    meta
}

fn resolve(entries: &[DesktopEntry], exe: &str) -> AppMeta {
    let exe_path = Path::new(exe);
    let basename = exe_path
        .file_name()
        .map(|b| b.to_string_lossy().into_owned())
        .unwrap_or_default();

    for e in entries {
        let exec_prog = exec_program(&e.exec);
        let try_prog = e.try_exec.as_deref().and_then(exec_program);
        let matches_exec = exec_prog.as_deref() == Some(basename.as_str());
        let matches_try = try_prog.as_deref() == Some(basename.as_str());
        // Absolute Exec= tokens can match the exe path directly.
        let matches_path = e
            .exec
            .split_whitespace()
            .next()
            .map(|t| t == exe)
            .unwrap_or(false);
        if matches_exec || matches_try || matches_path {
            let icon = e.icon.as_deref().and_then(icon_lookup);
            return AppMeta {
                name: e.name.clone(),
                icon,
            };
        }
    }
    AppMeta {
        name: prettify(&basename),
        icon: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_program_handles_env_prefix_and_paths() {
        assert_eq!(exec_program("firefox"), Some("firefox".into()));
        assert_eq!(exec_program("/usr/bin/firefox --new-window"), Some("firefox".into()));
        assert_eq!(
            exec_program("env FOO=1 BAR=2 vesktop.bin --ozone"),
            Some("vesktop.bin".into())
        );
        assert_eq!(exec_program("flatpak run org.foo.Bar"), Some("flatpak".into()));
    }

    #[test]
    fn prettify_makes_readable_names() {
        assert_eq!(prettify("vesktop.bin"), "Vesktop Bin");
        assert_eq!(prettify("Isolated Web Co"), "Isolated Web Co");
        assert_eq!(prettify("chrome_crashpad_handler"), "Chrome Crashpad Handler");
        assert_eq!(prettify("firefox-bin"), "Firefox Bin");
    }

    #[test]
    fn resolve_matches_desktop_entries_by_basename() {
        let entries = vec![DesktopEntry {
            name: "Mozilla Firefox".into(),
            exec: "/usr/lib/firefox/firefox".into(),
            try_exec: None,
            icon: Some("firefox".into()),
        }];
        let meta = resolve(&entries, "/usr/lib/firefox/firefox");
        assert_eq!(meta.name, "Mozilla Firefox");
        let meta2 = resolve(&entries, "/opt/custom/firefox");
        assert_eq!(meta2.name, "Mozilla Firefox");
        let miss = resolve(&entries, "/usr/bin/whatever");
        assert_eq!(miss.name, "Whatever");
    }
}