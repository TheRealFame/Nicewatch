//! /proc scanning.  All functions take a `root` path (default "/proc") so
//! tests can run against fixture trees instead of a live system.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::OnceLock;

/// `/proc/<pid>/stat` field 19 is the niceness (0-based field idx 16 after
/// `pid (comm)` is stripped).
const STAT_IDX_STATE: usize = 0;
const STAT_IDX_PPID: usize = 1;
const STAT_IDX_UTIME: usize = 11;
const STAT_IDX_STIME: usize = 12;
const STAT_IDX_NICE: usize = 16;
const STAT_IDX_STARTTIME: usize = 19;

/// Cap on how much of the environment block we read (a couple of MB would
/// be pathological; Steam envs are a few KB).
const ENVIRON_MAX_BYTES: usize = 256 * 1024;

fn clock_ticks() -> u64 {
    static TICKS: OnceLock<u64> = OnceLock::new();
    *TICKS.get_or_init(|| {
        let t = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        if t > 0 {
            t as u64
        } else {
            100
        }
    })
}

#[derive(Debug, Clone)]
pub struct ProcEntry {
    pub pid: u32,
    pub ppid: u32,
    /// `comm` from the stat line; can contain spaces/parens in theory.
    pub name: String,
    pub state: char,
    /// CPU ticks spent in user + kernel mode since process start.
    pub utime: u64,
    pub stime: u64,
    pub nice: i32,
    pub rss_kb: u64,
    pub uid: u32,
    pub start_secs: u64,
    pub exe: Option<String>,
    pub environ: Option<Vec<u8>>,
    /// Raw `/proc/<pid>/cmdline` bytes (NUL-separated argv), if readable.
    pub cmdline: Option<Vec<u8>>,
    /// True if this process holds an fd pointing into /dev/dri (active DRM
    /// usage).
    pub has_dri_fd: bool,
    /// Raw `/proc/<pid>/cgroup` contents (v2 unified line), when readable.
    /// Used for Flatpak detection (the scope name carries the app id).
    pub cgroup: Option<String>,
}

/// Scan all process directories under `root`.
pub fn scan_proc(root: &Path, btime: u64) -> Vec<ProcEntry> {
    let mut out = Vec::new();
    let ticks = clock_ticks();
    let Ok(rd) = fs::read_dir(root) else {
        return out;
    };
    for ent in rd.flatten() {
        let Some(name) = ent.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if !name.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let pid: u32 = match name.parse() {
            Ok(p) => p,
            Err(_) => continue,
        };
        let dir = ent.path();
        if let Some(mut entry) = read_proc_entry(&dir, pid, btime, ticks) {
            // Split the expensive parts out so a partial failure (env not
            // readable for other users' processes) doesn't lose the rest.
            entry.exe = read_link_opt(&dir.join("exe"));
            entry.environ = read_environ(&dir);
            entry.cmdline = read_file_opt(&dir.join("cmdline"));
            entry.has_dri_fd = has_dri_fd(&dir);
            entry.cgroup = read_cgroup(&dir);
            out.push(entry);
        }
    }
    out.sort_by_key(|e| e.pid);
    out
}

/// Read boot time from `/proc/stat` ("btime N" line).
pub fn read_btime(root: &Path) -> u64 {
    let Ok(text) = fs::read_to_string(root.join("stat")) else {
        return 0;
    };
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("btime ") {
            if let Ok(v) = rest.trim().parse::<u64>() {
                return v;
            }
        }
    }
    0
}

fn read_proc_entry(dir: &Path, pid: u32, btime: u64, ticks: u64) -> Option<ProcEntry> {
    let stat = fs::read_to_string(dir.join("stat")).ok()?;
    let (name, state, ppid, utime, stime, nice, starttime) = parse_stat(&stat)?;

    let rss_kb = read_rss_kb(dir);
    let uid = read_uid(dir);
    let start_secs = btime.saturating_add(starttime / ticks);

    Some(ProcEntry {
        pid,
        ppid,
        name,
        state,
        utime,
        stime,
        nice,
        rss_kb,
        uid,
        start_secs,
        exe: None,
        environ: None,
        cmdline: None,
        has_dri_fd: false,
        cgroup: None,
    })
}

