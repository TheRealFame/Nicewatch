<script lang="ts">
  import { onMount } from "svelte";
  import { getCurrentWindow } from "@tauri-apps/api/window";
  import type { ProcessInfo, Tier } from "./lib/api";
  import { TIER_LABELS, tierLabel } from "./lib/api";
  import {
    fixRootConfig,
    setCap,
    setDaemonRunning,
    setGuiSettings,
    setPollInterval,
    setTier,
    installService,
    type Outcome,
  } from "./lib/api";
  import ProcessTable from "./lib/ProcessTable.svelte";
  import OptionDropdown from "./lib/OptionDropdown.svelte";
  import { state as ui, start } from "./lib/store.svelte.ts";
  import { applyNativeTheme } from "./lib/theme";

  type SortKey = "name" | "cpu" | "nice" | "mem" | "tier";
  type View = "processes" | "rules" | "settings";

  const win = getCurrentWindow();

  let view: View = $state("processes");
  let filter = $state("");
  let gamesOnly = $state(false);
  let sortKey: SortKey = $state("cpu");
  let sortDesc = $state(true);
  let installing = $state(false);
  let installDetail = $state<string | null>(null);
  // Warning banner shows the newest daemon warning until dismissed; a new
  // warning (different text) re-arms it.
  let dismissWarnKey = $state<string | null>(null);
  const currentWarn = $derived(ui.warnings.at(-1) ?? null);
  const showBanner = $derived(!!currentWarn && currentWarn !== dismissWarnKey);
  // Root-config warnings get a one-click Fix button.
  const isRootConfigWarn = $derived(
    showBanner && !!currentWarn && currentWarn.includes("CANNOT WRITE ROOT CONFIG"),
  );
  let fixing = $state(false);
  let fixDetail = $state<string | null>(null);
  let toggleBusy = $state(false);

  // Install/start the daemon from within the app (shown only while offline).
  async function doInstall() {
    installing = true;
    installDetail = null;
    try {
      const out = await installService();
      installDetail = out.detail || (out.ok ? "Service started" : "Install failed");
    } catch (e) {
      installDetail = String(e);
    } finally {
      installing = false;
    }
  }

  async function doFixRootConfig() {
    fixing = true;
    fixDetail = null;
    try {
      const out: Outcome = await fixRootConfig();
      fixDetail = out.detail;
    } catch (e) {
      fixDetail = String(e);
    } finally {
      fixing = false;
    }
  }

  async function toggleDaemon() {
    if (toggleBusy) return;
    toggleBusy = true;
    try {
      await setDaemonRunning(!ui.connected);
    } finally {
      toggleBusy = false;
    }
  }

  async function onPollIntervalChange(ms: number) {
    if (ms >= 250) {
      ui.pollIntervalMs = ms;
      try {
        await setPollInterval(ms);
      } catch (err) {
        console.error("set_poll_interval failed", err);
      }
    }
  }

  const pollOptions = [
    { value: "500", label: "500 ms" },
    { value: "1000", label: "1 s" },
    { value: "2000", label: "2 s" },
    { value: "5000", label: "5 s" },
    { value: "10000", label: "10 s" },
  ];

  function onSettingChange() {
    void setGuiSettings(ui.settings).catch((e) =>
      console.error("set_gui_settings failed", e),
    );
  }

  onMount(() => {
    void applyNativeTheme();
    start();
  });

  const cols: { key: SortKey; label: string; title: string }[] = [
    { key: "name", label: "Name", title: "Process name (comm)" },
    {
      key: "cpu",
      label: "CPU",
      title: "CPU usage as % of total capacity — 9% ≈ one of your 12 cores",
    },
    {
      key: "nice",
      label: "Nice",
      title:
        "Scheduling priority (Linux nice value, −20..19): lower = higher priority, 0 = normal.\nOur presets: Realtime −15 · Game −10 · Streaming −5 · Software +5.\nPositive numbers mean the process yields CPU to others.",
    },
    {
      key: "mem",
      label: "RAM",
      title: "Physical memory used (RSS), not virtual",
    },
    { key: "tier", label: "Tier", title: "Priority preset applied by Nicewatch" },
  ];

  function tierRank(t: Tier | null): number {
    return t
      ? t === "software"
        ? 0
        : t === "game"
          ? 1
          : t === "streaming"
            ? 2
            : 3
      : -1;
  }

  let rows = $derived.by(() => {
    const q = filter.trim().toLowerCase();
    const list = ui.processes.filter((p) => {
      if (gamesOnly && !p.game_detected) return false;
      if (!q) return true;
      return (
        p.name.toLowerCase().includes(q) ||
        String(p.pid).includes(q) ||
        p.user.toLowerCase().includes(q)
      );
    });
    const dir = sortDesc ? -1 : 1;
    return [...list].sort((a, b) => {
      switch (sortKey) {
        case "name":
          return a.name.localeCompare(b.name) * dir;
        case "cpu":
          return (a.cpu_percent - b.cpu_percent) * dir;
        case "nice":
          return (a.nice - b.nice) * dir;
        case "mem":
          return (a.mem_kb - b.mem_kb) * dir;
        case "tier":
          return tierRank(a.tier) - tierRank(b.tier) || b.pid - a.pid;
      }
    });
  });

  let detected = $derived(ui.processes.filter((p) => p.game_detected).length);

  function toggleSort(key: SortKey) {
    if (key === sortKey) {
      sortDesc = !sortDesc;
    } else {
      sortKey = key;
      sortDesc = key === "cpu" || key === "mem";
    }
  }

  /** Optimistic local update; the daemon confirms via the next diff. */
  function pickTier(pid: number, tier: Tier) {
    const p = ui.processes.find((x) => x.pid === pid);
    if (p) p.tier = tier;
    void setTier(pid, tier).catch((e) => console.error("set_tier failed", e));
  }

  function tierClass(t: Tier | null): string {
    return t ? `tier-${t}` : "tier-none";
  }

  // Per-rule editable cap state: the daemon is the source of truth (snapshots
  // overwrite), so all we do here is optimistic echo while the IPC round
  // trips, and validate the value on commit.
  interface CapDraft {
    text: string;
    dirty: boolean;
  }
  const capDrafts = new Map<string, CapDraft>();

  function capDraft(name: string, current: number | null): CapDraft {
    let d = capDrafts.get(name);
    if (!d) {
      d = { text: current?.toString() ?? "", dirty: false };
      capDrafts.set(name, d);
    }
    return d;
  }

  function commitCap(name: string, pct: number | null) {
    capDrafts.set(name, { text: pct?.toString() ?? "", dirty: false });
    void setCap(name, pct).catch((e) => console.error("set_cap failed", e));
  }

  function fmtMem(kb: number): string {
    if (kb >= 1024 * 1024) return (kb / 1048576).toFixed(1) + "G";
    if (kb >= 1024) return (kb / 1024).toFixed(0) + "M";
    return String(kb);
  }
