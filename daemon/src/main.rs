//! Nicewatch daemon: polls /proc, resolves priorities, applies them, and
//! serves the GUI over a Unix-domain socket.
//!
//! Root-only operations (writing `/etc/proc-priority-daemon/`, renice of
//! other users' processes) are attempted and logged; the daemon never shells
//! out to sudo.  See SETUP.md for the manual sudo steps.

mod applier;
mod cli;
mod config;
mod game_detect;
mod known_games;
mod ipc;
mod proc_scan;
mod rules;
mod setup;
mod sync;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clap::Parser;
use log::{debug, info, warn};
use nicewatch_common::{
    APP_DISPLAY_NAME, APP_NAME, DEFAULT_POLL_INTERVAL_MS, GameAnswer, GamePrompt,
    IoniceClass, Preset, ProcessInfo, RuleInfo, ServerMsg, Snapshot, Tier,
};
use notify::Watcher;

use crate::game_detect::{FullscreenDetector, NoopFullscreenDetector};
use crate::rules::{ApplyTarget, Resolved, RuleSet};
use crate::sync::Sync;

#[derive(Parser, Debug)]
#[command(
    name = APP_NAME,
    version,
    about = format!("{APP_DISPLAY_NAME} — automatic CPU/IO scheduling priority daemon")
)]
struct Args {
    /// Override the root (live) `/etc/...` rules file.
    #[arg(long)]
    root_config: Option<PathBuf>,
    /// Override the local (`~/.config/...`) staging rules file.
    #[arg(long)]
    local_config: Option<PathBuf>,
    /// Override the IPC socket path.
    #[arg(long)]
    socket: Option<PathBuf>,
    /// Poll interval in milliseconds (min 250; a `poll_interval_ms` key in
    /// the config file wins over this flag).
    #[arg(long)]
    poll_ms: Option<u64>,
    /// Promote-debounce (ms) before the local config is written to root.
    #[arg(long, default_value_t = sync::DEFAULT_PROMOTE_DEBOUNCE_MS)]
    promote_debounce_ms: u64,
    /// Debounce (ms) before GUI-set rule edits hit the local config file.
    #[arg(long, default_value_t = 1_000)]
    local_write_debounce_ms: u64,

