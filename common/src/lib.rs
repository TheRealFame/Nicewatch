#![doc = "Common crate: the single source of truth for the application name, the rule
schema, the priority presets, and the IPC protocol shared by the daemon and
the GUI."]

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Application identity.
//
// THE APP NAME IS DEFINED EXACTLY ONCE HERE.  Do not re-type the literal
// string "Nicewatch" (or its filename spelling) anywhere else in the codebase.
// Import these constants instead.  The Cargo bin/package identifiers in the
// manifests (nicewatch, nicewatch-daemon, nicewatch-gui, nicewatch-common)
// are the build-system mirror of `APP_NAME`; keep them in lockstep with this
// constant if the project is ever renamed.
// ---------------------------------------------------------------------------

/// Filename spelling: binary name, IPC socket file, systemd unit, log prefix.
pub const APP_NAME: &str = "nicewatch";

/// Human-readable display name: window titles, CLI help, README-facing text.
pub const APP_DISPLAY_NAME: &str = "Nicewatch";

/// Binaries belonging to the Nicewatch project itself.  The daemon's
/// game-detection heuristic never flags these as games: the GUI holds a
/// `/dev/dri` fd for WebKit/GL rendering, which the heuristic would otherwise
/// read as "game".  `concat!` cannot reuse `APP_NAME` (it only accepts
/// literals), so this is the one place the two binary spellings live; they
/// mirror the Cargo bin names (`nicewatch`, `nicewatch-gui`).
pub const SELF_BINARY_NAMES: [&str; 2] = ["nicewatch", "nicewatch-gui"];

/// Config directory name under `/etc` / `~/.config`.  This is a fixed path
/// mandated by the integration spec, deliberately not derived from the app
/// name so a rename never silently moves the config location.
pub const CONFIG_DIR_NAME: &str = "proc-priority-daemon";

// ---------------------------------------------------------------------------
// Timing constants.
// ---------------------------------------------------------------------------

/// Fallback poll interval for the /proc scan loop (ms).  Overridable via the
/// `--poll-ms` CLI flag and via `poll_interval_ms` in the rules file.
pub const DEFAULT_POLL_INTERVAL_MS: u64 = 2000;

/// How long the local (staging) config must stop changing before it is
/// promoted over the root (live) `/etc/...` config.  The root file therefore
/// always reflects a "settled" state.
pub const PROMOTE_DEBOUNCE: std::time::Duration = std::time::Duration::from_secs(30);

/// Debounce before flushing a GUI-initiated rule edit to the local config
/// file on disk (rapid preset clicks collapse into one write).
pub const LOCAL_WRITE_DEBOUNCE: std::time::Duration = std::time::Duration::from_secs(1);

/// Poll-interval floor, to keep the poll loop from being set destructively
/// fast.
pub const MIN_POLL_INTERVAL_MS: u64 = 250;

// ---------------------------------------------------------------------------
// Well-known paths.  All derived from the constants above; nothing hardcoded
// elsewhere.
// ---------------------------------------------------------------------------

use std::path::PathBuf;

/// XDG runtime dir (fallback: system temp dir).  Used for the IPC socket so
/// multiple users never clash.
pub fn runtime_dir() -> PathBuf {
    if let Ok(d) = std::env::var("XDG_RUNTIME_DIR") {
        if !d.is_empty() {
            return PathBuf::from(d);
        }
    }
    std::env::temp_dir()
}

/// Unix-domain socket the daemon listens on and the GUI connects to.
pub fn ipc_socket_path() -> PathBuf {
    runtime_dir().join(format!("{APP_NAME}.sock"))
}

/// Authoritative "live" config.
pub fn root_config_path() -> PathBuf {
    PathBuf::from("/etc").join(CONFIG_DIR_NAME).join("rules.toml")
}