</script>

<div class="shell">
  <!-- Custom title bar (no native GTK header: thin + single app name). -->
  <header class="tbar" data-tauri-drag-region>
    <div class="brand" data-tauri-drag-region>
      <span class="dot"></span>
      <span class="name">{ui.appInfo?.display ?? "…"}</span>
      <span class="ver" data-tauri-drag-region>v{ui.appInfo?.version ?? ""}</span>
    </div>
    <!-- Whole-system usage sits top-left, next to the brand. -->
    <span
      class="pill systat"
      class:hot={ui.systemCpu >= 70}
      data-tauri-drag-region
      title="Current system-wide CPU usage (all cores, averaged)"
    >
      <span class="cbar"><span class="cfill" style="width: {ui.systemCpu.toFixed(0)}%;"></span></span>
      System CPU {ui.systemCpu.toFixed(1)}%
    </span>
    <span
      class="pill systat"
      data-tauri-drag-region
      title="Used / total physical memory (MemTotal − MemAvailable)"
    >
      <span class="cbar"><span class="cfill mem" style="width: {ui.systemMemTotalKb > 0 ? Math.min(100, (ui.systemMemUsedKb / ui.systemMemTotalKb) * 100).toFixed(0) : 0}%;"></span></span>
      RAM {ui.systemMemTotalKb > 0 ? `${fmtMem(ui.systemMemUsedKb)} / ${fmtMem(ui.systemMemTotalKb)}` : "…"}
    </span>
    <div class="tbar-spacer" data-tauri-drag-region></div>
    <div class="status">
      {#if ui.connected}
        <span class:pill={true} class:ok={ui.connected} class:down={!ui.connected}>
          Daemon Connected
        </span>
        <span class="pill" title="Active priority rules">{ui.rules.length} rules</span>
      {:else}
        <span class="pill down">Daemon Offline</span>
        <button
          class="pill btn"
          disabled={installing}
          onclick={() => void doInstall()}
          title="Install and start the daemon as a per-user service"
        >
          {installing ? "Installing…" : "Install Service"}
        </button>
        {#if installDetail}
          <span class="pill detail" title={installDetail}>{installDetail}</span>
        {/if}
      {/if}
    </div>
    <div class="tbar-btns">
      <button class="icon-btn" title="Minimize" onclick={() => win.minimize()}>─</button>
      <button class="icon-btn" title="Close (hides to tray if enabled)" onclick={() => win.close()}>✕</button>
    </div>
  </header>

  {#if showBanner}
    <div class="warn-banner" role="alert">
      <span class="warn-msg">{currentWarn}</span>
      {#if isRootConfigWarn}
        <button class="pill btn fix-btn" disabled={fixing} onclick={() => void doFixRootConfig()}>
          {fixing ? "Fixing…" : "Fix Permissions"}
        </button>
      {/if}
      <button class="warn-x" onclick={() => (dismissWarnKey = currentWarn)} aria-label="Dismiss warning">✕</button>
    </div>
  {/if}
  {#if fixDetail}
    <div class="fix-detail" role="status">{fixDetail}</div>
  {/if}

  <div class="layout">
    <aside class="sidebar">
      <button
        class="nav"
        class:active={view === "processes"}
        type="button"
        onclick={() => (view = "processes")}
      >
        <svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round">
          <path d="M8 6h13M8 12h13M8 18h13M3 6h.01M3 12h.01M3 18h.01" />
        </svg>
        <span>Processes</span>
        {#if detected > 0}<span class="badge">{detected}</span>{/if}
      </button>
      <button
        class="nav"
        class:active={view === "rules"}
        type="button"
        onclick={() => (view = "rules")}
      >
        <svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round">
          <path d="M20.6 13.4 12 22 3 13V3h10l7.6 7.6a2 2 0 0 1 0 2.8Z" />
          <path d="M7 7h.01" />
        </svg>
        <span>Rules</span>
        {#if ui.rules.length > 0}<span class="badge">{ui.rules.length}</span>{/if}
      </button>
      <button
        class="nav"
        class:active={view === "settings"}
        type="button"
        onclick={() => (view = "settings")}
      >
        <svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
          <circle cx="12" cy="12" r="3" />
          <path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 1 1-4 0v-.09a1.65 1.65 0 0 0-1-1.51 1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 1 1 0-4h.09a1.65 1.65 0 0 0 1.51-1 1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33h.01a1.65 1.65 0 0 0 1-1.51V3a2 2 0 1 1 4 0v.09a1.65 1.65 0 0 0 1 1.51h.01a1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82v.01a1.65 1.65 0 0 0 1.51 1H21a2 2 0 1 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1Z" />
        </svg>
        <span>Settings</span>
      </button>

      <div class="sidebar-foot">
        <div class="stat"><span class="k">Processes</span><span class="v">{ui.processes.length}</span></div>
        <div class="stat"><span class="k">Detected Games</span><span class="v">{detected}</span></div>
      </div>
    </aside>

    <main>
      {#if view === "processes"}
        {#if !ui.connected}
          <!-- Daemon-off state: the whole point of the app requires it. -->
          <div class="card daemon-off">
            <div class="off-inner">
              <svg width="46" height="46" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" class="off-ic">
                <circle cx="12" cy="12" r="10" />
                <path d="M8 12h8" />
              </svg>
              <h1 class="off-title">The Daemon Needs to Be Active</h1>
              <p class="off-sub">
                Nicewatch applies priority presets and limits from a small background daemon.
                Start it below (or close this dialog to keep using the tray) — the process list
                and rules refresh as soon as it connects.
              </p>
              <button
                class="primary off-btn"
                disabled={installing || toggleBusy}
                onclick={() => void (ui.connected ? toggleDaemon() : doInstall())}
              >
                {installing ? "Starting…" : "Start Daemon"}
              </button>
              {#if installDetail}
                <p class="off-detail">{installDetail}</p>
              {/if}
            </div>
          </div>
        {:else}
          <div class="toolbar">
            <div class="search">
              <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round">
                <circle cx="11" cy="11" r="7" />
                <path d="m21 21-4.3-4.3" />
              </svg>
              <input
                type="search"
                bind:value={filter}
                placeholder="Filter by name, PID, or user…"
                aria-label="Filter processes"
              />
            </div>
            <label class="switch">
              <input type="checkbox" bind:checked={gamesOnly} />
              <span>Auto-detected only</span>
            </label>
            <span
              class="count"
              title="Processes shown after filtering, out of all running processes"
            >
              Showing {rows.length} of {ui.processes.length} processes
            </span>
          </div>

          <div class="card table-card">
            <div class="grid head">
              <span class="th chev-th"></span>
              {#each cols as c (c.key)}
                <button
                  type="button"
                  class="th"
                  class:active={sortKey === c.key}
                  title={c.title}
                  onclick={() => toggleSort(c.key)}
                >
                  {c.label}{#if sortKey === c.key}<span class="dir">{sortDesc ? "▼" : "▲"}</span>{/if}
                </button>
              {/each}
            </div>
            <ProcessTable {rows} onPick={pickTier} />
          </div>
        {/if}
      {:else if view === "rules"}
        <div class="toolbar">
          <h2 class="view-title">Priority Rules</h2>
          <span class="count">{ui.rules.length} rule(s) · from /etc &amp; ~/.config</span>
        </div>
        <div class="card rules-card">
          {#if ui.rules.length === 0}
            <div class="empty">
              <p>No explicit rules yet.</p>
              <p class="dim italic">
                Set a tier for any process in the Processes view, or answer the
                game-detection dialog, and it gets persisted here automatically.
              </p>
            </div>
          {:else}
            <div class="rgrid head">
              <span>Name</span>
              <span>Match (Comm)</span>
              <span>Tier</span>
              <span>Nice</span>
              <span title="Hard CPU cap: percent of ONE core — type a value, Enter applies">Cap %</span>
            </div>
            {#each ui.rules as r (r.name)}
              {@const draft = capDraft(r.name, r.cpu_cap_percent)}
              <div class="rgrid row">
                <span class="rname">{r.name}</span>
                <span class="rmatch" title="Exact match against /proc/&lt;pid&gt;/comm">{r.match_name || "—"}</span>
                <span>
                  {#if r.tier}
                    <span class="pill {tierClass(r.tier)}">
                      {TIER_LABELS[r.tier]}
                    </span>
                  {:else}
                    <span class="pill">Manual</span>
                  {/if}
                </span>
                <span class="rnice">{r.nice ?? "—"}</span>
                <span class="rcap" title="Hard CPU cap: percent of one core. Type a value and press Enter to apply (e.g. 25 = a quarter of one core); empty + Enter removes the cap.">
                  <input
                    class="cap-input"
                    bind:value={draft.text}
                    placeholder="off"
                    title="CPU cap: percent of one core (1..=3200). Type a value and press Enter — this is the real hard limit. Empty the box and press Enter to remove it."
                    aria-label={`CPU cap for ${r.name}`}
                    onkeydown={(e) => {
                      if (e.key === "Enter") {
                        const v = draft.text.trim();
                        if (v === "") {
                          commitCap(r.name, null);
                        } else {
                          const n = Number(v);
                          if (Number.isFinite(n) && n >= 1 && n <= 3200) {
                            commitCap(r.name, Math.round(n));
                          }
                        }
                      } else if (e.key === "Escape") {
                        draft.text = r.cpu_cap_percent?.toString() ?? "";
                      }
                    }}
                    onblur={() => {
                      if (!capDrafts.has(r.name)) return;
                      const d = capDrafts.get(r.name)!;
                      if (d.text !== (r.cpu_cap_percent?.toString() ?? "")) {
                        const v = d.text.trim();
                        if (v === "") commitCap(r.name, null);
                        else {
                          const n = Number(v);
                          if (Number.isFinite(n) && n >= 1 && n <= 3200) {
                            commitCap(r.name, Math.round(n));
                          } else {
                            d.text = r.cpu_cap_percent?.toString() ?? "";
                          }
                        }
                      }
                    }}
                  />
                </span>
              </div>
            {/each}
          {/if}
        </div>
        <p class="hint dim italic">
          Precedence: explicit rule → detected game → default software.
          Tiers are safe CFS niceness (never SCHED_FIFO/RR); the Cap column is
          a real cgroup v2 hard limit — set one to genuinely throttle a
          process (e.g. 25 = a quarter of one core). Caps survive tier changes.
          Nice values are Linux scheduling priorities: −20 is highest priority,
          0 is normal, +19 is lowest — our presets use Realtime −15, Game −10,
          Streaming −5, Software +5. See README.
        </p>
      {:else}
        <div class="toolbar">
          <h2 class="view-title">Settings</h2>
        </div>

        <div class="settings">
          <section class="card set-card">
            <h3 class="set-title">Daemon</h3>
            <div class="set-row">
              <div class="set-text">
                <span class="set-label">Daemon Status</span>
                <span class="set-sub italic">
                  The daemon applies tiers and limits in the background. It runs as
                  a per-user systemd service ({ui.connected ? "connected — running" : "stopped"}).
                </span>
              </div>
              <button
                class="primary set-toggle"
                disabled={toggleBusy}
                onclick={() => void toggleDaemon()}
              >
                {ui.connected ? "Stop Daemon" : "Start Daemon"}
              </button>
            </div>
            <div class="set-row">
              <div class="set-text">
                <span class="set-label">Poll Interval</span>
                <span class="set-sub italic">
                  How often the daemon re-reads the process table and re-applies presets.
                  Lower = snappier tier changes, slightly more background work.
                </span>
              </div>
              <OptionDropdown
                options={pollOptions}
                value={String(ui.pollIntervalMs)}
                onPick={onPollIntervalChange}
              />
            </div>
          </section>

          <section class="card set-card">
            <h3 class="set-title">Window &amp; Tray</h3>
            <div class="set-row">
              <div class="set-text">
                <span class="set-label">Start in Tray</span>
                <span class="set-sub italic">
                  Launch hidden in the system tray instead of opening the window.
                  Use the tray menu to show it.
                </span>
              </div>
              <label class="switch">
                <input type="checkbox" bind:checked={ui.settings.start_in_tray} onchange={onSettingChange} />
                <span>Enabled</span>
              </label>
            </div>
            <div class="set-row">
              <div class="set-text">
                <span class="set-label">Minimize to Tray</span>
                <span class="set-sub italic">
                  Closing the window hides it to the tray instead of quitting the app.
                </span>
              </div>
              <label class="switch">
                <input type="checkbox" bind:checked={ui.settings.minimize_to_tray} onchange={onSettingChange} />
                <span>Enabled</span>
              </label>
            </div>
          </section>

          <section class="card set-card">
            <h3 class="set-title">Permissions</h3>
            <div class="set-row">
              <div class="set-text">
                <span class="set-label">Root Config Directory</span>
                <span class="set-sub italic">
                  The daemon prefers to promote your rules to
                  <code>/etc/proc-priority-daemon/rules.toml</code> so the CLI and other
                  users share them. If it cannot write there, one click below fixes the
                  ownership (a polkit dialog will ask for your password).
                </span>
              </div>
              <button
                class="set-toggle"
                disabled={fixing}
                onclick={() => void doFixRootConfig()}
              >
                {fixing ? "Fixing…" : "Fix Permissions"}
              </button>
            </div>
            {#if fixDetail}
              <p class="set-result" class:ok={!fixDetail.toLowerCase().includes("fail") && !fixDetail.toLowerCase().includes("cancel")}>
                {fixDetail}
              </p>
            {/if}
          </section>

          <section class="card set-card">
            <h3 class="set-title">About</h3>
            <div class="set-row">
              <div class="set-text">
                <span class="set-label">{ui.appInfo?.display ?? "Nicewatch"}</span>
                <span class="set-sub italic">
                  Version {ui.appInfo?.version ?? "?"} · Process priority manager for
                  gaming and streaming on Linux. Daemon binary:
                  <code>nicewatch</code>; GUI: <code>nicewatch-gui</code>.
                </span>
              </div>
            </div>
          </section>
        </div>
      {/if}
    </main>
  </div>
</div>

<style>
  .shell {
    display: flex;
    flex-direction: column;
    height: 100%;
    background: var(--bg);
  }

  /* ---- Title bar ---- */
  .tbar {
    display: flex;
    align-items: center;
    gap: 14px;
    height: var(--tbar-h);
    padding: 0 6px 0 14px;
    background: var(--bg-alt);
    border-bottom: 1px solid var(--border);
    flex-shrink: 0;
  }
  .brand {
    display: flex;
    align-items: center;
    gap: 9px;
    cursor: default;
  }
  .dot {
    width: 9px;
    height: 9px;
    border-radius: 50%;
    background: var(--accent);
  }
  .name {
    font-weight: 700;
    font-size: 13.5px;
    letter-spacing: 0.2px;
  }
  .ver {
    color: var(--text-faint);
    font-size: 11.5px;
  }
  .tbar-spacer {
    flex: 1;
    align-self: stretch;
  }
  .status {
    display: flex;
    gap: 8px;
    align-items: center;
  }
  .tbar-btns {
    display: flex;
    gap: 2px;
  }
  .tbar-btns .icon-btn:last-child:hover {
    background: var(--warn);
    color: #fff;
  }
  .systat {
    display: inline-flex;
    align-items: center;
    gap: 7px;
    margin-left: 4px;
    font-variant-numeric: tabular-nums;
  }
  .cbar {
    width: 52px;
    height: 7px;
    border-radius: 999px;
    background: var(--bg);
    border: 1px solid var(--border-strong);
    overflow: hidden;
  }
  .cfill {
    display: block;
    height: 100%;
    border-radius: 999px;
    background: var(--green);
    transition: width 0.9s linear;
  }
  .cfill.mem {
    background: var(--accent);
  }
  .systat.hot .cfill {
    background: var(--warn);
  }
  .fix-btn {
    flex: none;
  }
  .fix-detail {
    margin: 6px 12px 0;
    padding: 6px 12px;
    border-radius: 8px;
    border: 1px solid var(--border);
    background: var(--bg-raised);
    color: var(--text-dim);
    font-size: 12px;
  }

  /* ---- Layout: sidebar + content ---- */
  .layout {
    display: flex;
    flex: 1;
    min-height: 0;
  }
  .sidebar {
    width: 190px;
    flex-shrink: 0;
    display: flex;
    flex-direction: column;
    gap: 2px;
    padding: 10px 8px;
    background: var(--bg-alt);
    border-right: 1px solid var(--border);
  }
  .nav {
    display: flex;
    align-items: center;
    gap: 10px;
    width: 100%;
    text-align: left;
    padding: 9px 10px;
    border: none;
    background: none;
    border-radius: 8px;
    color: var(--text-dim);
    font-size: 13.5px;
    font-weight: 600;
  }
  .nav:hover {
    background: var(--bg-raised);
    color: var(--text);
  }
  .nav.active {
    background: var(--accent-soft);
    color: var(--accent);
  }
  .nav .badge {
    margin-left: auto;
    min-width: 20px;
    height: 19px;
    padding: 0 6px;
    border-radius: 999px;
    background: var(--bg-raised);
    border: 1px solid var(--border);
    color: var(--text-dim);
    font-size: 11px;
    font-weight: 700;
    display: inline-flex;
    align-items: center;
    justify-content: center;
  }
  .nav.active .badge {
    background: var(--accent);
    border-color: var(--accent);
    color: #fff;
  }
  .sidebar-foot {
    margin-top: auto;
    padding: 10px 10px 4px;
    border-top: 1px solid var(--border);
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .stat {
    display: flex;
    justify-content: space-between;
    font-size: 12px;
  }
  .stat .k {
    color: var(--text-dim);
  }
  .stat .v {
    font-variant-numeric: tabular-nums;
    color: var(--text);
  }

  /* ---- Content area ---- */
  main {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 12px;
    padding: 16px 18px 18px;
    background: var(--bg);
  }
  .toolbar {
    display: flex;
    align-items: center;
    gap: 14px;
    flex-shrink: 0;
  }
  .search {
    position: relative;
    display: flex;
    align-items: center;
  }
  .search svg {
    position: absolute;
    left: 10px;
    color: var(--text-faint);
    pointer-events: none;
  }
  .search input {
    width: 320px;
    padding-left: 32px;
  }
  .switch {
    display: flex;
    align-items: center;
    gap: 8px;
    color: var(--text-dim);
    font-size: 13px;
    cursor: pointer;
  }
  .switch input {
    accent-color: var(--accent);
  }
  .count {
    margin-left: auto;
    color: var(--text-dim);
    font-size: 12.5px;
  }
  .view-title {
    font-size: 15px;
    margin: 0;
  }
  .hint {
    font-size: 12px;
    margin: 4px 2px 0;
  }
  .dim {
    color: var(--text-dim);
  }
  .italic {
    font-style: italic;
  }
  code {
    font-family: ui-monospace, monospace;
    font-size: 0.92em;
    background: var(--bg-raised);
    border: 1px solid var(--border);
    border-radius: 5px;
    padding: 1px 5px;
  }

  /* ---- Cards ---- */
  .card {
    background: var(--card);
    border: 1px solid var(--border);
    border-radius: var(--radius);
    box-shadow: var(--shadow);
    min-height: 0;
    display: flex;
    flex-direction: column;
  }
  .table-card {
    flex: 1;
    overflow: hidden;
  }
  /* Columns: chevron | Name | CPU%(bar) | Nice | RAM | Tier */
  .grid {
    display: grid;
    grid-template-columns: 24px 1fr 130px 60px 90px 170px;
    gap: 6px;
    align-items: center;
  }
  .head {
    padding: 10px 14px 8px;
    border-bottom: 1px solid var(--border);
  }
  .th {
    background: none;
    border: none;
    padding: 0;
    text-align: left;
    font-size: 11px;
    font-weight: 700;
    letter-spacing: 0.5px;
    text-transform: uppercase;
    color: var(--text-faint);
  }
  .th:hover,
  .th.active {
    color: var(--text);
  }
  .th .dir {
    margin-left: 3px;
    font-size: 9px;
  }
  .chev-th {
    font-size: 0;
  }

  /* ---- Daemon-off state ---- */
  .daemon-off {
    flex: 1;
    align-items: center;
    justify-content: center;
  }
  .off-inner {
    max-width: 460px;
    text-align: center;
    padding: 30px;
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 6px;
  }
  .off-ic {
    color: var(--text-faint);
    margin-bottom: 8px;
  }
  .off-title {
    font-size: 20px;
    font-weight: 700;
    margin: 0;
  }
  .off-sub {
    color: var(--text-dim);
    font-size: 13.5px;
    line-height: 1.55;
    margin: 0 0 14px;
  }
  .off-btn {
    padding: 9px 26px;
    border-radius: var(--radius-lg);
    font-weight: 700;
    font-size: 14px;
  }
  .off-detail {
    color: var(--text-faint);
    font-size: 12px;
    margin: 10px 0 0;
    font-style: italic;
  }

  /* ---- Rules view ---- */
  .rules-card {
    flex-shrink: 0;
    overflow: hidden;
  }
  .rgrid {
    display: grid;
    grid-template-columns: 1fr 1fr 150px 70px 80px;
    gap: 8px;
    align-items: center;
    padding: 10px 14px;
  }
  .rgrid.head {
    color: var(--text-faint);
    font-size: 11px;
    font-weight: 700;
    letter-spacing: 0.5px;
    text-transform: uppercase;
    border-bottom: 1px solid var(--border);
    padding: 9px 14px;
  }
  .rgrid.row {
    font-size: 13px;
    border-bottom: 1px solid rgba(61, 61, 68, 0.5);
  }
  .rgrid.row:last-child {
    border-bottom: none;
  }
  .rgrid.row:hover {
    background: var(--bg-raised);
  }
  .rname {
    font-weight: 700;
  }
  .rmatch {
    color: var(--text-dim);
    font-family: ui-monospace, monospace;
    font-size: 12px;
  }
  .rnice {
    font-variant-numeric: tabular-nums;
  }
  .rcap input.cap-input {
    width: 100%;
    max-width: 76px;
    padding: 3px 6px;
    border-radius: 6px;
    border: 1px solid var(--border);
    background: var(--bg-raised);
    color: var(--text);
    font-size: 12.5px;
    font-variant-numeric: tabular-nums;
    text-align: right;
  }
  .rcap input.cap-input:focus {
    outline: none;
    border-color: var(--accent);
    box-shadow: 0 0 0 2px var(--accent-soft);
  }
  .rcap input.cap-input::placeholder {
    color: var(--text-faint);
  }
  .empty {
    padding: 34px 24px;
    text-align: center;
    color: var(--text);
  }
  .empty p {
    margin: 0 0 6px;
  }
  .empty .dim {
    font-size: 13px;
    max-width: 420px;
    margin: 0 auto;
  }

  /* ---- Settings view ---- */
  .settings {
    display: flex;
    flex-direction: column;
    gap: 12px;
    overflow-y: auto;
  }
  .set-card {
    flex-shrink: 0;
    padding: 16px 18px;
    gap: 4px;
  }
  .set-title {
    margin: 0 0 10px;
    font-size: 13px;
    font-weight: 700;
    letter-spacing: 0.4px;
    text-transform: uppercase;
    color: var(--text-dim);
  }
  .set-row {
    display: flex;
    align-items: center;
    gap: 18px;
    padding: 9px 0;
    border-top: 1px solid rgba(61, 61, 68, 0.5);
  }
  .set-row:first-of-type {
    border-top: none;
  }
  .set-text {
    flex: 1;
    display: flex;
    flex-direction: column;
    gap: 3px;
  }
  .set-label {
    font-weight: 700;
    font-size: 13.5px;
  }
  .set-sub {
    color: var(--text-dim);
    font-size: 12.5px;
    line-height: 1.5;
  }
  .set-input {
    min-width: 110px;
  }
  .set-toggle {
    flex-shrink: 0;
    border-radius: var(--radius-lg);
    font-weight: 600;
  }
  .set-result {
    margin: 6px 0 0;
    font-size: 12.5px;
    color: var(--warn);
    font-style: italic;
  }
  .set-result.ok {
    color: var(--green);
  }
</style>