    /// Install / uninstall / inspect the service backend for this system,
    /// or run the read-only CLI companions.
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

/// All subcommands: service management (the "click one button" from the GUI)
/// and the read-only companions.
#[derive(clap::Subcommand, Debug)]
enum Cmd {
    /// Install the daemon as a service: systemd user unit, system unit (as
    /// root), or an XDG autostart entry, whichever fits this system.
    Install,
    /// Remove the installed service unit/entry (keeps config and binary).
    Uninstall,
    /// Show which backend is active and where the files live.
    Status,
    /// Print the daemon's current view of running processes (its process
    /// snapshot, same data the GUI renders).
    Ps,
    /// Print the active rule set (root config wins over the local one).
    Rules,
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    let args = Args::parse();
    // Short-circuit the daemon loop for subcommands.
    if let Some(cmd) = &args.cmd {
        let root_cfg = args
            .root_config
            .clone()
            .unwrap_or_else(nicewatch_common::root_config_path);
        let local_cfg = args
            .local_config
            .clone()
            .unwrap_or_else(nicewatch_common::local_config_path);
        let socket = args
            .socket
            .clone()
            .unwrap_or_else(nicewatch_common::ipc_socket_path);
        let result = match cmd {
            Cmd::Install => setup::install().map(|m| println!("{m}")),
            Cmd::Uninstall => setup::uninstall().map(|m| println!("{m}")),
            Cmd::Status => Ok(setup::status()).map(|m| println!("{m}")),
            Cmd::Ps => cli::ps(&socket),
            Cmd::Rules => cli::rules(&root_cfg, &local_cfg),
        };
        if let Err(e) = result {
            eprintln!("{e}");
            std::process::exit(1);
        }
        return;
    }
    if let Err(e) = run(&args) {
        eprintln!("{e}");
        std::process::exit(1);
    }
}

#[allow(clippy::too_many_lines)]
fn run(args: &Args) -> Result<(), String> {
    let root_cfg = args
        .root_config
        .clone()
        .unwrap_or_else(nicewatch_common::root_config_path);
    let local_cfg = args
        .local_config
        .clone()
        .unwrap_or_else(nicewatch_common::local_config_path);
    let socket_path = args
        .socket
        .clone()
        .unwrap_or_else(nicewatch_common::ipc_socket_path);

    info!("{APP_DISPLAY_NAME} daemon starting");
    info!("root config: {}", root_cfg.display());
    info!("local config: {}", local_cfg.display());
    info!("ipc socket: {}", socket_path.display());

    // ------------------------------------------------------------------
    // Config sync (dual-config precedence).
    // ------------------------------------------------------------------
    let mut sync = Sync::new(
        root_cfg.clone(),
        local_cfg.clone(),
        Duration::from_millis(args.promote_debounce_ms),
        Duration::from_millis(args.local_write_debounce_ms),
    );
    sync.initial_load(Instant::now());
    for w in sync.warnings.drain(..) {
        warn!("{w}");
    }
    let mut ruleset = RuleSet::from_config(&sync.active);
    let mut poll_interval = effective_poll_interval(args, &sync.active);
    info!("poll interval: {poll_interval}ms");
    let mut cgroups = applier::CgroupManager::new();
    if let Some(b) = cgroups.base() {
        info!("cgroup limiting base: {}", b.display());
    }

    // ------------------------------------------------------------------
    // File watcher (notify).  The poll-based `sync.poll_mtimes()` fallback
    // covers directories that don't exist yet.
    // ------------------------------------------------------------------
    let (wx, wrx) = mpsc::channel::<notify::Event>();
    // `_watcher` lives for the whole daemon run (keeps the watch thread alive).
    let mut _watcher: notify::RecommendedWatcher =
        notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(ev) = res {
                let _ = wx.send(ev);
            }
        })
        .map_err(|e| format!("cannot start file watcher: {e}"))?;

    let dirs: HashSet<&std::path::Path> = [root_cfg.parent(), local_cfg.parent()]
        .into_iter()
        .flatten()
        .collect();
    for dir in dirs {
        match _watcher.watch(dir, notify::RecursiveMode::NonRecursive) {
            Ok(()) => {}
            Err(e) => warn!("cannot watch {}: {e} (poll fallback is active)", dir.display()),
        }
    }

    // ------------------------------------------------------------------
    // IPC server.
    // ------------------------------------------------------------------
    let (ipc_tx, ipc_rx) = mpsc::channel::<ipc::IpcEvent>();
    let ipc = ipc::start(&socket_path, ipc_tx)
        .map_err(|e| format!("cannot bind IPC socket {}: {e}", socket_path.display()))?;

    // Fullscreen detection: the active KWin window (D-Bus) is matched per
    // process; `refresh()` is called once per poll, so the poll loop never
    // blocks on a D-Bus round-trip (and the KDE QML import stays optional —
    // no Qt dependency, no dbus crate, AppImage-safe).
    // Noop: the daemon must never call the compositor (KWin D-Bus polling
    // caused the stuck-cursor bug).  Game detection uses Steam env + DRM
    // fds only.
    let fullscreen: Arc<dyn FullscreenDetector> = Arc::new(NoopFullscreenDetector);

    // Clean shutdown on SIGINT/SIGTERM (removes the socket file).
    let run_flag = Arc::new(AtomicBool::new(true));
    {
        let run_flag = run_flag.clone();
        let socket_path = socket_path.clone();
        ctrlc::set_handler(move || {
            run_flag.store(false, Ordering::SeqCst);
            let _ = std::fs::remove_file(&socket_path);
        })
        .map_err(|e| format!("cannot install signal handler: {e}"))?;
    }

    // ------------------------------------------------------------------
    // Poll-loop state.
    // ------------------------------------------------------------------
    let mut last_poll = Instant::now()
        .checked_sub(Duration::from_millis(poll_interval))
        .unwrap_or_else(Instant::now);
    let mut prev_ticks: HashMap<u32, (u64, u64)> = HashMap::new();
    let mut full_info: HashMap<u32, ProcessInfo> = HashMap::new();
    let mut flag_cache: HashMap<u32, game_detect::GameFlags> = HashMap::new();
    let mut failures = applier::FailureTracker::default();
    // Names the heuristic has asked about *for a currently running instance*
    // (per-run memory; the rules file is the cross-restart memory).
    let mut prompted_names: HashSet<String> = HashSet::new();
    // "Not now" answers: auto-game tier for the running instance only, and no
    // re-ask until every process with this name has exited.
    let mut instance_games: HashSet<String> = HashSet::new();
    let mut suppressed_until_exit: HashSet<String> = HashSet::new();
    let mut pending_prompts: Vec<GamePrompt> = Vec::new();
    let mut sent_initial_snapshot = false;
    // (busy, total) CPU ticks from /proc/stat; system CPU% = delta ratio.
    let mut prev_cpu_busy: Option<(u64, u64)> = None;
    let mut system_cpu = 0.0f32;
    // The first system-CPU sample right after startup measures whatever the
    // boot/launch workload left behind (AppImage unpacking, shader warm-ups)
    // and is alarmingly high for no reason.  Skip the first two samples so
    // the GUI pill only ever shows steady-state numbers.
    let mut system_cpu_samples = 0u32;
    // (total, used) KiB from /proc/meminfo, refreshed every poll.
    let mut system_mem = (0u64, 0u64);

    let ncpu = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let our_pid = std::process::id();
    // Known-games catalog (embedded; see known_games.rs).  Parsed once.
    let known = crate::known_games::KnownGames::embedded();
    info!("known-games catalog: {} entries", known.len());

    while run_flag.load(Ordering::SeqCst) {
        let now = Instant::now();
        // The poll interval is config-driven; a GUI change takes effect on
        // the next loop iteration.
        poll_interval = effective_poll_interval(args, &sync.active);

        // -- IPC requests (set tier, confirm game, hello) ----------------
        while let Ok(ev) = ipc_rx.try_recv() {
            let ipc::IpcEvent::Msg(msg) = ev;
            handle_client_msg(
                msg,
                &ipc,
                &mut sync,
                &mut ruleset,
                &full_info,
                &mut pending_prompts,
                &mut instance_games,
                &mut suppressed_until_exit,
                poll_interval,
                system_cpu,
                system_mem,
            );
        }

        // -- Config watcher events ---------------------------------------
        // `Access` events (Open/Close/Read) fire for every read of the config
        // files — including OUR OWN reads during a reload — and would loop
        // forever (reload -> read -> event -> reload).  Only react to real
        // content changes.
        let mut config_changed = false;
        while let Ok(ev) = wrx.try_recv() {
            if matches!(ev.kind, notify::EventKind::Access(_)) {
                continue;
            }
            for path in ev.paths {
                if sync.on_path_changed(&path, now) {
                    config_changed = true;
                    info!("config changed on disk — reloaded rule set");
                }
            }
        }
        if sync.poll_mtimes(now) {
            config_changed = true;
            info!("config mtime changed — reloaded rule set");
        }
        if config_changed {
            ruleset = RuleSet::from_config(&sync.active);
            broadcast_snapshot(&ipc, &full_info, &ruleset, &pending_prompts, poll_interval, system_cpu, system_mem);
        }
        if sync.tick(now) {
            broadcast_snapshot(&ipc, &full_info, &ruleset, &pending_prompts, poll_interval, system_cpu, system_mem);
        }

        // -- Poll cycle ---------------------------------------------------
        if now.duration_since(last_poll) >= Duration::from_millis(poll_interval) {
            let elapsed = now.duration_since(last_poll).as_secs_f64().max(0.001);
            last_poll = now;
            // Self-heal the base's controller delegation (systemd can drop
            // `cpu` from the session slices' subtree_control on rebuilds).
            cgroups.ensure_controllers();
            let btime = proc_scan::read_btime(std::path::Path::new("/proc"));
            let entries = proc_scan::scan_proc(std::path::Path::new("/proc"), btime);
            let our_uid = applier::our_euid();
            // Refresh the active-window picture once per poll so per-process
            // `is_fullscreen` calls stay allocation-free and non-blocking.
            fullscreen.refresh();
            let now_secs = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);

            let mut cur_info: HashMap<u32, ProcessInfo> = HashMap::new();
            let mut names_seen: HashSet<String> = HashSet::new();

            for e in &entries {
                names_seen.insert(e.name.clone());

                // Fresh names (no rule yet, never asked) get fresh heuristic
                // work each poll; established ones reuse cached flags.
                let undecided = !ruleset.has_rule(&e.name)
                    && !prompted_names.contains(&e.name)
                    && !suppressed_until_exit.contains(&e.name);
                // Never flag our own binaries as games (the GUI holds a
                // `/dev/dri` fd for WebKit/GL rendering).
                let ours = e.pid == our_pid || game_detect::is_self_binary(&e.name);
                // Other users' processes (root services, containers) reject
                // every scheduling write from a non-root daemon with EACCES.
                // Only a root daemon may touch them, so a user daemon skips
                // them wholesale — otherwise every transient root-owned
                // process (e.g. steam's `sh` scaffold scripts) would spam
                // warnings and game prompts into the GUI.
                let untouchable = e.uid != our_uid && !applier::can_apply_negative_nice();
                let flags = if ours || untouchable {
                    game_detect::GameFlags {
                        steam_env: false,
                        dri_fd: false,
                        fullscreen: false,
                        known: false,
                    }
                } else {
                    let mut f = match flag_cache.get(&e.pid) {
                        Some(f) if !undecided => *f,
                        _ => {
                            let f = game_detect::detect_game(e, fullscreen.as_ref());
                            flag_cache.insert(e.pid, f);
                            f
                        }
                    };
                    // Known-game catalog is authoritative for anything that
                    // isn't compositor/DE-critical (those stay hard-excluded
                    // below no matter what).
                    if !game_detect::is_de_critical(&e.name) {
                        f.known = known.is_known_entry(e);
                    }
                    f
                };

                let age = now_secs.saturating_sub(e.start_secs);
                let mut resolved = ruleset.resolve(&e.name, &flags, age);

                // Hard exclusion: compositor/desktop-critical processes are
                // NEVER touched, even when a rule matches them by name.  The
                // compositor owns input and cursor rendering; moving or
                // throttling it (or its helpers) is exactly how a stuck
                // cursor is produced.
                if game_detect::is_de_critical(&e.name) {
                    resolved = Resolved::None;
                }

                if matches!(resolved, Resolved::None) && instance_games.contains(&e.name) {
                    let p = ruleset.auto_game.preset();
                    resolved = Resolved::Apply(ApplyTarget {
                        tier: Some(ruleset.auto_game.tier.unwrap_or(Tier::Game)),
                        nice: p.nice,
                        ionice_class: p.ionice_class,
                        ionice_priority: p.ionice_priority,
                        rule: None,
                        cgroup: None,
                    });
                }

                // Untouchable processes win over everything above: a user
                // daemon cannot change another user's scheduling at all.
                if untouchable {
                    resolved = Resolved::None;
                }

                let tier = match &resolved {
                    Resolved::Apply(t) => t.tier,
                    Resolved::None => None,
                };

                // Cgroup limiting: reconcile the pid against its rule's
                // desired cgroup (creates the dir, moves the pid in, writes
                // cpu.max etc.; None = move back out).  Rule name keys the
                // managed dir when no explicit path is given.
                let (cg_key, cg_limits) = match &resolved {
                    Resolved::Apply(t) => (t.rule.as_deref().unwrap_or("game"), t.cgroup.as_ref()),
                    Resolved::None => ("", None),
                };
                cgroups.sync_pid(e.pid, cg_key, cg_limits);

                if let Resolved::Apply(t) = &resolved {
                    apply_target(e, t, &mut failures, &ipc);
                }

                let current_io = applier::get_ioprio(e.pid);
                let (io_class, io_prio) = match current_io {
                    Some(io) => (io.class, io.priority),
                    None => (IoniceClass::None, 0),
                };

                let cpu = proc_scan::compute_cpu_percent(
                    prev_ticks.get(&e.pid).copied(),
                    (e.utime, e.stime),
                    elapsed,
                    ncpu,
                );
                prev_ticks.insert(e.pid, (e.utime, e.stime));

                cur_info.insert(
                    e.pid,
                    ProcessInfo {
                        pid: e.pid,
                        ppid: e.ppid,
                        name: e.name.clone(),
                        user: proc_scan::uid_to_user(e.uid),
                        cpu_percent: cpu,
                        mem_kb: e.rss_kb,
                        status: status_label(e.state),
                        nice: e.nice,
                        ionice_class: io_class,
                        ionice_priority: io_prio,
                        tier,
                        game_detected: flags.is_game(),
                        capped: cgroups.is_capped(e.pid),
                        exe: e.exe.clone(),
                        start_secs: e.start_secs,
                    },
                );
            }

            // Reclaim managed cgroups from pids we never moved in (crash
            // remnants between the last state persist and a SIGKILL, or pids
            // stranded when systemd rebuilt the session slices).
            cgroups.sweep_strays();

            // Refresh the system CPU/mem picture first so the diff below can
            // carry it; skip the first two samples (startup workload is not
            // representative — it would show ~90% during AppImage unpack).
            let cpu_busy = proc_scan::read_cpu_busy(std::path::Path::new("/proc"));
            system_cpu = proc_scan::compute_system_cpu(prev_cpu_busy.take(), cpu_busy);
            prev_cpu_busy = Some(cpu_busy);
            system_cpu_samples += 1;
            let report_cpu = if system_cpu_samples >= 2 { system_cpu } else { 0.0 };
            system_mem = proc_scan::read_system_mem(std::path::Path::new("/proc"));

            // Diff against the previous cycle and broadcast.
            let diff = diff_tables(&full_info, &cur_info, report_cpu, system_mem);
            full_info = cur_info;

            for pid in &diff.removed {
                prev_ticks.remove(pid);
                flag_cache.remove(pid);
                cgroups.forget(*pid);
                failures.forget(*pid);
            }

            // Prune per-instance bookkeeping once a name has fully exited.
            for name in prompted_names
                .union(&suppressed_until_exit)
                .chain(instance_games.iter())
                .cloned()
                .collect::<Vec<_>>()
            {
                if !names_seen.contains(&name) {
                    prompted_names.remove(&name);
                    suppressed_until_exit.remove(&name);
                    instance_games.remove(&name);
                }
            }

            // Prompt new game-looking names (OpenSnitch-style confirmation).
            // Known games never prompt: they get the auto-game tier right
            // away via rules::resolve.
            for (name, pid) in fresh_game_names(
                &full_info,
                &ruleset,
                &prompted_names,
                &suppressed_until_exit,
                &known,
            ) {
                prompted_names.insert(name.clone());
                if ipc.client_count() > 0 {
                    pending_prompts.push(GamePrompt {
                        name: name.clone(),
                        pid,
                    });
                    ipc.broadcast(&ServerMsg::PromptGame(GamePrompt {
                        name: name.clone(),
                        pid,
                    }));
                    info!("flagged `{name}` as a likely game — awaiting GUI confirmation");
                } else {
                    // No GUI to ask: the non-blocking safe default is "Not
                    // now" (auto-game tier for this instance only).
                    warn!(
                        "heuristic flagged `{name}` as a likely game but no GUI is connected; \
                         applying the auto-game preset to this instance only (\"Not now\"). \
                         Start the GUI while it runs to get the confirmation dialog."
                    );
                    instance_games.insert(name.clone());
                    suppressed_until_exit.insert(name);
                }
            }

            if !sent_initial_snapshot {
                sent_initial_snapshot = true;
                broadcast_snapshot(&ipc, &full_info, &ruleset, &pending_prompts, poll_interval, system_cpu, system_mem);
            } else if !diff.is_empty() {
                ipc.broadcast(&ServerMsg::Diff(diff));
            }

            // Surface sync warnings once per cycle (e.g. can't write /etc).
            for w in sync.warnings.drain(..) {
                warn!("{w}");
                ipc.broadcast(&ServerMsg::Warn { msg: w });
            }
        }

        std::thread::sleep(Duration::from_millis(100));
    }

    // Release every process back to its original cgroup before exiting.
    if cgroups.stats() != (0, 0) {
        info!("shutting down: releasing {} managed cgroup(s), {} pid(s)", cgroups.stats().0, cgroups.stats().1);
        cgroups.shutdown();
    }

    info!("{APP_DISPLAY_NAME} daemon stopped");
    let _ = std::fs::remove_file(&socket_path);
    Ok(())
}

