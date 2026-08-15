//! Native OS theme + accent extraction.
//!
//! This is a faithful Rust port of the detection ladder in the
//! `@nearcade/native-palette` and `@nearcade/accent-color` npm packages
//! (see gui/package.json for the originals): KDE `kdeglobals` first, then GTK
//! `gsettings`, then bare WMs (XDG portal / `.Xresources`), with the accent
//! cascade portal → GNOME → Cinnamon → MATE → COSMIC → KDE6 → KDE5 → Hyprland
//! → GTK-theme guess → neutral fallback.
//!
//! Ported instead of required because the webview can't run Node modules and
//! AppImage users shouldn't need Node installed.  Same output shape, same
//! fallbacks, std-only (plus a tiny INI parser here).

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct NativeTheme {
    /// Window background ("canvas").
    pub bg: String,
    /// Sidebar / header background.
    pub sidebar: String,
    /// Card / raised surface.
    pub surface: String,
    pub surface_hover: String,
    /// Primary text.
    pub text: String,
    pub muted: String,
    pub muted2: String,
    pub border: String,
    /// The OS accent color (hex) — always populated.
    pub accent: String,
    /// Best-effort dark/light signal (drives pill text colors).
    pub dark: bool,
}

/// Tolerated environment value (both literal `"dark"` / `'dark'` and "1").
const DEFAULT_DARK_BG: &str = "#1e1e1e";

// ---------------------------------------------------------------------------
// Result helpers
// ---------------------------------------------------------------------------

fn run(bin: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(bin).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn unquote(s: &str) -> String {
    s.trim()
        .trim_matches(|c| c == '\'' || c == '"' || c == '(')
        .trim()
        .to_string()
}

fn rgb_tuple_to_hex(rgb: &str) -> Option<String> {
    let parts: Vec<i64> = rgb
        .split(',')
        .map(|p| p.trim().parse::<i64>())
        .collect::<Result<_, _>>()
        .ok()?;
    if parts.len() < 3 || parts.iter().any(|&p| !(0..=255).contains(&p)) {
        return None;
    }
    Some(format!("#{:02x}{:02x}{:02x}", parts[0], parts[1], parts[2]))
}

// ---------------------------------------------------------------------------
// KDE (kdeglobals)
// ---------------------------------------------------------------------------

/// Minimal INI parse: `[Section]` headers, `Key=Value` lines.  Ignores
/// comments; inline `#`/`//` are kept (kdeglobals colors never contain them).
fn parse_ini(path: &Path) -> HashMap<(String, String), String> {
    let mut out = HashMap::new();
    let Ok(raw) = std::fs::read_to_string(path) else {
        return out;
    };
    let mut section = String::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('[') && line.ends_with(']') {
            if line.starts_with('[') && line.ends_with(']') {
                section = line[1..line.len() - 1].to_string();
            }
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            out.insert((section.clone(), k.trim().to_string()), unquote(v));
        }
    }
    out
}

fn get_kde_theme() -> Option<NativeTheme> {
    let path = std::path::PathBuf::from(std::env::var("HOME").ok()?)
        .join(".config")
        .join("kdeglobals");
    if !path.exists() {
        return None;
    }
    let ini = parse_ini(&path);
    let win = |k: &str| {
        ini.get(&("Colors:Window".into(), k.into()))
            .and_then(|v| rgb_tuple_to_hex(v))
    };
    let view = |k: &str| {
        ini.get(&("Colors:View".into(), k.into()))
            .and_then(|v| rgb_tuple_to_hex(v))
    };
    let btn = |k: &str| {
        ini.get(&("Colors:Button".into(), k.into()))
            .and_then(|v| rgb_tuple_to_hex(v))
    };
    let sel = |k: &str| {
        ini.get(&("Colors:Selection".into(), k.into()))
            .and_then(|v| rgb_tuple_to_hex(v))
    };

    let bg = win("BackgroundNormal")?;
    let surface = btn("BackgroundNormal")?;
    let text = win("ForegroundNormal").unwrap_or_else(|| "#ffffff".into());
    let muted = win("ForegroundInactive").unwrap_or_else(|| "#888888".into());

    Some(NativeTheme {
        bg: bg.clone(),
        sidebar: view("BackgroundNormal").unwrap_or_else(|| bg.clone()),
        surface: surface.clone(),
        // No reliable "hover" key exists: in some schemes Button
        // BackgroundAlternate is the selection (accent) color.  The frontend
        // derives hover/faint tones by mixing (see lib/theme.ts).
        surface_hover: surface.clone(),
        text,
        muted: muted.clone(),
        muted2: muted,
        border: view("BackgroundAlternate").unwrap_or_else(|| surface.clone()),
        accent: sel("BackgroundNormal").unwrap_or_default(),
        dark: is_dark(&bg),
    })
}