/// Parse a `/proc/<pid>/stat` line.  `comm` may itself contain `(` and `)`,
/// so comm is everything between the FIRST `(` and the LAST `)`.
pub fn parse_stat(line: &str) -> Option<(String, char, u32, u64, u64, i32, u64)> {
    let open = line.find('(')?;
    let rparen = line.rfind(')')?;
    if rparen <= open {
        return None;
    }
    let comm = line[open + 1..rparen].trim().to_string();
    let fields: Vec<&str> = line[rparen + 1..].split_whitespace().collect();
    if fields.len() <= STAT_IDX_STARTTIME {
        return None;
    }
    let state = fields[STAT_IDX_STATE].chars().next()?;
    let ppid = fields[STAT_IDX_PPID].parse().ok()?;
    let utime = fields[STAT_IDX_UTIME].parse().ok()?;
    let stime = fields[STAT_IDX_STIME].parse().ok()?;
    let nice = fields[STAT_IDX_NICE].parse().ok()?;
    let starttime = fields[STAT_IDX_STARTTIME].parse().ok()?;
    Some((comm, state, ppid, utime, stime, nice, starttime))
}

fn read_rss_kb(dir: &Path) -> u64 {
    // statm: field 1 = total virtual size, field 2 = resident set size
    // (pages).  RSS is what matters — VSZ can be hundreds of GB for
    // Wine/Chromium-style processes and is meaningless here.
    let Ok(text) = fs::read_to_string(dir.join("statm")) else {
        return 0;
    };
    let pages: u64 = text
        .split_whitespace()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let page_kb = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64 / 1024;
    pages.saturating_mul(page_kb)
}

fn read_uid(dir: &Path) -> u32 {
    let Ok(text) = fs::read_to_string(dir.join("status")) else {
        return u32::MAX;
    };
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("Uid:") {
            if let Some(first) = rest.split_whitespace().next() {
                return first.parse().unwrap_or(u32::MAX);
            }
        }
    }
    u32::MAX
}

fn read_link_opt(path: &Path) -> Option<String> {
    fs::read_link(path).ok().map(|p| p.to_string_lossy().into_owned())
}

/// Best-effort read of `/proc/<pid>/cmdline` (NUL-separated argv), capped at
/// the same budget as the environment block.
fn read_file_opt(path: &Path) -> Option<Vec<u8>> {
    let mut file = fs::File::open(path).ok()?;
    use std::io::Read;
    let mut buf = vec![0u8; ENVIRON_MAX_BYTES];
    let n = file.read(&mut buf).ok()?;
    buf.truncate(n);
    Some(buf)
}

/// Read the comm (field 2, in parentheses) of a live pid — the sweep path
/// needs it to refuse DE-critical processes without a full entry.
pub fn comm_of(pid: u32) -> Option<String> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let open = stat.find('(')?;
    let close = stat[open..].find(')')?;
    Some(stat[open + 1..open + close].to_string())
}

/// Read the process's cgroup membership; the unified (v2) line is `0::/…`.
fn read_cgroup(dir: &Path) -> Option<String> {
    let text = fs::read_to_string(dir.join("cgroup")).ok()?;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("0::") {
            return Some(line.to_string());
        }
    }
    None
}

/// Best-effort read of the process environment block (NUL-separated
/// `KEY=value` entries).  `None` when unreadable or locked.
fn read_environ(dir: &Path) -> Option<Vec<u8>> {
    let mut file = fs::File::open(dir.join("environ")).ok()?;
    use std::io::Read;
    let mut buf = vec![0u8; ENVIRON_MAX_BYTES];
    let n = file.read(&mut buf).ok()?;
    buf.truncate(n);
    Some(buf)
}

/// Scan `/proc/<pid>/fd` for a symlink pointing into `/dev/dri`.
pub fn has_dri_fd(dir: &Path) -> bool {
    let Ok(rd) = fs::read_dir(dir.join("fd")) else {
        return false;
    };
    for ent in rd.flatten() {
        let target = fs::read_link(ent.path());
        if let Ok(t) = target {
            if t.to_string_lossy().starts_with("/dev/dri/") {
                return true;
            }
        }
    }
    false
}

/// Resolve a uid to a username (cached), falling back to the numeric uid.
pub fn uid_to_user(uid: u32) -> String {
    static CACHE: OnceLock<std::sync::Mutex<HashMap<u32, String>>> =
        OnceLock::new();
    let cache = CACHE.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    {
        if let Some(name) = cache.lock().unwrap().get(&uid) {
            return name.clone();
        }
    }
    let name = lookup_passwd(uid);
    cache.lock().unwrap().insert(uid, name.clone());
    name
}

fn lookup_passwd(uid: u32) -> String {
    #[cfg(unix)]
    {
        let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
        let mut buf = vec![0u8; 4096];
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        let rc = unsafe {
            libc::getpwuid_r(
                uid,
                &mut pw,
                buf.as_mut_ptr() as *mut libc::c_char,
                buf.len(),
                &mut result,
            )
        };
        if rc == 0 && !result.is_null() && !pw.pw_name.is_null() {
            let name = unsafe { std::ffi::CStr::from_ptr(pw.pw_name) };
            if let Ok(s) = name.to_str() {
                return s.to_string();
            }
        }
    }
    uid.to_string()
}