fn effective_poll_interval(args: &Args, cfg: &nicewatch_common::AppConfig) -> u64 {
    let v = cfg
        .poll_interval_ms
        .or(args.poll_ms)
        .unwrap_or(DEFAULT_POLL_INTERVAL_MS);
    v.clamp(nicewatch_common::MIN_POLL_INTERVAL_MS, 60_000)
}

fn status_label(state: char) -> String {
    match state {
        'R' => "running",
        'S' => "sleeping",
        'D' => "io-wait",
        'Z' => "zombie",
        'T' => "stopped",
        'I' => "idle",
        _ => "other",
    }
    .into()
}

/// Strip every stall-class limit from a focused client's cgroup limits.
///
/// Apply the desired (nice, ionice) to one process.  Compares against
/// kernel-read values and only syscalls on mismatch.  EPERM on other users'
/// processes is logged once per pid.  Cgroup limiting is handled separately
/// by `CgroupManager::sync_pid` (it needs per-rule reconcile semantics).
fn apply_target(
    e: &proc_scan::ProcEntry,
    t: &ApplyTarget,
    failures: &mut applier::FailureTracker,
    ipc: &ipc::IpcHandle,
) {
    // Real-time scheduled tasks (KWin's compositor on Plasma 6 runs
    // SCHED_RR) reject nice/ionice writes with EPERM by design — nothing we
    // can or should do to them.
    if applier::is_rt_scheduled(e.pid) {
        debug!("pid {} is RT-scheduled — leaving its nice/ionice alone", e.pid);
        return;
    }

    // A user daemon may only *raise* nice (increase the value): lowering it
    // — even on its own processes, even from an already-high value like 19 —
    // needs CAP_SYS_NICE.  Clamp the target to the process's current value
    // instead of failing per-pid (cgroup weights still carry the actual
    // prioritization for such processes).
    let effective_nice = if applier::can_apply_negative_nice() {
        t.nice
    } else {
        t.nice.max(e.nice)
    };

    if e.nice != effective_nice {
        match applier::apply_nice(e.pid, effective_nice) {
            Ok(()) => debug!("pid {} nice -> {}", e.pid, effective_nice),
            Err(errno) => {
                let msg = format!(
                    "cannot set nice={} for `{}` (pid {}) — {errno}. Run the daemon as root \
                     (systemd unit, see SETUP.md) to touch other users' processes or use \
                     negative nice",
                    effective_nice, e.name, e.pid
                );
                if failures.first_failure(e.pid) {
                    warn!("{msg}");
                    ipc.broadcast(&ServerMsg::Warn { msg });
                }
            }
        }
    }

    // Ionice only when the rule/preset demands a real class (None = leave
    // alone, kernel default).
    if t.ionice_class != IoniceClass::None {
        let wants = applier::CurrentIoprio {
            class: t.ionice_class,
            priority: t.ionice_priority,
        };
        let differs = match applier::get_ioprio(e.pid) {
            Some(cur) => cur != wants,
            None => true,
        };
        if differs {
            match applier::set_ioprio(e.pid, t.ionice_class, t.ionice_priority) {
                Ok(()) => debug!(
                    "pid {} ionice -> {}/{}",
                    e.pid,
                    t.ionice_class.label(),
                    t.ionice_priority
                ),
                Err(errno) => {
                    let msg = format!(
                        "cannot set ionice for `{}` (pid {}): {errno}",
                        e.name, e.pid
                    );
                    if failures.first_failure(e.pid) {
                        warn!("{msg}");
                        ipc.broadcast(&ServerMsg::Warn { msg });
                    }
                }
            }
        }
    }
}

