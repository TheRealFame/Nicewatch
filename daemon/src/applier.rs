//! Kernel-interaction layer: renice, ioprio, optional cgroup v2 limits.
//!
//! Everything here is best-effort: EPERM (running the daemon without root /
//! CAP_SYS_NICE, or operating on another user's process) is a logged warning
//! per pid, never a crash.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::proc_scan;
use nicewatch_common::{CgroupLimits, IoniceClass};

/// Real-time IO class is allowed in hand-written rules but never in a preset;
/// see the comment in `common/src/lib.rs` for why SCHED_FIFO/RR (and genuine
/// "unrestricted" priority) are intentionally excluded from this project.
const IOPRIO_CLASS_NONE: u64 = 0;
const IOPRIO_CLASS_REALTIME: u64 = 1;
const IOPRIO_CLASS_BEST_EFFORT: u64 = 2;
const IOPRIO_CLASS_IDLE: u64 = 3;
const IOPRIO_WHO_PROCESS: libc::c_int = 1;
const IOPRIO_CLASS_SHIFT: u64 = 13;

/// cgroup v2 `cpu.max` scheduling period: 100 ms.
const CGROUP_PERIOD_US: u64 = 100_000;
/// Clamp for `cpu_cap_percent` (percent of one core; 3200% = 32 cores).
const MAX_CAP_PERCENT: u32 = 3200;

/// Well-known base for cgroups the daemon itself owns (root daemons only;
/// user daemons use their delegated session subtree instead).
const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// A pid we have moved into a managed cgroup.
#[derive(Debug, Clone)]
struct Enrolled {
    /// Absolute path of the managed cgroup dir.
    dir: PathBuf,
    /// The cgroup the pid came from, so it can be moved back when the rule
    /// stops applying (or the daemon shuts down).
    origin: PathBuf,
    /// Limits last written to `dir` (per-rule dirs are single-rule, so one
    /// signature per dir is enough).
    limits: Option<CgroupLimits>,
}

/// On-disk (de)serialization of the enrolled table, so a SIGKILLed daemon's
/// orphaned pids can be moved back to their origin by the next daemon run.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct EnrolledState {
    /// pid -> (managed dir, origin cgroup) as absolute paths.
    entries: std::collections::BTreeMap<u32, (String, String)>,
}

impl EnrolledState {
    fn from_manager(m: &CgroupManager) -> Self {
        EnrolledState {
            entries: m
                .enrolled
                .iter()
                .map(|(pid, e)| {
                    (
                        *pid,
                        (e.dir.display().to_string(), e.origin.display().to_string()),
                    )
                })
                .collect(),
        }
    }
}

/// Owns the managed cgroup v2 subtree: discovers a writable base, creates
/// per-rule child dirs, moves pids in, writes limits, and moves pids back
/// when they stop matching.
pub struct CgroupManager {
    base: Option<PathBuf>,
    enrolled: HashMap<u32, Enrolled>,
    created_dirs: HashSet<PathBuf>,
    warned: HashSet<String>,
    /// Set when the base's controller set had to be re-asserted (systemd
    /// dropped `cpu` and the kernel reset every child's cpu.weight back to
    /// 100).  Makes the next reconcile rewrite all limits once.
    controllers_reasserted: bool,
    /// JSON file mirroring `enrolled`, so a SIGKILLed daemon's orphans can
    /// be repatriated by the next run.
    state_path: Option<PathBuf>,
}

impl CgroupManager {
    /// Discover the writable cgroup base and prepare the manager.  Best
    /// effort: `None` means "cgroup limiting unavailable" (no permission,
    /// not cgroup v2) and every operation degrades to a no-op warning.
    pub fn new() -> Self {
        let base = discover_cgroup_base();
        if base.is_none() {
            log::warn!(
                "cgroup v2 limiting unavailable: no writable base under {CGROUP_ROOT} \
                 (run the daemon as your user inside your session, or as root) — \
                 cpu_cap_percent / cpu_weight / memory_high rules will be skipped"
            );
        } else {
            log::info!("cgroup v2 base: {}", base.as_ref().unwrap().display());
        }
        let mut m = CgroupManager {
            base,
            enrolled: HashMap::new(),
            created_dirs: HashSet::new(),
            warned: HashSet::new(),
            controllers_reasserted: false,
            state_path: Some(nicewatch_common::runtime_dir().join("nicewatch-cgroups.json")),
        };
        m.recover_orphans();
        m.ensure_controllers();
        m
    }

