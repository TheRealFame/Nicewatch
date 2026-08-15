//! Dual-config precedence and sync.
//!
//! Two files hold the rules:
//!   * root  — `/etc/proc-priority-daemon/rules.toml`, the authoritative
//!             "live" config.
//!   * local — `~/.config/proc-priority-daemon/rules.toml`, the staging /
//!             backup copy (GUI-set rules land here first).
//!
//! Behavior:
//!   * On start and on every change of either file we compare mtimes.
//!   * If the LOCAL file is newer than the ROOT file, the local config is a
//!     "pending" update: apply it in-memory immediately (fast effect), but
//!     only overwrite the root file after a debounce window (30s by default)
//!     during which the local file must not change again.  The root file
//!     thus always lags slightly and reflects a settled state.
//!   * If the daemon cannot write to root (no permission), it logs that
//!     clearly and keeps operating from the local copy: no crash, no block.
//!   * Our own writes to the local file are recognized (mtime remembered)
//!     so the change watcher doesn't loop back on them.
//!
//! All timestamps/clock use `Instant` so the debounce logic is unit-testable
//! with synthetic time.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use nicewatch_common::{AppConfig, Tier};

use crate::config;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Root,
    Local,
    Missing,
}

/// How long the local file must stay untouched after a change before it is
/// promoted over the root file (ms).  Also used as the CLI default.
pub const DEFAULT_PROMOTE_DEBOUNCE_MS: u64 = 30_000;
/// Minimal debounce to stay between "settled state" and "typing burst".
pub const MIN_PROMOTE_DEBOUNCE: Duration = Duration::from_millis(50);

/// Tolerance for recognizing our own writes via mtime (coarse filesystems).
const SELF_WRITE_TOLERANCE: Duration = Duration::from_secs(1);

pub struct Sync {
    pub root: PathBuf,
    pub local: PathBuf,

    /// The config currently in effect (already merged/decided).
    pub active: AppConfig,
    pub active_source: Source,

    promote_debounce: Duration,
    local_write_debounce: Duration,

    // Promote machinery.
    pending_promote: bool,
    local_changed_at: Option<Instant>,
    // Mtime of the local file at the moment its change was registered.
    local_mtime_at_change: Option<SystemTime>,

    // Local-file write machinery (GUI-set rules are debounced before they
    // hit disk).
    pending_local_write: bool,
    local_write_at: Option<Instant>,

    // Mtimes of files we wrote ourselves, used to ignore self-induced
    // change events.
    self_writes: HashMap<PathBuf, SystemTime>,

    // Last observed mtime per watched file, for the poll-based fallback.
    known_mtimes: HashMap<PathBuf, Option<SystemTime>>,

    pub warnings: Vec<String>,
}

impl Sync {
    pub fn new(root: PathBuf, local: PathBuf, promote_debounce: Duration, local_write_debounce: Duration) -> Self {
        Sync {
            root,
            local,
            active: AppConfig::default(),
            active_source: Source::Missing,
            promote_debounce: promote_debounce.max(MIN_PROMOTE_DEBOUNCE),
            local_write_debounce,
            pending_promote: false,
            local_changed_at: None,
            local_mtime_at_change: None,
            pending_local_write: false,
            local_write_at: None,
            self_writes: HashMap::new(),
            known_mtimes: HashMap::new(),
            warnings: Vec::new(),
        }
    }

    /// Initial load: pick whichever config source wins on first boot.
    pub fn initial_load(&mut self, now: Instant) -> AppConfig {
        self.warnings.clear();
        let root_m = file_mtime(&self.root);
        let local_m = file_mtime(&self.local);
        match (root_m, local_m) {
            (None, None) => {
                self.active = AppConfig::default();
                self.active_source = Source::Missing;
                self.warnings.push(format!(
                    "no config files found (root: {}, local: {}) — using defaults",
                    self.root.display(),
                    self.local.display()
                ));
            }
            (Some(_), None) => {
                self.load_root();
            }
            (None, Some(_)) => {
                // Local-only: seed from local, then promote after debounce
                // (which also creates the root file when permitted).
                self.load_local(now);
                self.pending_promote = true;
            }
            (Some(_), Some(_)) => {
                if local_m > root_m {
                    self.load_local(now);
                    self.pending_promote = true;
                    log::info!(
                        "local config is newer than root — applying local as pending update"
                    );
                } else {
                    self.load_root();
                }
            }
        }
        self.snapshot_mtimes();
        self.active.clone()
    }