fn diff_tables(
    prev: &HashMap<u32, ProcessInfo>,
    cur: &HashMap<u32, ProcessInfo>,
    system_cpu: f32,
    system_mem: (u64, u64),
) -> nicewatch_common::Diff {
    let mut added = Vec::new();
    let mut updated = Vec::new();
    let mut removed = Vec::new();
    for (pid, info) in cur {
        match prev.get(pid) {
            None => added.push(info.clone()),
            Some(old) => {
                if process_changed(old, info) {
                    updated.push(info.clone());
                }
            }
        }
    }
    for pid in prev.keys() {
        if !cur.contains_key(pid) {
            removed.push(*pid);
        }
    }
    nicewatch_common::Diff {
        added,
        updated,
        removed,
        system_cpu,
        system_mem_total_kb: system_mem.0,
        system_mem_used_kb: system_mem.1,
    }
}

fn process_changed(a: &ProcessInfo, b: &ProcessInfo) -> bool {
    a.ppid != b.ppid
        || a.name != b.name
        || a.user != b.user
        || (a.cpu_percent - b.cpu_percent).abs() > 0.05
        || a.mem_kb != b.mem_kb
        || a.status != b.status
        || a.nice != b.nice
        || a.ionice_class != b.ionice_class
        || a.ionice_priority != b.ionice_priority
        || a.tier != b.tier
        || a.capped != b.capped
        || a.game_detected != b.game_detected
}

