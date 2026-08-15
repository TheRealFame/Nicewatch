//! GUI-local settings, persisted as JSON next to the daemon's config
//! directory.  These are window/app preferences only — the daemon has its
//! own config for poll interval and rules.

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct GuiSettings {
    /// Start the app hidden in the system tray instead of opening the window.
    pub start_in_tray: bool,
    /// Closing the window hides to the tray instead of quitting.
    pub minimize_to_tray: bool,
}

pub fn settings_path() -> PathBuf {
    let base = nicewatch_common::local_config_path()
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("gui-settings.json")
}

pub fn load() -> GuiSettings {
    let Ok(text) = fs::read_to_string(settings_path()) else {
        return GuiSettings::default();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

pub fn save(s: &GuiSettings) -> Result<(), String> {
    let path = settings_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(s).map_err(|e| e.to_string())?;
    fs::write(&path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))
}