// ---------------------------------------------------------------------------
// GTK (gsettings) — known desktop palettes, then generic dark/light.
// ---------------------------------------------------------------------------

struct GtkPalette {
    bg: &'static str,
    sidebar: &'static str,
    surface: &'static str,
    hover: &'static str,
    text: &'static str,
    muted: &'static str,
    border: &'static str,
    accent: &'static str,
}

const GTK_PALETTES: &[(&str, GtkPalette)] = &[
    (
        "adwaita-dark",
        GtkPalette {
            bg: "#242424",
            sidebar: "#1e1e1e",
            surface: "#303030",
            hover: "#3c3c3c",
            text: "#ffffff",
            muted: "#9a9996",
            border: "#1e1e1e",
            accent: "#3584e4",
        },
    ),
    (
        "adwaita",
        GtkPalette {
            bg: "#fafafa",
            sidebar: "#f0f0f0",
            surface: "#ffffff",
            hover: "#f5f5f5",
            text: "#000000",
            muted: "#77767b",
            border: "#e6e6e6",
            accent: "#3584e4",
        },
    ),
    (
        "yaru-dark",
        GtkPalette {
            bg: "#1e1e1e",
            sidebar: "#111111",
            surface: "#2d2d2d",
            hover: "#3d3d3d",
            text: "#f7f7f7",
            muted: "#b3b3b3",
            border: "#1e1e1e",
            accent: "#e95420",
        },
    ),
    (
        "mint-y-dark",
        GtkPalette {
            bg: "#2f3032",
            sidebar: "#2a2b2d",
            surface: "#383a3c",
            hover: "#424446",
            text: "#dfdfdf",
            muted: "#a0a0a0",
            border: "#252627",
            accent: "#62a05f",
        },
    ),
    (
        "pop-dark",
        GtkPalette {
            bg: "#333132",
            sidebar: "#292728",
            surface: "#413e3f",
            hover: "#4d4a4b",
            text: "#f2f2f2",
            muted: "#b0afb0",
            border: "#292728",
            accent: "#f6d32d",
        },
    ),
];

fn gsettings_get(schema: &str, key: &str) -> Option<String> {
    run("gsettings", &["get", schema, key]).map(|s| unquote(&s))
}

fn get_gtk_theme() -> Option<NativeTheme> {
    let theme = gsettings_get("org.gnome.desktop.interface", "gtk-theme").unwrap_or_default();
    let theme_l = theme.to_lowercase();
    let scheme = gsettings_get("org.gnome.desktop.interface", "color-scheme");
    for (needle, palette) in GTK_PALETTES {
        if theme_l.contains(needle) {
            return Some(NativeTheme {
                bg: palette.bg.into(),
                sidebar: palette.sidebar.into(),
                surface: palette.surface.into(),
                surface_hover: palette.hover.into(),
                text: palette.text.into(),
                muted: palette.muted.into(),
                muted2: palette.muted.into(),
                border: palette.border.into(),
                accent: palette.accent.into(),
                dark: is_dark(palette.bg),
            });
        }
    }
    let dark = scheme.as_deref() == Some("prefer-dark") || theme_l.contains("dark");
    let palette = if dark {
        &GTK_PALETTES[0].1
    } else if !theme_l.is_empty() {
        &GTK_PALETTES[1].1
    } else {
        return None;
    };
    Some(NativeTheme {
        bg: palette.bg.into(),
        sidebar: palette.sidebar.into(),
        surface: palette.surface.into(),
        surface_hover: palette.hover.into(),
        text: palette.text.into(),
        muted: palette.muted.into(),
        muted2: palette.muted.into(),
        border: palette.border.into(),
        accent: palette.accent.into(),
        dark,
    })
}

// ---------------------------------------------------------------------------
// Bare WMs (.Xresources, XDG portal color scheme)
// ---------------------------------------------------------------------------