    /// Make sure the controllers we write (cpu, memory; pids is free) are
    /// delegated at the base's subtree_control.
    ///
    /// The probe that picks the base requires cpu to be enabled *at discovery
    /// time*, but systemd can rebuild the session slices afterwards (session
    /// refresh, daemon-reexec) and re-create `app.slice` with only memory+pids
    /// delegated.  The daemon then keeps writing into a base where child
    /// cgroups have no `cpu.weight` at all — every write fails with EPERM and
    /// the whole CPU half of the app silently dies.  Re-asserting the
    /// controllers is idempotent and cheap, so we do it on startup and once
    /// per poll to self-heal within one cycle.
    pub fn ensure_controllers(&mut self) {
        let Some(base) = &self.base else { return };
        let path = base.join("cgroup.subtree_control");
        let had_cpu = std::fs::read_to_string(&path)
            .unwrap_or_default()
            .split_whitespace()
            .any(|c| c == "cpu");
        if let Err(e) = std::fs::write(&path, "+cpu +memory +pids") {
            let key = format!("subtree:{path:?}");
            if self.warned.insert(key) {
                log::warn!(
                    "cannot enable controllers at {}: {e} — cpu.weight / memory.high \
                     writes into managed cgroups will fail",
                    path.display()
                );
            }
            return;
        }
        if !had_cpu {
            // systemd dropped `cpu` and the kernel reset every child's
            // cpu.weight (and other controller state) back to the defaults;
            // the daemon only rewrites limits on *change*, so nothing would
            // restore them.  Force a full rewrite on the next reconcile.
            self.controllers_reasserted = true;
        }
    }

    /// The reconcile has finished rewriting every managed dir after a
    /// controller re-assertion — subsequent polls can diff normally again.
    pub fn clear_controllers_reasserted(&mut self) {
        self.controllers_reasserted = false;
    }

