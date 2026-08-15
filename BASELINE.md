# Nicewatch baseline reference (for before/after comparisons)

Captured 2026-08-14 (after session reboot at 16:41, daemon restarted pid 49454,
log /tmp/nwtest/real-run4.log).

## 1. Machine facts (stable, not affected by apps)
- CPU: AMD Ryzen 5 5500, 12 threads, 31GB RAM. NOT a VM (steal=0).
- CONTENTION: user-1000.slice capped at CPUQuota=1020% (10.2 of 12 cores) by
  /etc/systemd/system/user-1000.slice.d/50-resourcecap.conf (unrelated to
  nicewatch). Under full load the WHOLE desktop incl. OBS throttles at 10.2
  cores. Lifting this cap gives weights the full 12 threads to arbitrate.
- cgroup v2 base: /sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/app.slice
- Kernel silently ignores CPU burst (cpu.max 3rd field accepted, never applied).

## 2. Expected behavior (what to verify when apps open again)
Under contention (demand > capacity) CPU should split ~proportionally to weight:
- OBS + obs-browser-pag: weight 1000 (top priority, never throttled by us)
- DELTARUNE.exe: 900 | VNyan.exe: 700
- Isolated Web Co (browser/Twitch): 300, nice 5
- vesktop.bin (Discord): 50, nice 10
- wineserver / ai.opencode.des: 30, nice 10
Memory: no caps set by nicewatch. No cpu.max caps set on any rule.

## 3. Baselines from earlier (PRE-reboot session, while daemon ran)
These used per-cgroup cpu.stat sampling which is authoritative.

During real stress (DELTARUNE + VNyan + OBS + browser, ~99.9% CPU busy):
- Total busy was ~10.7 of 10.2 cores (session hitting the quota).
- Attributions seen: VNyan ~2.2c, Discord ~2.1c, DELTARUNE ~1.3c, OBS ~1.1c,
  opencode ~1.2c, rest ~2.3c. NOTE these were sampled DURING the cap; the
  quota was throttling (nr_throttled 30641, ~978s throttled) — part of the
  observed stutter is the cap, not the weights.

Weight calibration (synthetic load, weights not fully linear):
- hi w=1000 vs lo w=100 (20 vs 20 procs): ratio 5.06 (fair would be 1.0,
  nominal weight-linear would be 10) -> weights clearly act, not linear.
- hi w=2000 vs lo w=200: ratio 5.28.
- Same tests under session quota throttling (~850% busy of 1200% possible).

## 4. Known wart
- Daemon runs non-root: can only RAISE nice (10->11), never LOWER (10->0,
  EACCES 13). OBS starts at nice 0 so rule nice=0 is a no-op -> safe.
- cpu.weight writes work unprivileged -> the real protection.

## 5. How to re-measure (the A/B for the next live window)
   B=/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/app.slice
   while apps run, sample each managed dir's cpu.stat usage_usec delta over ~6s:
   - % of one core per dir = usage_usec_delta / 6s * 100
   - share of busy % = dir / sum(all)
   Then compare: OBS(app-weight 1000) share vs Discord(50) vs browser(300);
   OBS should dominate under saturation and NEVER show throttling at our layer.

   Also record: grep nr_periods/nr_throttled app.slice/obs/cpu.stat (ours is 0
   by design - no caps), and user-1000.slice quota throttles (the cap's fault).