/// Compute a ProcessInfo's cpu% from tick deltas, expressed as a percentage
/// of TOTAL system capacity (matching GNOME/KDE system monitors): a process
/// pegging one whole core reads 100/ncpu %, not 100 %.
pub fn compute_cpu_percent(
    prev_ticks: Option<(u64, u64)>,
    cur_ticks: (u64, u64),
    elapsed_secs: f64,
    ncpu: usize,
) -> f32 {
    // Newly seen processes start at 0% until their first delta.
    let Some((p_u, p_s)) = prev_ticks else {
        return 0.0;
    };
    let delta = (cur_ticks.0.saturating_sub(p_u) + cur_ticks.1.saturating_sub(p_s)) as f64;
    if delta <= 0.0 || elapsed_secs <= 0.0 || ncpu == 0 {
        return 0.0;
    }
    // Delta ticks over the interval, relative to one core, then scaled to
    // % of total capacity.
    let per_core = (delta / clock_ticks() as f64) / elapsed_secs;
    (per_core * 100.0 / ncpu as f64).min(100.0) as f32
}

/// Read the aggregate `cpu` line of `/proc/stat` as (busy, total) ticks.
/// System-wide CPU% ver = delta busy / delta total; use it in the same tick
/// `compute_cpu_percent` does.  (0,0) whenever the file is unreadable.
pub fn read_cpu_busy(root: &Path) -> (u64, u64) {
    let Ok(text) = fs::read_to_string(root.join("stat")) else {
        return (0, 0);
    };
    let Some(line) = text.lines().find(|l| l.starts_with("cpu ")) else {
        return (0, 0);
    };
    let nums: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|v| v.parse().ok())
        .collect();
    if nums.len() < 4 {
        return (0, 0);
    }
    // Field order: user nice system idle iowait irq softirq steal ...
    let idle = nums[3] + nums.get(4).copied().unwrap_or(0);
    let total: u64 = nums.iter().sum();
    (total.saturating_sub(idle), total)
}

/// % of total capacity from two (busy, total) samples.
pub fn compute_system_cpu(prev: Option<(u64, u64)>, cur: (u64, u64)) -> f32 {
    let Some((p_b, p_t)) = prev else {
        return 0.0;
    };
    let busy = cur.0.saturating_sub(p_b);
    let total = cur.1.saturating_sub(p_t);
    if busy == 0 || total == 0 {
        return 0.0;
    }
    ((busy as f64 / total as f64) * 100.0).min(100.0) as f32
}

/// (total, used) physical memory in KiB from /proc/meminfo.  Used = total -
/// available (available is what the kernel really estimates as usable by
/// apps, so cache does not count as "used").
pub fn read_system_mem(root: &Path) -> (u64, u64) {
    let Ok(text) = fs::read_to_string(root.join("meminfo")) else {
        return (0, 0);
    };
    let mut total = 0u64;
    let mut available = 0u64;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            total = parse_meminfo_kb(rest);
        } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
            available = parse_meminfo_kb(rest);
        }
    }
    if total == 0 {
        return (0, 0);
    }
    (total, total.saturating_sub(available.min(total)))
}