    /// Move any pids our previous run left behind back to their origin
    /// (crash recovery): load the persisted table, unbind every entry, and
    /// clear the file.
    fn recover_orphans(&mut self) {
        let Some(path) = self.state_path.clone() else {
            return;
        };
        let state = match std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<EnrolledState>(&s).ok())
        {
            Some(s) => s,
            None => {
                // Stale/corrupt file: nothing to do, drop it.
                let _ = std::fs::remove_file(&path);
                return;
            }
        };
        if state.entries.is_empty() {
            return;
        }
        log::info!(
            "crash recovery: repatriating {} pid(s) left in managed cgroups",
            state.entries.len()
        );
        let pids: Vec<u32> = state.entries.keys().copied().collect();
        for pid in pids {
            let (dir, origin) = &state.entries[&pid];
            // Only touch pids that still exist and are still in OUR managed
            // dir (a reparented/dead pid is not ours to move).
            let in_dir = proc_cgroup_of(pid)
                .map(|rel| {
                    let mut p = PathBuf::from(CGROUP_ROOT);
                    p.push(rel.trim_start_matches('/'));
                    p == Path::new(dir)
                })
                .unwrap_or(false);
            if in_dir {
                if let Err(msg) = move_pid(pid, Path::new(origin)) {
                    log::warn!("crash recovery could not repatriate pid {pid}: {msg}");
                }
                let p = PathBuf::from(dir);
                self.prune_dir(&p);
            }
        }
        let _ = std::fs::remove_file(&path);
    }

    fn persist(&self) {
        let Some(path) = &self.state_path else {
            return;
        };
        let state = EnrolledState::from_manager(self);
        let bytes = match serde_json::to_vec_pretty(&state) {
            Ok(b) => b,
            Err(e) => {
                log::warn!("cannot serialize cgroup state: {e}");
                return;
            }
        };
        match std::fs::write(path, bytes) {
            Ok(()) => {}
            Err(e) => log::warn!("cannot write cgroup state {}: {e}", path.display()),
        }
    }

    /// Origin persisted by a previous daemon run, if any.
    fn persisted_origin(&self, pid: u32) -> Option<PathBuf> {
        let path = self.state_path.as_ref()?;
        let state = std::fs::read_to_string(path).ok()?;
        let state: EnrolledState = serde_json::from_str(&state).ok()?;
        state.entries.get(&pid).map(|(_, origin)| PathBuf::from(origin))
    }

    pub fn base(&self) -> Option<&Path> {
        self.base.as_deref()
    }

    /// True when the pid currently lives in one of our managed cgroups.
    pub fn is_capped(&self, pid: u32) -> bool {
        self.enrolled.contains_key(&pid)
    }

    /// Number of managed dirs / enrolled pids (for logs).
    pub fn stats(&self) -> (usize, usize) {
        (self.created_dirs.len(), self.enrolled.len())
    }

    /// Reconcile one process against its desired limits.
    ///
    ///   * `Some(limits)` → make sure the pid sits in a managed cgroup with
    ///     those limits written (creates the dir, moves the pid in once).
    ///   * `None` → the rule no longer applies: move the pid back to its
    ///     origin cgroup and remove the now-empty dir.
    pub fn sync_pid(&mut self, pid: u32, key: &str, limits: Option<&CgroupLimits>) {
        let base = match &self.base {
            Some(b) => b.clone(),
            None => {
                if let Some(lims) = limits {
                    let name = format!("{key}:{lims:?}");
                    if self.warned.insert(name) {
                        log::warn!("cgroup rule for `{key}` skipped (no cgroup base)");
                    }
                }
                return;
            }
        };

        match limits {
            Some(lims) => {
                // Managed dirs are prefixed `nw-` so they can never collide
                // with systemd's own unit dirs under the same base (e.g. a
                // `claude-desktop` rule vs the `app.slice/claude-desktop`
                // unit dir systemd owns — systemd would overwrite our
                // cpu.weight and the process's origin bookkeeping would
                // fight over one directory).
                let dir = lims
                    .path
                    .as_ref()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| base.join(format!("nw-{}", sanitize_dir(key))));

                let already = self.enrolled.get(&pid);
                if let Some(en) = already {
                    if en.dir == dir {
                        // Same target: only rewrite when limits changed — or
                        // after a controller re-assertion reset them (kernel
                        // default weights) while we weren't looking.
                        if en.limits != Some(lims.clone()) || self.controllers_reasserted {
                            if let Err(msg) = apply_limits(&dir, lims) {
                                self.warn_once(pid, &msg);
                            }
                            if let Some(en) = self.enrolled.get_mut(&pid) {
                                en.limits = Some(lims.clone());
                            }
                        }
                        return;
                    }
                    // Different dir: move back first, then re-enroll.
                    self.unenroll(pid);
                }

                // Record where the pid lives NOW (before the move), so we can
                // put it back later.  Reading `/proc/<pid>/cgroup` after the
                // move would capture our own managed dir as its origin.  If
                // the pid still sits in one of OUR dirs (orphan of a crashed
                // run), the parsed origin is meaningless — fall back to the
                // persisted one.
                let mut origin = self.origin_of(pid, &dir);
                if origin != base && origin.starts_with(&base) {
                    let managed = origin != base
                        && (self.created_dirs.contains(&origin) || self.enrolled.values().any(|e| e.dir == origin));
                    if managed {
                        match self.persisted_origin(pid) {
                            Some(saved) => origin = saved,
                            None => {
                                let name = format!("{pid}:orphan-origin");
                                if self.warned.insert(name) {
                                    log::warn!(
                                        "pid {pid} was already in a managed cgroup with no \
                                         persisted origin; its true origin is unknown and it \
                                         cannot be restored after the limit stops applying"
                                    );
                                }
                            }
                        }
                    }
                }
                if let Err(msg) = enroll(pid, &dir, lims) {
                    self.warn_once(pid, &msg);
                    return;
                }
                self.enrolled.insert(
                    pid,
                    Enrolled {
                        dir: dir.clone(),
                        origin,
                        limits: Some(lims.clone()),
                    },
                );
                self.created_dirs.insert(dir);
                self.persist();
            }
            None => self.unenroll(pid),
        }
    }

    /// Forget a pid that has exited (its kernel entry vanished; nothing to
    /// move back, but the managed dir may now be empty).
    pub fn forget(&mut self, pid: u32) {
        if let Some(en) = self.enrolled.remove(&pid) {
            self.prune_dir(&en.dir);
            self.persist();
        }
    }

    /// Sweep the managed cgroups for pids we did not move in.
    ///
    /// These are crash remnants that landed between the last state persist
    /// and the SIGKILL (so `recover_orphans` never saw them), or pids
    /// stranded when systemd rebuilt the session slices.  Their true origin
    /// is not recorded anywhere, so the best we can do is park them in a
    /// leaf dir with no limits written (`_strays`) — the base itself is an
    /// internal cgroup and cannot host processes (EBUSY).  Runs after the
    /// per-pid reconcile, so anything a rule legitimately enrolled this
    /// cycle is already in `enrolled` and skipped.
    pub fn sweep_strays(&mut self) {
        let Some(base) = &self.base else { return };
        let base = base.clone();
        let stray_dir = base.join("_strays");
        let managed: Vec<PathBuf> = self.created_dirs.iter().cloned().collect();
        for dir in managed {
            let Ok(procs) = std::fs::read_to_string(dir.join("cgroup.procs")) else {
                continue;
            };
            for line in procs.lines() {
                let Ok(pid) = line.trim().parse::<u32>() else {
                    continue;
                };
                if self.enrolled.contains_key(&pid) {
                    continue;
                }
                // Double-check the pid still lives in this dir right now.
                let still_here = proc_cgroup_of(pid)
                    .map(|rel| {
                        let mut p = PathBuf::from(CGROUP_ROOT);
                        p.push(rel.trim_start_matches('/'));
                        p == dir
                    })
                    .unwrap_or(false);
                if !still_here {
                    continue;
                }
                // Never relocate compositor/desktop-critical processes, even
                // if they somehow sit in a managed dir (leftover from a
                // pre-exclusion run).  Moving the compositor between cgroups
                // is exactly how input/cursor state gets stuck.
                if proc_scan::comm_of(pid)
                    .map(|comm| crate::game_detect::is_de_critical(&comm))
                    .unwrap_or(false)
                {
                    continue;
                }
                let _ = std::fs::create_dir_all(&stray_dir);
                if let Err(msg) = move_pid(pid, &stray_dir) {
                    self.warn_once(pid, &msg);
                }
            }
            self.prune_dir(&dir);
            self.prune_dir(&stray_dir);
        }
    }

    /// Move every enrolled pid back to its origin and remove managed dirs.
    pub fn shutdown(&mut self) {
        let pids: Vec<u32> = self.enrolled.keys().copied().collect();
        for pid in pids {
            self.unenroll(pid);
        }
        // Clean slate: no orphans to recover next run.
        if let Some(path) = &self.state_path {
            let _ = std::fs::remove_file(path);
        }
    }

    fn origin_of(&self, pid: u32, _dir: &Path) -> PathBuf {
        let mut origin = PathBuf::from(CGROUP_ROOT);
        if let Some(rel) = proc_cgroup_of(pid) {
            origin.push(rel.trim_start_matches('/'));
        }
        origin
    }

    fn warn_once(&mut self, pid: u32, msg: &str) {
        if self.warned.insert(format!("{pid}:{msg}")) {
            log::warn!("cgroup for pid {pid} failed ({msg})");
        }
    }

    fn unenroll(&mut self, pid: u32) {
        let Some(en) = self.enrolled.remove(&pid) else {
            return;
        };
        // Move the pid back to where it came from.  Best effort: if the
        // origin is unreachable (other user's process, gone cgroup) we can't
        // restore it — log once per dir.
        if let Err(msg) = move_pid(pid, &en.origin) {
            self.warn_once(pid, &msg);
        }
        self.prune_dir(&en.dir);
        self.persist();
    }

    /// Remove a managed dir once it holds no pids and is no longer wanted.
    fn prune_dir(&mut self, dir: &Path) {
        // Writing an empty string to cgroup.procs is a no-op on the real
        // cgroupfs (and clears the file on a plain test fs).
        let _ = std::fs::write(dir.join("cgroup.procs"), "");
        let empty = std::fs::read_to_string(dir.join("cgroup.procs"))
            .map(|s| s.trim().is_empty())
            .unwrap_or(false);
        if !empty {
            return;
        }
        // Real cgroupfs: `rmdir` works with virtual files.  Fall back to
        // recursive removal for plain test filesystems.
        if std::fs::remove_dir(dir).is_ok() || std::fs::remove_dir_all(dir).is_ok() {
            log::debug!("removed empty managed cgroup {}", dir.display());
            self.created_dirs.remove(dir);
        }
    }
}