/// Local staging/backup config.
pub fn local_config_path() -> PathBuf {
    if let Ok(d) = std::env::var("XDG_CONFIG_HOME") {
        if !d.is_empty() {
            return PathBuf::from(d).join(CONFIG_DIR_NAME).join("rules.toml");
        }
    }
    match std::env::var("HOME") {
        Ok(h) if !h.is_empty() => PathBuf::from(h)
            .join(".config")
            .join(CONFIG_DIR_NAME)
            .join("rules.toml"),
        _ => PathBuf::from(CONFIG_DIR_NAME).join("rules.toml"),
    }
}

// ---------------------------------------------------------------------------
// Tauri event names (single definition point so both sides agree).
// ---------------------------------------------------------------------------

pub const EVT_HELLO: &str = "nw/hello";
pub const EVT_SNAPSHOT: &str = "nw/snapshot";
pub const EVT_DIFF: &str = "nw/diff";
pub const EVT_PROMPT: &str = "nw/prompt";
pub const EVT_WARN: &str = "nw/warn";

// ---------------------------------------------------------------------------
// Priority presets.
//
// IMPORTANT (read before "improving" the Realtime preset):
// The "Realtime" preset is the most aggressive *safe* preset.  It maps to
// nice -12 with ionice best-effort/0 — still the ordinary CFS scheduling
// class.  We deliberately do NOT use SCHED_FIFO/SCHED_RR or any "unrestricted"
// mode here, because a misbehaving process on true kernel realtime scheduling
// runs at system-call priority, preempts everything including the kernel's own
// bookkeeping, and can starve the whole machine — hanging it beyond even a
// remote kill.  No preset in this application may ever set a realtime
// scheduling class; if a future developer reads the word "Realtime" and is
// tempted to translate it to SCHED_FIFO/RR, re-read this comment first.  That
// path is intentionally closed.
// ---------------------------------------------------------------------------

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "kebab-case")]
pub enum Tier {
    #[default]
    Software,
    Game,
    Streaming,
    Realtime,
}

impl Tier {
    pub const ALL: [Tier; 4] = [Tier::Software, Tier::Game, Tier::Streaming, Tier::Realtime];

    pub fn label(self) -> &'static str {
        match self {
            Tier::Software => "Software",
            Tier::Game => "Game",
            Tier::Streaming => "Streaming",
            Tier::Realtime => "Realtime",
        }
    }

    /// The safe internal mapping for this tier.  See the README for the
    /// mapping table and the safety rationale above re: realtime.
    pub fn preset(self) -> Preset {
        match self {
            // Default tier: background apps, browsers, anything not a game.
            Tier::Software => Preset {
                nice: 0,
                ionice_class: IoniceClass::BestEffort,
                ionice_priority: 4,
            },
            // Auto-detected tier (Steam env vars / DRM fd heuristic).
            Tier::Game => Preset {
                nice: -8,
                ionice_class: IoniceClass::BestEffort,
                ionice_priority: 2,
            },
            // Latency-sensitive capture/broadcast (e.g. OBS).
            Tier::Streaming => Preset {
                nice: -10,
                ionice_class: IoniceClass::BestEffort,
                ionice_priority: 1,
            },
            // Safe ceiling: still CFS, most favorable niceness + IO priority.
            Tier::Realtime => Preset {
                nice: -12,
                ionice_class: IoniceClass::BestEffort,
                ionice_priority: 0,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum IoniceClass {
    /// No explicit IO class / kernel default.
    #[default]
    None,
    // Not used by any built-in preset (requires CAP_SYS_ADMIN), but allowed
    // for hand-written rules.
    Realtime,
    BestEffort,
    Idle,
}

impl IoniceClass {
    pub fn label(self) -> &'static str {
        match self {
            IoniceClass::None => "none",
            IoniceClass::Realtime => "rt",
            IoniceClass::BestEffort => "be",
            IoniceClass::Idle => "idle",
        }
    }
}

/// A concrete nice + ionice combination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Preset {
    pub nice: i32,
    pub ionice_class: IoniceClass,
    pub ionice_priority: u8,
}

/// Optional limits for a cgroup v2 path (CPU cap/weight, memory.high).
///
/// When `path` is omitted the daemon picks a writable cgroup v2 base
/// automatically (the current user's delegated session subtree, or the
/// daemon's own cgroup when running as root) and creates a per-rule child
/// of it.  The process is moved into that cgroup, so the limits bind
/// (verified: `cpu_cap_percent = 50` throttles a process to ~50% of one
/// core; `nice` alone cannot do this).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CgroupLimits {
    /// Explicit cgroup directory.  When absent, auto-derived from a writable
    /// base + the rule name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// cpu.weight value (1..=10000 for cgroup v2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_weight: Option<u32>,
    /// Hard CPU cap: percent of ONE core (100 = a full core; the machine's
    /// other cores stay untouched).  Implemented as cgroup v2 `cpu.max`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_cap_percent: Option<u32>,
    /// memory.high value, e.g. "4G" or "2048M".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_high: Option<String>,
    /// Hard memory cap: cgroup v2 `memory.max` value, e.g. "6G".  The kernel
    /// enforces it by reclaiming and, if necessary, OOM-killing processes in
    /// this group — unlike `memory_high`, this is a wall, not a soft target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_max: Option<String>,
    /// cgroup v2 `cpu.idle`: when true the group's tasks only run when no
    /// other (non-idle) task is runnable — an even stronger deprioritization
    /// than a low `cpu_weight`.  Ideal for pure background apps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_idle: Option<bool>,
}