    /// Register a file-change event (from the notify watcher or the
    /// poll-based fallback).  Returns true when the active config changed.
    pub fn on_path_changed(&mut self, path: &Path, now: Instant) -> bool {
        if path == self.local {
            if self.is_self_write(path) {
                return false;
            }
            self.load_local(now);
            self.pending_promote = true;
            true
        } else if path == self.root {
            if self.is_self_write(path) {
                return false;
            }
            // Root edited directly: it is authoritative immediately.
            self.load_root();
            self.pending_promote = false;
            true
        } else {
            false
        }
    }

    /// Poll-based fallback that catches mtime changes even without the
    /// notify watcher (cheap metadata stat; called once per poll cycle).
    pub fn poll_mtimes(&mut self, now: Instant) -> bool {
        let mut changed = false;
        for path in [self.local.clone(), self.root.clone()] {
            let cur = file_mtime(&path);
            if self.known_mtimes.get(&path) != Some(&cur) {
                self.known_mtimes.insert(path.clone(), cur);
                if self.on_path_changed(&path, now) {
                    changed = true;
                }
            }
        }
        changed
    }

    /// Called once per loop iteration: flush debounced local writes, then
    /// promote to root once the debounce window has passed with no further
    /// local edits.  Returns true if the root file was written.
    pub fn tick(&mut self, now: Instant) -> bool {
        let mut wrote_root = false;
        if self.pending_local_write
            && self.local_write_at.map_or(false, |t| now >= t)
        {
            self.write_local();
            self.pending_local_write = false;
        }
        if self.pending_promote {
            let settled = self
                .local_changed_at
                .map_or(false, |t| now.duration_since(t) >= self.promote_debounce);
            if settled && self.local_still_unchanged() {
                wrote_root = self.promote();
            }
        }
        wrote_root
    }

    /// Insert/overwrite a rule pinning `process_name` to `tier`'s preset, and
    /// schedule a debounced write to the local config file.  This is the
    /// single persistence path for GUI selections and confirmation-window
    /// answers.  An existing cgroup block is preserved (a cap keeps binding
    /// across tier changes).
    pub fn upsert_preset_rule(&mut self, process_name: &str, tier: Tier) {
        let existing = self.active.rules.get(process_name).cloned();
        let mut rule = nicewatch_common::Rule::from_preset(process_name, tier);
        rule.cgroup = existing.and_then(|r| r.cgroup);
        self.active.rules.insert(process_name.to_string(), rule);
        self.active_source = Source::Local;
        self.schedule_local_write();
    }

    /// Set (Some) or remove (None) the hard CPU cap (`cgroup.cpu_cap_percent`)
    /// on the rule for `process_name`, keeping its tier/nice/ionice.  Creates
    /// a software-tier default rule when none exists yet.
    pub fn upsert_cap_rule(&mut self, process_name: &str, pct: Option<u32>) {
        if let Some(existing) = self.active.rules.get_mut(process_name) {
            match &mut existing.cgroup {
                Some(cg) => {
                    if let Some(p) = pct {
                        cg.cpu_cap_percent = Some(p);
                    } else {
                        cg.cpu_cap_percent = None;
                        // Drop an empty cgroup block entirely.
                        if cg.path.is_none()
                            && cg.cpu_weight.is_none()
                            && cg.memory_high.is_none()
                            && cg.memory_max.is_none()
                        {
                            existing.cgroup = None;
                        }
                    }
                }
                None => {
                    if let Some(p) = pct {
                        existing.cgroup = Some(nicewatch_common::CgroupLimits {
                            path: None,
                            cpu_weight: None,
                            cpu_cap_percent: Some(p),
                            memory_high: None,
                            memory_max: None,
                            cpu_idle: None,
                        });
                    }
                }
            }
        } else {
            let mut rule = nicewatch_common::Rule::from_preset(process_name, Tier::Software);
            if let Some(p) = pct {
                rule.cgroup = Some(nicewatch_common::CgroupLimits {
                    path: None,
                    cpu_weight: None,
                    cpu_cap_percent: Some(p),
                    memory_high: None,
                    memory_max: None,
                    cpu_idle: None,
                });
            }
            self.active.rules.insert(process_name.to_string(), rule);
        }
        self.active_source = Source::Local;
        self.schedule_local_write();
    }