impl Default for CgroupManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Best-effort: (re)move a pid into a cgroup by writing it to
/// `dir/cgroup.procs`.  cgroup v2 semantics: this is a move, so the pid
/// leaves its current cgroup — and only works across dirs we may write.
fn move_pid(pid: u32, dir: &Path) -> Result<(), String> {
    std::fs::write(dir.join("cgroup.procs"), pid.to_string())
        .map_err(|e| format!("{}: {e}", dir.join("cgroup.procs").display()))
}

/// Create the cgroup dir if missing, write the requested limits, then move
/// the pid into it (in that order, so the pid never runs unbound while we
/// set things up).
fn enroll(pid: u32, dir: &Path, limits: &CgroupLimits) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    apply_limits(dir, limits)?;
    move_pid(pid, dir)
}

fn apply_limits(dir: &Path, limits: &CgroupLimits) -> Result<(), String> {
    if let Some(pct) = limits.cpu_cap_percent {
        let pct = pct.min(MAX_CAP_PERCENT).max(1);
        let quota = (pct as u64) * (CGROUP_PERIOD_US / 100);
        write_cgroup_file(&dir.join("cpu.max"), &format!("{quota} {CGROUP_PERIOD_US}"))?;
    }
    // cpu.idle: when the group is idle, the kernel fixes its effective weight
    // to the minimum and REJECTS cpu.weight writes (EINVAL) — idle classes
    // only run when nothing else wants the CPU, so the weight is moot.  Write
    // the flag first; skip the weight when idle to avoid EINVAL noise.
    if let Some(idle) = limits.cpu_idle {
        write_cgroup_file(&dir.join("cpu.idle"), if idle { "1" } else { "0" })?;
        if idle {
            // cpu.weight is kernel-pinned for idle groups; memory_high still
            // applies, so fall through to it below.
        }
    } else if let Some(w) = limits.cpu_weight {
        write_cgroup_file(&dir.join("cpu.weight"), &w.to_string())?;
    }
    if let Some(high) = &limits.memory_high {
        write_cgroup_file(&dir.join("memory.high"), high)?;
    }
    if let Some(max) = &limits.memory_max {
        write_cgroup_file(&dir.join("memory.max"), max)?;
    }
    Ok(())
}