fn fresh_game_names(
    full: &HashMap<u32, ProcessInfo>,
    ruleset: &RuleSet,
    prompted: &HashSet<String>,
    suppressed: &HashSet<String>,
    known: &crate::known_games::KnownGames,
) -> Vec<(String, u32)> {
    let mut out = Vec::new();
    for info in full.values() {
        if info.game_detected
            && !known.is_known_name(&info.name)
            && !ruleset.has_rule(&info.name)
            && !prompted.contains(&info.name)
            && !suppressed.contains(&info.name)
        {
            out.push((info.name.clone(), info.pid));
        }
    }
    out.sort();
    out.dedup();
    out
}

#[allow(clippy::too_many_arguments)]
fn handle_client_msg(
    msg: nicewatch_common::ClientMsg,
    ipc: &ipc::IpcHandle,
    sync: &mut Sync,
    ruleset: &mut RuleSet,
    full_info: &HashMap<u32, ProcessInfo>,
    pending_prompts: &mut Vec<GamePrompt>,
    instance_games: &mut HashSet<String>,
    suppressed_until_exit: &mut HashSet<String>,
    poll_interval: u64,
    system_cpu: f32,
    system_mem: (u64, u64),
) {
    use nicewatch_common::ClientMsg;
    match msg {
        ClientMsg::Hello { client_kind } => {
            info!("IPC client connected ({client_kind})");
            broadcast_snapshot(ipc, full_info, ruleset, pending_prompts, poll_interval, system_cpu, system_mem);
        }
        ClientMsg::RequestSnapshot => {
            broadcast_snapshot(ipc, full_info, ruleset, pending_prompts, poll_interval, system_cpu, system_mem);
        }
        ClientMsg::SetPollInterval { poll_interval_ms } => {
            let clamped = poll_interval_ms.clamp(nicewatch_common::MIN_POLL_INTERVAL_MS, 60_000);
            info!("user set poll interval to {clamped} ms via GUI");
            sync.set_poll_interval(clamped);
            // The loop re-reads the interval from the active config next
            // iteration; broadcast so the GUI pill/settings update promptly.
            broadcast_snapshot(ipc, full_info, ruleset, pending_prompts, clamped, system_cpu, system_mem);
        }
        ClientMsg::SetTier { pid, tier } => {
            let Some(info) = full_info.get(&pid) else {
                warn!("SetTier for unknown pid {pid} (already exited?)");
                return;
            };
            let name = info.name.clone();
            info!("user set `{name}` to {} via GUI", tier.label());
            // Persist a rule for the name via the config-sync path (debounced
            // local write, then 30s-settled promotion to root).
            sync.upsert_preset_rule(&name, tier);
            *ruleset = RuleSet::from_config(&sync.active);
            // Apply instantly to every currently running instance.
            let preset = tier.preset();
            for p in full_info.values().filter(|p| p.name == name) {
                apply_preset_now(p, &preset);
            }
            broadcast_snapshot(ipc, full_info, ruleset, pending_prompts, poll_interval, system_cpu, system_mem);
        }
        ClientMsg::SetCap { name, pct } => {
            info!(
                "user set cap for `{name}` to {} via GUI",
                pct.map(|p| format!("{p}%")).unwrap_or_else(|| "none".into())
            );
            sync.upsert_cap_rule(&name, pct);
            *ruleset = RuleSet::from_config(&sync.active);
            // The next poll reconciles every matching pid into/out of the
            // managed cgroup; broadcast so the GUI shows the new rule soon.
            broadcast_snapshot(ipc, full_info, ruleset, pending_prompts, poll_interval, system_cpu, system_mem);
        }
        ClientMsg::ConfirmGame { name, answer } => {
            info!("game confirmation for `{name}`: {:?}", answer);
            pending_prompts.retain(|p| p.name != name);
            match answer {
                GameAnswer::Yes => {
                    // Persisted: never asked again for this name.
                    sync.upsert_preset_rule(&name, Tier::Game);
                }
                GameAnswer::No => {
                    // Pinned to Software: never auto-flagged again.
                    sync.upsert_preset_rule(&name, Tier::Software);
                }
                GameAnswer::NotNow => {
                    // Instance-only; re-ask after all instances exit.
                    instance_games.insert(name.clone());
                    suppressed_until_exit.insert(name.clone());
                }
            }
            *ruleset = RuleSet::from_config(&sync.active);
            broadcast_snapshot(ipc, full_info, ruleset, pending_prompts, poll_interval, system_cpu, system_mem);
        }
    }
}