    /// Set the poll interval in the active config and persist it via the
    /// normal debounced local write (promotes to root after the settle
    /// window).  Callers validate/clamp the value.
    pub fn set_poll_interval(&mut self, poll_interval_ms: u64) {
        self.active.poll_interval_ms = Some(poll_interval_ms);
        self.active_source = Source::Local;
        self.schedule_local_write();
    }

    fn schedule_local_write(&mut self) {
        self.pending_local_write = true;
        // Debounce collapses rapid-fire preset clicks into a single write.
        self.local_write_at = Some(Instant::now() + self.local_write_debounce);
    }

    fn write_local(&mut self) {
        if let Some(parent) = self.local.parent() {
            if let Err(e) = fs::create_dir_all(parent) {
                self.warnings
                    .push(format!("cannot create {}: {e}", parent.display()));
            }
        }
        let rendered = config::render(&self.active);
        match atomic_write(&self.local, rendered.as_bytes()) {
            Ok(()) => {
                log::info!("wrote local config: {}", self.local.display());
                // This write itself is a "change" that starts the promote
                // clock once it settles on disk.
                if let Some(m) = file_mtime(&self.local) {
                    self.self_writes.insert(self.local.clone(), m);
                }
                self.local_changed_at = Some(Instant::now());
                self.local_mtime_at_change = file_mtime(&self.local);
                self.pending_promote = true;
            }
            Err(e) => {
                self.warnings
                    .push(format!("cannot write local config {}: {e}", self.local.display()));
            }
        }
    }

    /// Promote the local content over the root file (atomic rename).
    fn promote(&mut self) -> bool {
        let bytes = match fs::read(&self.local) {
            Ok(b) => b,
            Err(e) => {
                self.warnings
                    .push(format!("cannot read local config for promotion: {e}"));
                return false;
            }
        };
        match atomic_write(&self.root, &bytes) {
            Ok(()) => {
                log::info!(
                    "promoted local config over root ({}) after settle window",
                    self.root.display()
                );
                if let Some(m) = file_mtime(&self.root) {
                    self.self_writes.insert(self.root.clone(), m);
                }
                self.pending_promote = false;
                self.active_source = Source::Root;
                true
            }
            Err(e) => {
                // The daemon must keep running read-only-ish from local.
                self.warnings.push(format!(
                    "CANNOT WRITE ROOT CONFIG {}: {e} — continuing with local copy only (no sudo required, changes take effect locally but are not promoted to /etc)",
                    self.root.display()
                ));
                // Don't retry every tick; retry only after the next local edit.
                self.pending_promote = false;
                false
            }
        }
    }

    fn load_root(&mut self) {
        match load_file(&self.root, &mut self.warnings, "root") {
            Some(cfg) => {
                self.active = cfg;
                self.active_source = Source::Root;
            }
            None => {
                // Parse failure: keep the previous active config.
                self.warnings.push(format!(
                    "root config {} failed to parse — keeping previously loaded config",
                    self.root.display()
                ));
            }
        }
    }

    fn load_local(&mut self, now: Instant) {
        match load_file(&self.local, &mut self.warnings, "local") {
            Some(cfg) => {
                self.active = cfg;
                self.active_source = Source::Local;
                self.local_changed_at = Some(now);
                self.local_mtime_at_change = file_mtime(&self.local);
            }
            None => {}
        }
    }

    fn local_still_unchanged(&self) -> bool {
        match (file_mtime(&self.local), self.local_mtime_at_change) {
            (Some(cur), Some(at)) => cur == at,
            _ => false,
        }
    }

    fn is_self_write(&self, path: &Path) -> bool {
        let Some(recorded) = self.self_writes.get(path) else {
            return false;
        };
        match file_mtime(path) {
            Some(cur) => {
                let within_tolerance =
                    cur.duration_since(*recorded).map(|d| d < SELF_WRITE_TOLERANCE).unwrap_or(false);
                within_tolerance || cur == *recorded
            }
            None => false,
        }
    }