/// Resolve `/sys/fs/cgroup/<rel>` from a pid's `cgroup.procs`-style line
/// (`/proc/<pid>/cgroup`).
fn proc_cgroup_of(pid: u32) -> Option<String> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    let line = text.lines().find(|l| l.starts_with("0::"))?;
    Some(line["0::".len()..].to_string())
}

/// Find a writable base directory for managed cgroups.
///
/// Walk up from the daemon's own cgroup and return the first directory we
/// can actually create a child in.  For a user daemon that is the daemon's
/// enclosing slice (`app.slice`); for a root daemon it lands wherever the
/// daemon's unit lives (writable by root).
///
/// Boundaries are forbidden as bases: the user session root
/// (`user@<uid>.service`), its slices, and `session.slice`.  Managed dirs
/// created there would be *siblings* of the session's own slices and, with
/// their cpu weights (up to 1000), would starve the interactive session
/// (audio stack included) out of the CPU share under contention — the
/// daemon's budget must compete inside `app.slice`, never against
/// `session.slice`.
fn discover_cgroup_base() -> Option<PathBuf> {
    // Fast path: try to create a probe dir under the daemon's own cgroup.
    let mut own = PathBuf::from(CGROUP_ROOT);
    if let Some(rel) = proc_cgroup_of(std::process::id()) {
        // `proc_cgroup_of` yields a path starting with '/'; joining it via
        // push would *replace* the base (PathBuf semantics for absolute
        // args), so strip the leading slash into a relative join.
        own.push(rel.trim_start_matches('/'));
    }
    // Try the daemon's own cgroup first (deepest, most specific), then walk
    // up toward the root.  The first writable ancestor is the base.
    let mut candidates: Vec<PathBuf> = Vec::new();
    let mut c = own.clone();
    loop {
        candidates.push(c.clone());
        if !c.pop() {
            break;
        }
    }
    // A write failure on the daemon's OWN cgroup is expected: systemd unit
    // cgroups have empty subtree_control, so their children expose no
    // controller files at all — walk up.  A failing candidate above that is
    // just transient (systemd re-computes slice subtree_control on unit
    // churn and can drop `cpu`), so retry the whole walk a few times before
    // giving up — the walk stops at the first forbidden boundary, never
    // inside one.  The probe writes `memory.high` rather than `cpu.max`:
    // memory is delegated to the session unconditionally, so the probe
    // tests writability, not the current (changing) controller set; the
    // controllers themselves are asserted by `ensure_controllers` right
    // after discovery.
    for _ in 0..3 {
        for (i, cand) in candidates.iter().enumerate() {
            if is_forbidden_base(cand) {
                return None; // managed dirs must never sit beside the slices
            }
            let mut probed = false;
            for _ in 0..5 {
                let probe = cand.join(format!(".nw-probe-{}", std::process::id()));
                if std::fs::create_dir(&probe).is_err() {
                    break; // leaf cgroup with processes — walk up
                }
                let ok = std::fs::write(probe.join("memory.high"), "max").is_ok();
                let _ = std::fs::remove_dir(&probe);
                if ok {
                    return Some(cand.clone());
                }
                probed = true;
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            if !probed {
                // create_dir itself failed; this candidate hosts processes
                // (leaf) or is unwritable — either way, keep walking up.
                continue;
            }
            if i == 0 {
                // The daemon's own cgroup: systemd unit cgroups carry no
                // controllers, so child dirs expose no controller files by
                // design.  Expected — walk up to the enclosing slice.
                continue;
            }
            // A higher candidate exists but won't take any write — do not
            // walk further up into a boundary; limiting just stays off.
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
    None
}

/// The managed-subtree base must never be a session boundary: cgroups whose
/// children are *slices* (or the cgroup root itself).  Managed dirs created
/// there would compete directly against the session's slices for CPU shares.
fn is_forbidden_base(cand: &std::path::Path) -> bool {
    let Some(name) = cand.file_name().and_then(|n| n.to_str()) else {
        return true; // the filesystem root — never write there
    };
    if name == "session.slice" || name == "user.slice" {
        return true;
    }
    if name.starts_with("user@") && name.ends_with(".service") {
        return true;
    }
    if name.starts_with("user-") && name.ends_with(".slice") {
        return true;
    }
    cand == std::path::Path::new(CGROUP_ROOT)
}

/// Rule name -> filesystem-safe cgroup dir name.
fn sanitize_dir(key: &str) -> String {
    key.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CurrentIoprio {
    pub class: IoniceClass,
    pub priority: u8,
}

/// Rate-limiter for per-pid syscall failures (log once, then stay quiet
/// until the pid is gone and comes back).
#[derive(Default)]
pub struct FailureTracker {
    warned: HashSet<u32>,
}

impl FailureTracker {
    /// Returns true when this failure should actually be logged.
    pub fn first_failure(&mut self, pid: u32) -> bool {
        self.warned.insert(pid)
    }

    pub fn forget(&mut self, pid: u32) {
        self.warned.remove(&pid);
    }
}

/// True when we hold CAP_SYS_NICE-like privileges (root in practice).  Cached
/// once: the daemon's privileges never change mid-run.
pub fn can_apply_negative_nice() -> bool {
    static ROOT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ROOT.get_or_init(|| unsafe { libc::geteuid() == 0 })
}

/// The daemon's own effective uid — other users' processes are untouchable
/// for a non-root daemon, so we need to know who "we" are.
pub fn our_euid() -> u32 {
    unsafe { libc::geteuid() }
}

/// Is this process running under a real-time scheduling class (SCHED_FIFO or
/// SCHED_RR)?
///
/// RT tasks reject nice and ionice changes with EPERM by design (KWin's
/// compositor, for example, runs SCHED_RR on Plasma 6), so any scheduling
/// write to them is pointless — and would otherwise warn once per pid
/// forever.
pub fn is_rt_scheduled(pid: u32) -> bool {
    use std::fs::read_to_string;
    // /proc/<pid>/stat, space-separated: field 40 = rt_priority,
    // field 41 = policy (1 = SCHED_FIFO, 2 = SCHED_RR).
    let Ok(stat) = read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    let mut fields = stat.split_whitespace();
    let mut idx = 0;
    for f in fields.by_ref() {
        idx += 1;
        if idx >= 42 {
            break;
        }
        if idx == 41 {
            return f == "1" || f == "2";
        }
    }
    false
}

pub fn apply_nice(pid: u32, nice: i32) -> Result<(), i32> {
    // `setpriority` on a single pid.  Non-root users may only *raise* nice
    // values; lowering (negative nice) needs CAP_SYS_NICE (root in practice).
    // Presets (e.g. the game tier) legitimately ask for negative nice, so as
    // a user daemon we clamp to 0 once instead of failing per-pid forever —
    // cgroup weights still carry the actual prioritization.
    let nice = if nice < 0 && !can_apply_negative_nice() {
        0
    } else {
        nice
    };
    let rc = unsafe { libc::setpriority(libc::PRIO_PROCESS, pid as libc::id_t, nice as libc::c_int) };
    if rc == -1 {
        Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(-1))
    } else {
        Ok(())
    }
}

pub fn get_ioprio(pid: u32) -> Option<CurrentIoprio> {
    let rc = unsafe {
        libc::syscall(libc::SYS_ioprio_get, IOPRIO_WHO_PROCESS, pid as libc::c_int)
    };
    if rc < 0 {
        return None;
    }
    let v = rc as u64;
    let class = match (v >> IOPRIO_CLASS_SHIFT) & 0b111 {
        0 => IoniceClass::None,
        1 => IoniceClass::Realtime,
        2 => IoniceClass::BestEffort,
        _ => IoniceClass::Idle,
    };
    Some(CurrentIoprio {
        class,
        priority: (v & 0xff) as u8,
    })
}

pub fn set_ioprio(pid: u32, class: IoniceClass, priority: u8) -> Result<(), i32> {
    let class_bits = match class {
        IoniceClass::None => IOPRIO_CLASS_NONE,
        IoniceClass::Realtime => IOPRIO_CLASS_REALTIME,
        IoniceClass::BestEffort => IOPRIO_CLASS_BEST_EFFORT,
        IoniceClass::Idle => IOPRIO_CLASS_IDLE,
    };
    let value = (class_bits << IOPRIO_CLASS_SHIFT) | (priority as u64 & 0xff);
    let rc = unsafe {
        libc::syscall(
            libc::SYS_ioprio_set,
            IOPRIO_WHO_PROCESS,
            pid as libc::c_int,
            value,
        )
    };
    if rc == -1 {
        Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(-1))
    } else {
        Ok(())
    }
}

/// Apply optional cgroup v2 limits.  Only meaningful when the daemon runs
/// with write access to the cgroup fs and the target path is in a delegated
/// subtree — see SETUP.md.  Best-effort: failures are logged, never fatal.
fn write_cgroup_file(path: &Path, value: &str) -> Result<(), String> {
    std::fs::write(path, value).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioprio_value_round_trips() {
        // best-effort/2 -> (2 << 13) | 2 = 16386
        let v: u64 = (IOPRIO_CLASS_BEST_EFFORT << IOPRIO_CLASS_SHIFT) | 2;
        assert_eq!(v, 2 * (1 << 13) + 2);
        // decode back
        assert_eq!((v >> IOPRIO_CLASS_SHIFT) & 0b111, IOPRIO_CLASS_BEST_EFFORT);
        assert_eq!(v & 0xff, 2);
    }

    #[test]
    fn failure_tracker_rates_limits() {
        let mut t = FailureTracker::default();
        assert!(t.first_failure(42));
        assert!(!t.first_failure(42));
        t.forget(42);
        assert!(t.first_failure(42));
    }

    #[test]
    fn rt_scheduling_detection() {
        // Our own test process runs under the normal CFS class.
        assert!(!is_rt_scheduled(std::process::id()));
        // A dead pid must not panic.
        assert!(!is_rt_scheduled(999_999_999));
    }

    #[test]
    fn session_boundaries_are_forbidden_bases() {
        // Managed dirs must never be siblings of the session's slices: that
        // would let our cpu weights starve the interactive session (audio
        // stack included) out of the CPU share under contention.
        assert!(is_forbidden_base(std::path::Path::new("/sys/fs/cgroup")));
        assert!(is_forbidden_base(std::path::Path::new(
            "/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service"
        )));
        assert!(is_forbidden_base(std::path::Path::new(
            "/sys/fs/cgroup/user.slice/user-1000.slice"
        )));
        assert!(is_forbidden_base(std::path::Path::new(
            "/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/session.slice"
        )));
        assert!(is_forbidden_base(std::path::Path::new("/sys/fs/cgroup/user.slice")));
        // Legitimate bases stay allowed: the daemon's enclosing slice.
        assert!(!is_forbidden_base(std::path::Path::new(
            "/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/app.slice"
        )));
        assert!(!is_forbidden_base(std::path::Path::new(
            "/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/app.slice/nicewatch.service"
        )));
        assert!(!is_forbidden_base(std::path::Path::new("/sys/fs/cgroup/system.slice")));
    }

    #[test]
    fn quota_maps_percent_to_cpu_max() {
        // 50% of one core, period 100000 µs.
        assert_eq!(quota_string(Some(50)), "50000 100000");
        assert_eq!(quota_string(Some(100)), "100000 100000");
        // Caps clamp to the safe ceiling (3200% = 32 cores).
        assert_eq!(quota_string(Some(99999)), "3200000 100000");
        assert_eq!(quota_string(Some(0)), "1000 100000");
        assert_eq!(quota_string(None), "");
    }

    #[test]
    fn sanitize_dir_keeps_safe_chars() {
        assert_eq!(sanitize_dir("cs2"), "cs2");
        assert_eq!(sanitize_dir("Isolated Web Co"), "Isolated_Web_Co");
        assert_eq!(sanitize_dir("a/b:../c"), "a_b_.._c");
        assert_eq!(sanitize_dir(""), "");
    }

    #[test]
    fn manager_uses_explicit_path_and_writes_limits() {
        let td = tempfile::TempDir::new().unwrap();
        let explicit = td.path().join("explicit");
        std::fs::create_dir_all(&explicit).unwrap();
        let mut m = CgroupManager {
            base: Some(td.path().to_path_buf()),
            enrolled: HashMap::new(),
            created_dirs: HashSet::new(),
            warned: HashSet::new(),
            controllers_reasserted: false,
            state_path: None,
        };
        let lims = CgroupLimits {
            path: Some(explicit.display().to_string()),
            cpu_weight: Some(900),
            cpu_cap_percent: Some(50),
            memory_high: Some("1G".into()),
            memory_max: Some("2G".into()),
            cpu_idle: None,
        };
        // A real pid that exists (our own test process) so the move works:
        // writes the pid into cgroup.procs of the temp dir — fake cgroup fs.
        m.sync_pid(std::process::id(), "cs2", Some(&lims));
        assert!(m.is_capped(std::process::id()));
        assert_eq!(
            std::fs::read_to_string(explicit.join("cpu.max")).unwrap(),
            "50000 100000"
        );
        assert_eq!(
            std::fs::read_to_string(explicit.join("cpu.weight")).unwrap(),
            "900"
        );
        assert_eq!(
            std::fs::read_to_string(explicit.join("memory.high")).unwrap(),
            "1G"
        );
        assert_eq!(
            std::fs::read_to_string(explicit.join("memory.max")).unwrap(),
            "2G"
        );
        assert_eq!(
            std::fs::read_to_string(explicit.join("cgroup.procs")).unwrap(),
            std::process::id().to_string()
        );
        m.shutdown();
    }

    #[test]
    fn controller_reassertion_forces_limit_rewrite() {
        // When systemd drops `cpu` from the base's subtree_control and the
        // daemon re-enables it, the kernel resets every child's cpu.weight
        // to the default (100).  The daemon only rewrites limits on *change*,
        // so the reassertion flag must force a rewrite of an unchanged rule.
        let td = tempfile::TempDir::new().unwrap();
        let mut m = CgroupManager {
            base: Some(td.path().to_path_buf()),
            enrolled: HashMap::new(),
            created_dirs: HashSet::new(),
            warned: HashSet::new(),
            controllers_reasserted: false,
            state_path: None,
        };
        let lims = CgroupLimits {
            path: None,
            cpu_weight: Some(300),
            cpu_cap_percent: None,
            memory_high: None,
            memory_max: None,
            cpu_idle: None,
        };
        m.sync_pid(std::process::id(), "chrome", Some(&lims));
        let dir = td.path().join("nw-chrome");
        assert_eq!(
            std::fs::read_to_string(dir.join("cpu.weight")).unwrap(),
            "300"
        );
        // Kernel-side reset (as if the controller was re-enabled).
        std::fs::write(dir.join("cpu.weight"), "100").unwrap();
        // Without the flag, an unchanged rule would not be rewritten...
        m.sync_pid(std::process::id(), "chrome", Some(&lims));
        assert_eq!(
            std::fs::read_to_string(dir.join("cpu.weight")).unwrap(),
            "100"
        );
        // ...and with it, the next reconcile restores the configured weight.
        m.controllers_reasserted = true;
        m.sync_pid(std::process::id(), "chrome", Some(&lims));
        assert_eq!(
            std::fs::read_to_string(dir.join("cpu.weight")).unwrap(),
            "300"
        );
        m.clear_controllers_reasserted();
        m.shutdown();
    }

    #[test]
    fn manager_auto_creates_dir_from_rule_name() {
        let td = tempfile::TempDir::new().unwrap();
        let mut m = CgroupManager {
            base: Some(td.path().to_path_buf()),
            enrolled: HashMap::new(),
            created_dirs: HashSet::new(),
            warned: HashSet::new(),
            controllers_reasserted: false,
            state_path: None,
        };
        let lims = CgroupLimits {
            path: None,
            cpu_weight: None,
            cpu_cap_percent: Some(25),
            memory_high: None,
            memory_max: None,
            cpu_idle: None,
        };
        m.sync_pid(std::process::id(), "Isolated Web Co", Some(&lims));
        let dir = td.path().join("nw-Isolated_Web_Co");
        assert!(dir.is_dir());
        assert_eq!(
            std::fs::read_to_string(dir.join("cpu.max")).unwrap(),
            "25000 100000"
        );
        assert!(m.is_capped(std::process::id()));
        // Re-sync with the same limits is a no-op (no rewrite churn).
        m.sync_pid(std::process::id(), "Isolated Web Co", Some(&lims));
        // Re-sync with None moves the pid back and prunes the dir.
        m.sync_pid(std::process::id(), "Isolated Web Co", None);
        assert!(!m.is_capped(std::process::id()));
        assert!(!dir.exists());
        m.shutdown();
    }

    #[test]
    fn no_base_degrades_gracefully() {
        let mut m = CgroupManager {
            base: None,
            enrolled: HashMap::new(),
            created_dirs: HashSet::new(),
            warned: HashSet::new(),
            controllers_reasserted: false,
            state_path: None,
        };
        let lims = CgroupLimits {
            path: None,
            cpu_weight: None,
            cpu_cap_percent: Some(25),
            memory_high: None,
            memory_max: None,
            cpu_idle: None,
        };
        // Must not panic or create anything.
        m.sync_pid(1, "x", Some(&lims));
        assert!(!m.is_capped(1));
    }

    fn quota_string(pct: Option<u32>) -> String {
        let Some(p) = pct else { return String::new() };
        let p = p.min(MAX_CAP_PERCENT).max(1);
        let quota = (p as u64) * (CGROUP_PERIOD_US / 100);
        format!("{quota} {CGROUP_PERIOD_US}")
    }
}