fn parse_xresources() -> Option<NativeTheme> {
    let path = std::path::PathBuf::from(std::env::var("HOME").ok()?)
        .join(".Xresources");
    let raw = std::fs::read_to_string(&path).ok()?;
    let mut bg: Option<String> = None;
    let mut text: Option<String> = None;
    let mut muted: Option<String> = None;
    let mut accent: Option<String> = None;
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('!') {
            continue;
        }
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let val = v.trim().to_string();
        match k.trim().to_lowercase().as_str() {
            "background" => bg.get_or_insert(val),
            "foreground" => text.get_or_insert(val),
            "color8" | "color 8" => muted.get_or_insert(val),
            "color4" | "color 4" => accent.get_or_insert(val),
            _ => continue,
        };
    }
    let bg = bg?;
    let text = text?;
    Some(NativeTheme {
        bg: bg.clone(),
        sidebar: bg.clone(),
        surface: bg.clone(),
        surface_hover: bg.clone(),
        text,
        muted: muted.clone().unwrap_or_else(|| "#444444".into()),
        muted2: muted.clone().unwrap_or_else(|| "#444444".into()),
        border: muted.unwrap_or_else(|| "#444444".into()),
        accent: accent.unwrap_or_else(|| "#8b5cf6".into()),
        dark: is_dark(&bg),
    })
}

fn get_bare_theme() -> Option<NativeTheme> {
    // XDG portal color-scheme: "1" = dark.
    if let Some(out) = run(
        "dbus-send",
        &[
            "--print-reply=literal",
            "--dest=org.freedesktop.portal.Desktop",
            "/org/freedesktop/portal/desktop",
            "org.freedesktop.portal.Settings.Read",
            "string:org.freedesktop.appearance",
            "string:color-scheme",
        ],
    ) {
        if out.trim().contains('1') {
            return Some(NativeTheme {
                bg: DEFAULT_DARK_BG.into(),
                sidebar: "#1a1a1a".into(),
                surface: "#252526".into(),
                surface_hover: "#333333".into(),
                text: "#ffffff".into(),
                muted: "#888888".into(),
                muted2: "#555555".into(),
                border: "#333333".into(),
                accent: "#8b5cf6".into(),
                dark: true,
            });
        }
    }
    parse_xresources()
}

// ---------------------------------------------------------------------------
// Accent cascade (mirrors @nearcade/accent-color lib/linux.js)
// ---------------------------------------------------------------------------

const GTK_PRESETS: &[(&str, u8, u8, u8)] = &[
    ("blue", 53, 132, 228),
    ("teal", 25, 162, 155),
    ("green", 51, 171, 80),
    ("yellow", 242, 185, 53),
    ("orange", 245, 135, 31),
    ("red", 207, 73, 73),
    ("purple", 130, 90, 209),
    ("pink", 222, 82, 150),
    ("slate", 120, 120, 130),
];

fn rgb(r: u8, g: u8, b: u8) -> String {
    format!("#{r:02x}{g:02x}{b:02x}")
}

fn portal_accent() -> Option<String> {
    let out = run(
        "dbus-send",
        &[
            "--session",
            "--print-reply",
            "--dest=org.freedesktop.portal.Desktop",
            "/org/freedesktop/portal/desktop",
            "org.freedesktop.portal.Settings.ReadOne",
            "string:org.freedesktop.appearance",
            "string:accent-color",
        ],
    )?;
    let vals: Vec<f64> = out
        .match_indices("double ")
        .map(|(i, _)| {
            out[i + "double ".len()..]
                .split_whitespace()
                .next()
                .and_then(|n| n.parse::<f64>().ok())
                .unwrap_or(0.0)
        })
        .collect();
    if vals.len() >= 3 {
        Some(rgb(
            (vals[0] * 255.0).round() as u8,
            (vals[1] * 255.0).round() as u8,
            (vals[2] * 255.0).round() as u8,
        ))
    } else {
        None
    }
}

fn gsetting_preset_or_hex(schema: &str, key: &str) -> Option<String> {
    let val = gsettings_get(schema, key)?;
    if let Some((_, r, g, b)) = GTK_PRESETS.iter().find(|(name, ..)| *name == val) {
        return Some(rgb(*r, *g, *b));
    }
    let hex = val.trim_start_matches('#');
    if hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Some(format!("#{}", hex.to_lowercase()));
    }
    None
}

fn kde_accent(bin: &str) -> Option<String> {
    let out = run(
        bin,
        &["--file", "kdeglobals", "--group", "General", "--key", "AccentColor"],
    )?;
    rgb_tuple_to_hex(&unquote(&out))
}

fn hyprland_accent() -> Option<String> {
    let out = run("hyprctl", &["getoption", "decoration:col.active_border"])?;
    let hex = out
        .split_whitespace()
        .find_map(|tok| tok.strip_prefix("0x")?.to_string().into())?;
    let clean = hex.trim_end_matches("ff");
    let clean = if clean.len() >= 6 { &clean[clean.len() - 6..] } else { clean };
    if clean.len() == 6 && clean.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(format!("#{}", clean.to_lowercase()))
    } else {
        None
    }
}