    fn snapshot_mtimes(&mut self) {
        for path in [self.local.clone(), self.root.clone()] {
            self.known_mtimes.insert(path.clone(), file_mtime(&path));
        }
    }
}

fn file_mtime(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).and_then(|m| m.modified()).ok()
}

fn load_file(path: &Path, warnings: &mut Vec<String>, kind: &str) -> Option<AppConfig> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            warnings.push(format!("cannot read {kind} config {}: {e}", path.display()));
            return None;
        }
    };
    match config::parse(&text) {
        Ok(cfg) => {
            log::debug!("loaded {kind} config from {}", path.display());
            Some(cfg)
        }
        Err(e) => {
            warnings.push(format!("{kind} config {} invalid: {e}", path.display()));
            None
        }
    }
}

/// Atomic write: temp file + rename.  Fine for a config file that's at most
/// a few KB.
fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or(Path::new("."));
    if !parent.exists() {
        fs::create_dir_all(parent)?;
    }
    let tmp = parent.join(format!("{}.tmp", file_name_of(path)));
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

fn file_name_of(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "rules.toml".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Short debounce for tests: ctor takes it explicitly, so we can use
    /// 100ms windows and sleep 120ms between writes instead of faking time.
    fn setup(name: &str) -> (TempDir, PathBuf, PathBuf, Sync) {
        let td = TempDir::new().unwrap();
        // nb: the Sync constructor wants root/local paths inside `td`.
        let root = td.path().join("root").join("rules.toml");
        let local = td.path().join("local").join("rules.toml");
        let sync = Sync::new(
            root.clone(),
            local.clone(),
            Duration::from_millis(100),
            Duration::from_millis(5),
        );
        let _ = name;
        (td, root, local, sync)
    }

    fn write(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn config_with_rule(name: &str, m: &str) -> String {
        format!(
            r#"[rules.{}]
match = "{}"
tier = "software"
nice = 0
"#,
            name, m
        )
    }

    #[test]
    fn new_local_wins_over_root() {
        let (_td, root, local, mut sync) = setup("1");
        write(&root, &config_with_rule("r", "rootproc"));
        // sleep so the local mtime is strictly newer (ext4 has ns mtime, but
        // be safe on coarse filesystems by staging an old root instead).
        std::thread::sleep(Duration::from_millis(20));
        write(&local, &config_with_rule("l", "localproc"));

        let active = sync.initial_load(Instant::now());
        assert_eq!(sync.active_source, Source::Local);
        assert!(active.rules.contains_key("l"));
        assert!(!active.rules.contains_key("r"));

        // After the settle window, local content is promoted to root.
        std::thread::sleep(Duration::from_millis(120));
        let promoted = sync.tick(Instant::now());
        assert!(promoted);
        assert_eq!(fs::read_to_string(&root).unwrap(), config_with_rule("l", "localproc"));
    }

    #[test]
    fn root_newer_stays_authoritative() {
        let (_td, root, local, mut sync) = setup("2");
        write(&root, &config_with_rule("r", "rootproc"));
        std::thread::sleep(Duration::from_millis(20));
        write(&local, &config_with_rule("l", "localproc"));
        // Make root newer by touching it after local.
        std::thread::sleep(Duration::from_millis(20));
        write(&root, &config_with_rule("r2", "rootproc2"));

        sync.initial_load(Instant::now());
        assert_eq!(sync.active_source, Source::Root);
        assert!(sync.active.rules.contains_key("r2"));
        assert!(!sync.active.rules.contains_key("l"));
    }

    #[test]
    fn equal_mtimes_prefer_root() {
        let (_td, root, local, mut sync) = setup("3");
        write(&root, &config_with_rule("r", "rootproc"));
        write(&local, &config_with_rule("l", "localproc"));
        // Force equal mtimes.
        let t = file_mtime(&local).unwrap();
        let _ = fs::File::open(&root).unwrap().set_modified(t);
        sync.initial_load(Instant::now());
        assert_eq!(sync.active_source, Source::Root);
    }

    #[test]
    fn local_only_seeds_and_promotes() {
        let (_td, root, local, mut sync) = setup("4");
        write(&local, &config_with_rule("l", "localproc"));
        sync.initial_load(Instant::now());
        assert_eq!(sync.active_source, Source::Local);
        assert!(sync.active.rules.contains_key("l"));
        std::thread::sleep(Duration::from_millis(120));
        assert!(sync.tick(Instant::now()));
        assert_eq!(fs::read_to_string(&root).unwrap(), config_with_rule("l", "localproc"));
    }

    #[test]
    fn nothing_present_starts_with_defaults() {
        let (_td, _root, _local, mut sync) = setup("5");
        let active = sync.initial_load(Instant::now());
        assert!(active.rules.is_empty());
        assert_eq!(sync.active_source, Source::Missing);
        assert!(!sync.warnings.is_empty());
    }

    #[test]
    fn promote_failure_keeps_operating_from_local() {
        let (_td, _root, local, mut sync) = setup("6");
        // Root target sits under *a file*, so create_dir_all fails even when
        // running as root (parent exists but is not a directory).
        let blocker = local.with_extension("blocked");
        std::fs::create_dir_all(local.parent().unwrap()).unwrap();
        std::fs::write(&blocker, "x").unwrap();
        sync.root = blocker.join("rules.toml");

        write(&local, &config_with_rule("l", "localproc"));
        sync.initial_load(Instant::now());
        assert_eq!(sync.active_source, Source::Local);
        assert!(sync.active.rules.contains_key("l"));

        std::thread::sleep(Duration::from_millis(120));
        let promoted = sync.tick(Instant::now());
        assert!(!promoted);
        // Warning explains we keep going local-only.
        assert!(sync.warnings.iter().any(|w| w.contains("continuing with local")));
        // Still fully operational.
        assert!(sync.active.rules.contains_key("l"));
    }

    #[test]
    fn self_write_does_not_loop_back() {
        let (_td, _root, local, mut sync) = setup("7");
        write(&local, &config_with_rule("l", "localproc"));
        sync.initial_load(Instant::now());

        // A write *we* made must not register as an external change.
        sync.active.rules.insert(
            "tmp".into(),
            nicewatch_common::Rule::from_preset("tmpproc", Tier::Game),
        );
        sync.write_local();
        let mtime = file_mtime(&local).unwrap();
        sync.self_writes.insert(local.clone(), mtime);

        let changed = sync.on_path_changed(&local, Instant::now());
        assert!(!changed, "our own write must be ignored");
    }

    #[test]
    fn rapid_local_edits_reset_the_promote_clock() {
        let (_td, root, local, mut sync) = setup("8");
        write(&local, &config_with_rule("v1", "first"));
        sync.initial_load(Instant::now());
        assert!(!sync.tick(Instant::now()));

        // Edit 2 within the window.
        std::thread::sleep(Duration::from_millis(60));
        write(&local, &config_with_rule("v2", "second"));
        assert!(sync.on_path_changed(&local, Instant::now()));

        // At 60ms the second edit's own debounce is not settled yet.
        std::thread::sleep(Duration::from_millis(30));
        assert!(!sync.tick(Instant::now()));

        std::thread::sleep(Duration::from_millis(80));
        assert!(sync.tick(Instant::now()));
        assert_eq!(
            fs::read_to_string(&root).unwrap(),
            config_with_rule("v2", "second")
        );
    }

    #[test]
    fn upsert_preset_rule_is_debounced_then_lands_in_local() {
        let (_td, _root, local, mut sync) = setup("9");
        write(&local, &config_with_rule("x", "x"));
        sync.initial_load(Instant::now());

        sync.upsert_preset_rule("VNyan.exe", Tier::Software);
        sync.tick(Instant::now()); // before debounce -> nothing written yet
        let text_now = fs::read_to_string(&local).unwrap();
        assert!(!text_now.contains("VNyan.exe"));

        std::thread::sleep(Duration::from_millis(20));
        sync.tick(Instant::now());
        let text = fs::read_to_string(&local).unwrap();
        // Rule serialized with kebab tier name.
        assert!(text.contains("VNyan.exe"));
        assert!(text.contains("tier = \"software\""));
        assert!(text.contains("nice = 0"));

        // And the same rule is keyed by the process name.
        let reparsed = config::parse(&text).unwrap();
        assert!(reparsed.rules.contains_key("VNyan.exe"));
    }
}