fn parse_meminfo_kb(rest: &str) -> u64 {
    rest.split_whitespace()
        .next()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

#[test]
fn comm_of_our_own_process() {
    let comm = comm_of(std::process::id()).expect("own pid readable");
    assert!(!comm.is_empty());
    assert!(comm.len() < 32, "comm is capped at 15 chars + nul");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;

    fn fixture_proc_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "nicewatch-procfix-{tag}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_stat(dir: &Path, pid: u32, comm: &str, state: char, ppid: u32, utime: u64, stime: u64, nice: i32, starttime: u64) {
        fs::create_dir_all(dir.join(pid.to_string())).unwrap();
        // 1 pid, 2 comm, 3 state, 4 ppid ... 14 utime, 15 stime, 19 nice, 22 starttime
        let mut fields: Vec<String> = (1..=22).map(|f| match f {
            3 => state.to_string(),
            4 => ppid.to_string(),
            14 => utime.to_string(),
            15 => stime.to_string(),
            19 => nice.to_string(),
            22 => starttime.to_string(),
            _ => (f * 10).to_string(),
        }).collect();
        // field 2 is comm (with parens inside the parens) handled below
        fields[1] = format!("({comm})");
        let line = format!("{} {} {}\n", pid, fields[1], fields[2..].join(" "));
        let mut f = fs::File::create(dir.join(format!("{pid}/stat"))).unwrap();
        f.write_all(line.as_bytes()).unwrap();
    }

    #[test]
    fn parse_stat_handles_parens_and_comm_spaces() {
        // "4242 (foo) (bar) baz" — only the LAST ')' ends the comm.
        let line = "4242 (foo ) (bar) R 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21";
        let (comm, state, ppid, utime, stime, nice, starttime) = parse_stat(line).unwrap();
        assert_eq!(comm, "foo ) (bar");
        assert_eq!(state, 'R');
        assert_eq!(ppid, 1);
        assert_eq!(utime, 11);
        assert_eq!(stime, 12);
        assert_eq!(nice, 16);
        assert_eq!(starttime, 19);
    }

    #[test]
    fn scan_fixture_finds_processes_and_flags() {
        let root = fixture_proc_dir("scan");
        fs::create_dir_all(root.join("1234/fd")).unwrap();
        fs::create_dir_all(root.join("9999/fd")).unwrap();
        write_stat(&root, 1234, "firefox-bin", 'S', 1, 100, 50, 5, 2000);
        write_stat(&root, 9999, "game", 'S', 1, 90, 70, 19, 3000);
        fs::write(root.join("1234/statm"), "500000 200 0 0 0 0 0\n").unwrap();
        fs::write(root.join("1234/status"), "Uid:\t1000\t1000\t1000\t1000\n").unwrap();
        std::os::unix::fs::symlink("/dev/dri/card0", root.join("9999/fd/5")).unwrap();
        fs::write(root.join("9999/environ"), b"STEAM_COMPAT_DATA_PATH=/x\x00SteamAppId=123\x00").unwrap();
        fs::write(
            root.join("stat"),
            "cpu  1 2 3 4 5 6 7 8\nbtime 1700000000\n",
        )
        .unwrap();

        let entries = scan_proc(&root, read_btime(&root));
        assert_eq!(entries.len(), 2);
        let fx = entries.iter().find(|e| e.pid == 1234).unwrap();
        assert_eq!(fx.name, "firefox-bin");
        assert_eq!(fx.nice, 5);
        assert_eq!(fx.uid, 1000);
        assert_eq!(fx.rss_kb, 200 * 4);
        assert!(!fx.has_dri_fd);
        let game = entries.iter().find(|e| e.pid == 9999).unwrap();
        assert!(game.has_dri_fd);
        assert!(game.environ.as_ref().is_some());
        assert_eq!(game.start_secs, 1700000000 + 3000 / clock_ticks());
    }

    #[test]
    fn non_pid_dirs_are_skipped() {
        let root = fixture_proc_dir("names");
        fs::create_dir_all(root.join("notanumber")).unwrap();
        write_stat(&root, 42, "sys", 'S', 1, 1, 1, 0, 1);
        let entries = scan_proc(&root, 0);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].pid, 42);
    }

    #[test]
    fn cpu_percent_is_delta_based_and_scaled_to_total_capacity() {
        // 200 ticks of work over a 1s interval on 8 cores = 25% of total
        // capacity (a system monitor would read 25%, not 200%).
        let pct = compute_cpu_percent(Some((0, 0)), (100, 100), 1.0, 8);
        assert!((pct - 25.0).abs() < 0.5, "pct={pct}");
        // One core fully pegged on an 8-core box is 12.5% of capacity.
        let single = compute_cpu_percent(Some((0, 0)), (100, 0), 1.0, 8);
        assert!((single - 12.5).abs() < 0.5, "single={single}");
        // No work -> 0.
        assert_eq!(compute_cpu_percent(Some((100, 100)), (100, 100), 1.0, 8), 0.0);
        // Newly seen process -> 0 until first delta.
        assert_eq!(compute_cpu_percent(None, (100, 100), 1.0, 8), 0.0);
    }

    #[test]
    fn system_cpu_delta_and_stat_parsing() {
        let root = fixture_proc_dir("cpustat");
        fs::write(
            root.join("stat"),
            "cpu  10 0 5 80 5 1 2 0\ncpu0 2 0 1 40 0 0 1 0\nbtime 100\n",
        )
        .unwrap();
        // busy = total - idle(80+5);  total = 103;  busy = 18.
        let (busy, total) = read_cpu_busy(&root);
        assert_eq!((busy, total), (18, 103));
        // Delta across polls: 5 busy of 10 total -> 50%.
        let cur = (busy + 5, total + 10);
        let pct = compute_system_cpu(Some((busy, total)), cur);
        assert!((pct - 50.0).abs() < 0.5, "pct={pct}");
        assert_eq!(compute_system_cpu(None, cur), 0.0);
    }

    #[test]
    fn uid_fallback_is_numeric() {
        assert_eq!(uid_to_user(u32::MAX), u32::MAX.to_string());
    }
}