/// A single rule.  `name` is the TOML table key (`[rules.<name>]`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rule {
    #[serde(default, skip_serializing)]
    pub name: String,
    /// `/proc/<pid>/comm` value to match against (exact string match).
    #[serde(rename = "match")]
    pub match_name: String,
    /// Explicit tier (if given, and it wins over the game-detection heuristic).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<Tier>,
    /// Explicit niceness override (-20..=19).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nice: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ionice_class: Option<IoniceClass>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ionice_priority: Option<u8>,
    /// Apply only once the process has been running at least `delay` seconds
    /// (avoids reacting to short-lived processes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cgroup: Option<CgroupLimits>,
}

impl Rule {
    /// Build a rule from the given preset (used for GUI-set rules and for
    /// the confirmation-window "persist" answers).
    pub fn from_preset(process_name: &str, tier: Tier) -> Self {
        let p = tier.preset();
        Rule {
            name: process_name.to_string(),
            match_name: process_name.to_string(),
            tier: Some(tier),
            nice: Some(p.nice),
            ionice_class: Some(p.ionice_class),
            ionice_priority: Some(p.ionice_priority),
            delay: None,
            cgroup: None,
        }
    }
}

/// The `[auto_game_default]` section: preset applied to process names the
/// game-detection heuristic flagged and that have no explicit rule.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct AutoGameConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<Tier>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nice: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ionice_class: Option<IoniceClass>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ionice_priority: Option<u8>,
}

impl AutoGameConfig {
    pub fn preset(&self) -> Preset {
        let base = self.tier.unwrap_or(Tier::Game).preset();
        Preset {
            nice: self.nice.unwrap_or(base.nice),
            ionice_class: self.ionice_class.unwrap_or(base.ionice_class),
            ionice_priority: self.ionice_priority.unwrap_or(base.ionice_priority),
        }
    }
}

/// Parsed contents of a `rules.toml` file.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poll_interval_ms: Option<u64>,
    #[serde(default)]
    pub rules: std::collections::BTreeMap<String, Rule>,
    #[serde(default, rename = "auto_game_default", skip_serializing_if = "Option::is_none")]
    pub auto_game: Option<AutoGameConfig>,
}

impl AppConfig {
    /// `Rule::name` is not serialized (the TOML table key IS the name), so
    /// restore it from the map key whenever a config is loaded.
    pub fn fill_rule_names(&mut self) {
        for (key, rule) in &mut self.rules {
            rule.name = key.clone();
        }
    }
}