fn fallback_accent() -> String {
    // Neutral (blue) rather than the packages' purple — the indigo/purple
    // accent is the single most-recognized "AI slop" tell.
    rgb(53, 132, 228)
}

fn get_accent() -> String {
    portal_accent()
        .or_else(|| gsetting_preset_or_hex("org.gnome.desktop.interface", "accent-color"))
        .or_else(|| gsetting_preset_or_hex("org.cinnamon.desktop.interface", "accent-color"))
        .or_else(|| gsetting_preset_or_hex("org.mate.interface", "accent-color"))
        .or_else(|| gsetting_preset_or_hex("com.system76.CosmicSettings", "accent-color"))
        .or_else(|| kde_accent("kreadconfig6"))
        .or_else(|| kde_accent("kreadconfig5"))
        .or_else(hyprland_accent)
        .unwrap_or_else(fallback_accent)
}

fn is_dark(hex: &str) -> bool {
    let hex = hex.trim_start_matches('#');
    if hex.len() != 6 {
        return true;
    }
    if let (Ok(r), Ok(g), Ok(b)) = (
        u8::from_str_radix(&hex[0..2], 16),
        u8::from_str_radix(&hex[2..4], 16),
        u8::from_str_radix(&hex[4..6], 16),
    ) {
        (0.299 * f32::from(r) + 0.587 * f32::from(g) + 0.114 * f32::from(b)) < 128.0
    } else {
        true
    }
}

// ---------------------------------------------------------------------------
// Public entry
// ---------------------------------------------------------------------------

/// Extract the native theme, walking KDE → GTK → bare.  The accent is always
/// filled in from the dedicated cascade even when the theme backend lacks one.
pub fn get_native_theme() -> NativeTheme {
    let mut theme = get_kde_theme()
        .or_else(get_gtk_theme)
        .or_else(get_bare_theme)
        .unwrap_or_else(|| NativeTheme {
            bg: DEFAULT_DARK_BG.into(),
            sidebar: "#252526".into(),
            surface: "#2d2d30".into(),
            surface_hover: "#3e3e42".into(),
            text: "#d4d4d4".into(),
            muted: "#808080".into(),
            muted2: "#555555".into(),
            border: "#404040".into(),
            accent: fallback_accent(),
            dark: true,
        });
    if theme.accent.is_empty() {
        theme.accent = get_accent();
    }
    theme
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgb_tuple_parsing() {
        assert_eq!(
            rgb_tuple_to_hex("34,209,238").as_deref(),
            Some("#22d1ee")
        );
        assert_eq!(rgb_tuple_to_hex("12, 34, 56").as_deref(), Some("#0c2238"));
        assert_eq!(rgb_tuple_to_hex("300,0,0"), None);
        assert_eq!(rgb_tuple_to_hex("a,b,c"), None);
    }

    #[test]
    fn ini_parse_sections() {
        let dir = std::env::temp_dir().join("nw-theme-test.ini");
        std::fs::write(
            &dir,
            "[Colors:Window]\nBackgroundNormal=12,13,14\nForegroundNormal=200,200,200\n[Colors:Button]\nBackgroundNormal=1,2,3\n",
        )
        .unwrap();
        let ini = parse_ini(&dir);
        assert_eq!(
            ini.get(&("Colors:Window".into(), "BackgroundNormal".into())).unwrap(),
            "12,13,14"
        );
        assert_eq!(
            ini.get(&("Colors:Button".into(), "BackgroundNormal".into())).unwrap(),
            "1,2,3"
        );
        assert!(ini.get(&("Colors:Window".into(), "Bogus".into())).is_none());
        std::fs::remove_file(&dir).ok();
    }

    #[test]
    fn darkness_detection() {
        assert!(is_dark("#0a0a0a"));
        assert!(!is_dark("#ffffff"));
        assert!(!is_dark("#e0e0e0"));
    }

    #[test]
    fn accent_fallback_is_neutral_not_purple() {
        assert_eq!(fallback_accent(), "#3584e4");
    }
}
#[cfg(test)]
mod live_probe {
    use super::get_native_theme;

    #[test]
    #[ignore = "prints the live theme for manual verification"]
    fn print_theme() {
        let t = get_native_theme();
        println!("detected theme: {t:?}");
    }
}
