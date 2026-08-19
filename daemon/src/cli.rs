//! CLI companion commands (`nicewatch ps`, `nicewatch rules`).
//!
//! `ps` connects to the running daemon's IPC socket and prints its current
//! view of the process table (the daemon is the authority — same data the
//! GUI renders).  `rules` resolves the config the same way the daemon does
//! (dual root/local sync) and prints the active rule set.  Both are
//! read-only: no state is touched, so they are safe alongside a live daemon.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use nicewatch_common::{ClientMsg, ProcessInfo, ServerMsg, Snapshot};

/// Connect to the daemon, say hello, and print the first snapshot it sends.
pub fn ps(socket: &Path) -> Result<(), String> {
    let mut stream = UnixStream::connect(socket)
        .map_err(|e| format!("cannot connect to daemon socket {}: {e} (is the daemon running?)", socket.display()))?;

    let mut reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
    // The daemon is obliged to send `Hello` first (protocol contract), then a
    // full `Snapshot` after we say hello.  Read until the snapshot arrives.
    let reader_handle = std::thread::spawn(move || -> Result<Snapshot, String> {
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => return Err("daemon closed the connection".to_string()),
                Ok(_) => {}
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            match nicewatch_common::decode_msg::<ServerMsg>(trimmed) {
                Ok(ServerMsg::Snapshot(s)) => return Ok(s),
                Ok(_) => {} // Hello / Diff / PromptGame frames: ignore
                Err(e) => return Err(format!("bad frame from daemon: {e}")),
            }
        }
    });
    let _ = stream.write_all(&nicewatch_common::encode_msg(&ClientMsg::Hello {
        client_kind: "cli".into(),
    }));
    let _ = stream.flush();

    let snapshot = match reader_handle.join() {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return Err(e),
        Err(_) => return Err("connection thread panicked".to_string()),
    };

    print_ps(&snapshot);
    Ok(())
}

fn print_ps(s: &Snapshot) {
    let mut rows: Vec<&ProcessInfo> = s.processes.iter().collect();
    rows.sort_by(|a, b| {
        b.cpu_percent
            .partial_cmp(&a.cpu_percent)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.name.cmp(&b.name))
    });
    println!(
        "{:>7}  {:<18} {:<10} {:>6} {:>8} {:>5}  {:<10} {}",
        "PID", "NAME", "USER", "CPU%", "MEM", "NICE", "TIER", "GAME"
    );
    for p in rows {
        let tier = p.tier.map(|t| t.label()).unwrap_or("");
        let game = if p.game_detected { "*" } else { "" };
        println!(
            "{:>7}  {:<18} {:<10} {:>6.1} {:>7.0}M {:>5}  {:<10} {}",
            p.pid,
            truncate(&p.name, 18),
            truncate(&p.user, 10),
            p.cpu_percent,
            p.mem_kb as f64 / 1024.0,
            p.nice,
            tier,
            game,
        );
    }
    println!("{} processes — poll {} ms", s.processes.len(), s.poll_interval_ms);
}

/// Resolve and print the active rule set (root config wins over local).
pub fn rules(root_cfg: &Path, local_cfg: &Path) -> Result<(), String> {
    let mut sync = crate::sync::Sync::new(
        root_cfg.to_path_buf(),
        local_cfg.to_path_buf(),
        Duration::from_millis(crate::sync::DEFAULT_PROMOTE_DEBOUNCE_MS),
        Duration::from_secs(1),
    );
    sync.initial_load(Instant::now());
    for w in sync.warnings.drain(..) {
        eprintln!("warning: {w}");
    }
    let set = crate::rules::RuleSet::from_config(&sync.active);
    println!(
        "{:<22} {:<22} {:<10} {:>6} {:<18} {}",
        "NAME", "MATCH", "TIER", "NICE", "IONICE", "CGROUP"
    );
    let mut all: Vec<_> = set.rules().collect();
    all.sort_by(|a, b| a.match_name.cmp(&b.match_name));
    for r in all {
        let tier = r.tier.map(|t| t.label().to_string()).unwrap_or_default();
        let ionice = r.ionice_class.map(|c| c.label().to_string()).unwrap_or_default();
        let cg = r.cgroup.as_ref().map(|c| {
            let mut parts = Vec::new();
            if let Some(w) = c.cpu_weight { parts.push(format!("w={w}")); }
            if let Some(p) = c.cpu_cap_percent { parts.push(format!("cap={p}%")); }
            if let Some(h) = &c.memory_high { parts.push(format!("high={h}")); }
            if let Some(m) = &c.memory_max { parts.push(format!("max={m}")); }
            if c.cpu_idle == Some(true) { parts.push("idle".to_string()); }
            parts.join(" ")
        }).unwrap_or_default();
        println!(
            "{:<22} {:<22} {:<10} {:>6} {:<18} {}",
            truncate(&r.name, 22),
            truncate(&r.match_name, 22),
            tier,
            r.nice.unwrap_or_default(),
            ionice,
            cg,
        );
    }
    Ok(())
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_keeps_short_and_ellipsizes_long() {
        assert_eq!(truncate("firefox", 18), "firefox");
        assert_eq!(truncate("SampleGame.exe_very_long", 10), "SampleGam…");
        assert_eq!(truncate("x", 1), "x");
    }
}