// ---------------------------------------------------------------------------
// Process table / IPC protocol.
//
// Wire format: newline-delimited JSON (one message per line).  The daemon
// sends a full Snapshot on first connection, then only Diffs of added /
// updated / removed processes each poll cycle.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcessInfo {
    pub pid: u32,
    pub ppid: u32,
    /// `/proc/<pid>/comm`
    pub name: String,
    pub user: String,
    /// Percentage of one CPU core over the last poll interval.
    pub cpu_percent: f32,
    pub mem_kb: u64,
    pub status: String,
    /// Current niceness as read from the kernel.
    pub nice: i32,
    /// Current ionice class as read from the kernel.
    pub ionice_class: IoniceClass,
    pub ionice_priority: u8,
    /// The tier this process currently resolves to (None = default/software).
    pub tier: Option<Tier>,
    /// True when the process lives in a Nicewatch-managed cgroup (i.e. a
    /// rule's cgroup limits are actually binding on it right now).
    pub capped: bool,
    /// True when the game-detection heuristic flagged this process.
    pub game_detected: bool,
    pub exe: Option<String>,
    /// Wall-clock start time (unix seconds), used for rule `delay` handling.
    pub start_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleInfo {
    pub name: String,
    pub match_name: String,
    pub tier: Option<Tier>,
    pub nice: Option<i32>,
    /// Percent of one core this rule caps CPU at (None = no hard cap).
    pub cpu_cap_percent: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GamePrompt {
    pub name: String,
    pub pid: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub processes: Vec<ProcessInfo>,
    pub rules: Vec<RuleInfo>,
    pub prompts: Vec<GamePrompt>,
    pub poll_interval_ms: u64,
    /// Current system-wide CPU usage as % of total capacity (matching system
    /// monitor conventions), computed from /proc/stat deltas.
    pub system_cpu: f32,
    /// Total physical memory in KiB (/proc/meminfo MemTotal).
    pub system_mem_total_kb: u64,
    /// Used physical memory in KiB (MemTotal - MemAvailable).
    pub system_mem_used_kb: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Diff {
    pub added: Vec<ProcessInfo>,
    pub updated: Vec<ProcessInfo>,
    pub removed: Vec<u32>,
    /// System CPU % at the moment this diff was produced; the GUI updates
    /// its system stats on every frame, not only on full snapshots.
    pub system_cpu: f32,
    pub system_mem_total_kb: u64,
    pub system_mem_used_kb: u64,
}

impl Diff {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.updated.is_empty() && self.removed.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GameAnswer {
    /// Apply Game tier and persist a rule for this process name.
    Yes,
    /// Leave at Software and persist a rule so it is never auto-flagged again.
    No,
    /// Apply Game tier for this running instance only; ask again next launch.
    NotNow,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ServerMsg {
    Hello {
        app_name: String,
        version: String,
        poll_interval_ms: u64,
    },
    Snapshot(Snapshot),
    Diff(Diff),
    /// Ask the GUI (OpenSnitch-style) whether a newly seen process that the
    /// heuristic flagged is really a game.  Answer via `ClientMsg::ConfirmGame`.
    PromptGame(GamePrompt),
    /// Operational warning (a rule could not be applied, config ignored
    /// root-only, ...) surfaced in the GUI banner.  Advisory only.
    Warn { msg: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ClientMsg {
    Hello { client_kind: String },
    /// Set a preset for `pid`; the daemon applies it immediately and persists
    /// a rule for the process name via the normal config-sync path.
    /// Preserves any cgroup cap the rule already carries.
    SetTier { pid: u32, tier: Tier },
    /// Set (Some pct) or remove (None) the hard CPU cap for the rule
    /// matching this process name; persists via the config-sync path.
    SetCap { name: String, pct: Option<u32> },
    /// Daemon-side bookkeeping: the process we are answering may already be
    /// gone, so resolve per process-name.
    ConfirmGame { name: String, answer: GameAnswer },
    /// Change the poll interval (ms).  The daemon validates/clamps it and
    /// persists it through the normal config-sync path.
    SetPollInterval { poll_interval_ms: u64 },
    RequestSnapshot,
}

// ---------------------------------------------------------------------------
// Framing helpers.
// ---------------------------------------------------------------------------

pub fn encode_msg<T: serde::Serialize>(msg: &T) -> Vec<u8> {
    let mut out = serde_json::to_vec(msg).expect("IPC message must serialize");
    out.push(b'\n');
    out
}

pub fn decode_msg<T: serde::de::DeserializeOwned>(line: &str) -> Result<T, serde_json::Error> {
    serde_json::from_str(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preset_table_niceness_ordering() {
        // Lower niceness == higher priority.  The safe-ceiling order must be:
        // Realtime < Streaming < Game < Software.
        let order: Vec<i32> = Tier::ALL
            .iter()
            .map(|t| t.preset().nice)
            .collect();
        assert!(order[0] > order[1] && order[1] > order[2] && order[2] > order[3]);
        assert_eq!(Tier::Realtime.preset().nice, -12);
    }

    #[test]
    fn no_preset_may_use_kernel_realtime_scheduling() {
        // Guard: all presets stay on best-effort ionice and normal CFS via
        // nice; none may claim the realtime IO class or SCHED_FIFO/RR (there
        // is no scheduling-class field at all, by design).
        for t in Tier::ALL {
            assert_eq!(t.preset().ionice_class, IoniceClass::BestEffort);
        }
    }

    #[test]
    fn config_round_trips_through_toml() {
        let mut cfg = AppConfig {
            poll_interval_ms: Some(2500),
            rules: Default::default(),
            auto_game: None,
        };
        cfg.rules.insert(
            "vn".into(),
            Rule {
                name: "vn".into(),
                match_name: "ExampleApp.exe".into(),
                tier: Some(Tier::Software),
                nice: Some(0),
                ionice_class: Some(IoniceClass::BestEffort),
                ionice_priority: Some(6),
                delay: None,
                cgroup: None,
            },
        );
        let toml = toml_render(&cfg);
        let mut back: AppConfig = toml::from_str(&toml).unwrap();
        back.fill_rule_names();
        assert_eq!(back, cfg);
        // The rules survive the round trip with all fields intact.
        assert_eq!(back.rules["vn"].match_name, "ExampleApp.exe");
        assert_eq!(back.rules["vn"].ionice_priority, Some(6));
    }

    #[test]
    fn cgroup_limits_round_trip_and_cap_is_optional() {
        let cfg = AppConfig {
            poll_interval_ms: None,
            auto_game: None,
            rules: [(
                "cj".into(),
                Rule {
                    name: "cj".into(),
                    match_name: "cj".into(),
                    tier: Some(Tier::Game),
                    nice: None,
                    ionice_class: None,
                    ionice_priority: None,
                    delay: None,
                    cgroup: Some(CgroupLimits {
                        path: None,
                        cpu_weight: Some(900),
                        cpu_cap_percent: Some(50),
                        memory_high: Some("4G".into()),
                        memory_max: Some("6G".into()),
                        cpu_idle: None,
                    }),
                },
            )]
            .into_iter()
            .collect(),
        };
        let toml = toml_render(&cfg);
        assert!(toml.contains("cpu_cap_percent = 50"), "{toml}");
        let back: AppConfig = toml::from_str(&toml).unwrap();
        let c = back.rules["cj"].cgroup.as_ref().unwrap();
        assert_eq!(c.cpu_cap_percent, Some(50));
        assert_eq!(c.cpu_weight, Some(900));
        assert_eq!(c.memory_high.as_deref(), Some("4G"));
        assert_eq!(c.memory_max.as_deref(), Some("6G"));
        assert_eq!(c.path, None);
        // Omitting path must not serialize a bogus default.
        assert!(!toml.contains("path ="), "{toml}");
    }

    #[test]
    fn rule_from_preset_has_kebab_serialization() {
        let r = Rule::from_preset("ExampleApp.exe", Tier::Streaming);
        assert_eq!(r.match_name, "ExampleApp.exe");
        assert_eq!(r.tier, Some(Tier::Streaming));
        assert_eq!(r.nice, Some(-10));
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("\"ionice_class\":\"best-effort\""));
    }

    fn toml_render(cfg: &AppConfig) -> String {
        toml::to_string_pretty(cfg).expect("render")
    }
}