/// Immediate per-PID apply of a preset (no persistence involved).
fn apply_preset_now(info: &ProcessInfo, preset: &Preset) {
    if info.nice != preset.nice {
        if let Err(errno) = applier::apply_nice(info.pid, preset.nice) {
            debug!("immediate renice pid {}: {errno}", info.pid);
        }
    }
    if preset.ionice_class != IoniceClass::None {
        let wants = applier::CurrentIoprio {
            class: preset.ionice_class,
            priority: preset.ionice_priority,
        };
        let differs = match applier::get_ioprio(info.pid) {
            Some(cur) => cur != wants,
            None => true,
        };
        if differs {
            if let Err(errno) = applier::set_ioprio(info.pid, preset.ionice_class, preset.ionice_priority) {
                debug!("immediate ionice pid {}: {errno}", info.pid);
            }
        }
    }
}

fn broadcast_snapshot(
    ipc: &ipc::IpcHandle,
    full_info: &HashMap<u32, ProcessInfo>,
    ruleset: &RuleSet,
    pending_prompts: &[GamePrompt],
    poll_interval: u64,
    system_cpu: f32,
    system_mem: (u64, u64),
) {
    let mut processes: Vec<ProcessInfo> = full_info.values().cloned().collect();
    processes.sort_by_key(|p| p.pid);
    let rules: Vec<RuleInfo> = ruleset
        .rules()
        .map(|r| RuleInfo {
            name: r.name.clone(),
            match_name: r.match_name.clone(),
            tier: r.tier,
            nice: r.nice,
            cpu_cap_percent: r.cgroup.as_ref().and_then(|c| c.cpu_cap_percent),
        })
        .collect();
    ipc.broadcast(&ServerMsg::Snapshot(Snapshot {
        processes,
        rules,
        prompts: pending_prompts.to_vec(),
        poll_interval_ms: poll_interval,
        system_cpu,
        system_mem_total_kb: system_mem.0,
        system_mem_used_kb: system_mem.1,
    }));
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_uses_the_noop_fullscreen_backend() {
        let fs: Arc<dyn FullscreenDetector> = Arc::new(NoopFullscreenDetector);
        fs.refresh();
        assert!(!fs.is_fullscreen(&proc_scan::ProcEntry {
            pid: 1,
            ppid: 0,
            start_secs: 0,
            name: "x".into(),
            state: 'R',
            utime: 0,
            stime: 0,
            nice: 0,
            rss_kb: 0,
            uid: 0,
            exe: None,
            environ: None,
            cmdline: None,
            has_dri_fd: true,
            cgroup: None,
        }));
    }
}
