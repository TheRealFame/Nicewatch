<script lang="ts">
  import type { ProcessInfo, Tier } from "./api";
  import { appMeta, type AppMeta } from "./api";
  import { TIER_ORDER } from "./api";
  import TierDropdown from "./TierDropdown.svelte";

  let { rows, onPick } = $props<{
    rows: ProcessInfo[];
    onPick: (pid: number, tier: Tier) => void;
  }>();

  // ----------------------------------------------------------------------
  // App grouping: one row per application (exe path), children collapsible.
  // ----------------------------------------------------------------------

  interface Group {
    key: string;
    exe: string | null;
    label: string;
    meta: AppMeta;
    members: ProcessInfo[];
    cpu: number;
    memKb: number;
    tier: Tier | null;
    flagged: boolean;
    capped: boolean;
  }

  const metaCache = new Map<string, AppMeta>();
  // Bumped when an async icon/name resolves so the derived groups rebuild.
  let metaVersion = $state(0);

  function groupKey(p: ProcessInfo): string {
    return p.exe || `name:${p.name}`;
  }

  function buildGroups(): Group[] {
    void metaVersion;
    const byKey = new Map<string, Group>();
    for (const p of rows) {
      const key = groupKey(p);
      let g = byKey.get(key);
      if (!g) {
        g = {
          key,
          exe: p.exe,
          label: "",
          meta: { name: p.name, icon: null },
          members: [],
          cpu: 0,
          memKb: 0,
          tier: null,
          flagged: false,
          capped: false,
        };
        byKey.set(key, g);
        void resolveMeta(g);
      }
      g.members.push(p);
    }
    const groups = [...byKey.values()];
    for (const g of groups) {
      g.cpu = g.members.reduce((s, p) => s + p.cpu_percent, 0);
      g.memKb = g.members.reduce((s, p) => s + p.mem_kb, 0);
      g.tier = g.members[0]?.tier ?? null;
      g.flagged = g.members.some((p) => p.game_detected);
      g.capped = g.members.some((p) => p.capped);
      g.label = prettyLabel(g.meta.name, g.members[0]?.name);
    }
    return groups;
  }

  function resolveMeta(g: Group) {
    const key = g.key;
    const cached = metaCache.get(key);
    if (cached) {
      g.meta = cached;
      g.label = prettyLabel(cached.name, g.members[0]?.name);
      return;
    }
    void appMeta(key).then((m) => {
      metaCache.set(key, m);
      g.meta = m;
      g.label = prettyLabel(m.name, g.members[0]?.name);
      metaVersion += 1;
    });
  }

  function prettyLabel(metaName: string, fallback: string | undefined): string {
    return metaName && metaName !== fallback ? metaName : fallback ?? metaName;
  }

  const groups = $derived(buildGroups());

  // ----------------------------------------------------------------------
  // Expand/collapse: small apps start expanded, big ones collapsed.
  // ----------------------------------------------------------------------
  const collapsed = $state(new Map<string, boolean>());
  const defaultCollapsed = $derived(new Set(groups.filter((g) => g.members.length > 4).map((g) => g.key)));

  function isCollapsed(g: Group): boolean {
    const c = collapsed.get(g.key);
    return c !== undefined ? c : defaultCollapsed.has(g.key);
  }

  // ----------------------------------------------------------------------
  // Virtualized flat row list (uniform 36px rows).
  // ----------------------------------------------------------------------
  const ROW_H = 36;

  let container = $state<HTMLDivElement | null>(null);
  let viewport = $state(600);
  let scrollTop = $state(0);

  $effect(() => {
    const el = container;
    if (!el) return;
    const ro = new ResizeObserver(() => {
      viewport = el.clientHeight;
    });
    ro.observe(el);
    return () => ro.disconnect();
  });

  interface FlatRow {
    kind: "group" | "proc";
    group: Group;
    proc?: ProcessInfo;
  }

  const flat = $derived.by(() => {
    const out: FlatRow[] = [];
    for (const g of groups) {
      out.push({ kind: "group", group: g });
      if (!isCollapsed(g)) {
        for (const p of g.members) {
          out.push({ kind: "proc", group: g, proc: p });
        }
      }
    }
    return out;
  });

  let startIdx = $derived(Math.max(0, Math.floor(scrollTop / ROW_H) - 4));
  let endIdx = $derived(
    Math.min(flat.length, Math.ceil((scrollTop + viewport) / ROW_H) + 4),
  );
  let visible = $derived(flat.slice(startIdx, endIdx));

  // ----------------------------------------------------------------------
  // Tier selection (debounced, per group or per process).
  // ----------------------------------------------------------------------
  const debouncers = new Map<string, number>();

  function pickGroup(g: Group, tier: Tier) {
    const key = `g${g.key}`;
    const existing = debouncers.get(key);
    if (existing) window.clearTimeout(existing);
    debouncers.set(
      key,
      window.setTimeout(() => {
        debouncers.delete(key);
        for (const p of g.members) onPick(p.pid, tier);
      }, 400),
    );
  }

  function pickProc(pid: number, tier: Tier) {
    const key = `p${pid}`;
    const existing = debouncers.get(key);
    if (existing) window.clearTimeout(existing);
    debouncers.set(
      key,
      window.setTimeout(() => {
        debouncers.delete(key);
        onPick(pid, tier);
      }, 400),
    );
  }

  function toggle(g: Group) {
    collapsed.set(g.key, !isCollapsed(g));
  }

  function fmtMem(kb: number): string {
    if (kb >= 1024 * 1024) return (kb / 1048576).toFixed(1) + " G";
    if (kb >= 1024) return (kb / 1024).toFixed(1) + " M";
    return String(kb);
  }

  function cpuWidth(pct: number): string {
    return `${Math.min(Math.max(pct, 0), 100)}%`;
  }

  function ioTitle(p: ProcessInfo): string {
    return p.ionice_class === "none"
      ? "ionice: kernel default"
      : `ionice: ${p.ionice_class}/${p.ionice_priority}`;
  }
</script>

<div
  class="table"
  bind:this={container}
  onscroll={(e) => (scrollTop = (e.currentTarget as HTMLDivElement).scrollTop)}
>
  {#if flat.length === 0}
    <div class="empty">No processes match</div>
  {:else}
    <div class="spacer" style="height: {flat.length * ROW_H}px;">
      {#each visible as r, i (r.kind === "group" ? `g:${r.group.key}` : `p:${r.proc!.pid}`)}
        {#if r.kind === "group"}
          {@const g = r.group}
          <div
            class="row group-row"
            class:flagged={g.flagged}
            class:open={!isCollapsed(g)}
            style="transform: translateY({(startIdx + i) * ROW_H}px)"
            role="button"
            tabindex="0"
            onclick={() => toggle(g)}
            onkeydown={(e) => {
              if (e.key === "Enter" || e.key === " ") {
                e.preventDefault();
                toggle(g);
              }
            }}
            title={`${g.members.length} process(es) — click to ${isCollapsed(g) ? "expand" : "collapse"}`}
          >
            <span class="cell chev">
              <svg class="chev-svg" width="11" height="11" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round">
                <path d="m6 9 6 6 6-6" />
              </svg>
            </span>
            <span class="cell name">
              {#if g.meta.icon}
                <img class="app-ic" src={g.meta.icon} alt="" loading="lazy" />
              {:else}
                <span class="app-ic ph">{g.label.slice(0, 1).toUpperCase()}</span>
              {/if}
              <span class="nm strong">{g.label}</span>
              <span class="child-count">{g.members.length}</span>
              {#if g.flagged}
                <span class="badge" title="Auto-detected as a game (Steam env / DRM fd heuristic)">game?</span>
              {/if}
              {#if g.capped}
                <span class="badge cap" title="At least one process here lives in a Nicewatch-managed cgroup — its CPU/RAM limits are binding right now">capped</span>
              {/if}
            </span>
            <span class="cell cpu" title="Aggregate CPU usage as % of total capacity — 9% means roughly one of your 12 cores.">
              <span class="barwrap">
                <span class="bar" style="width: {cpuWidth(g.cpu)};"></span>
              </span>
              <span class="val">{g.cpu.toFixed(1)}%</span>
            </span>
            <span class="cell nice">—</span>
            <span class="cell mem">{fmtMem(g.memKb)}</span>
            <span class="cell tier" role="presentation" onclick={(e) => e.stopPropagation()}>
              <TierDropdown value={g.tier} onPick={(t) => pickGroup(g, t)} />
            </span>
          </div>
        {:else}
          {@const p = r.proc!}
          <div
            class="row proc-row"
            class:flagged={p.game_detected}
            style="transform: translateY({(startIdx + i) * ROW_H}px)"
            title={p.exe ?? p.name}
          >
            <span class="cell chev"></span>
            <span class="cell name">
              <span class="nm">{p.name}</span>
              {#if p.game_detected}
                <span class="badge" title="Auto-detected as a game (Steam env / DRM fd heuristic)">game?</span>
              {/if}
              {#if p.capped}
                <span class="badge cap" title="Moved into a Nicewatch cgroup — its CPU/RAM limits are actually binding right now">capped</span>
              {/if}
              <span class="user">{p.user}</span>
            </span>
            <span class="cell cpu" title={ioTitle(p) + ". Tiers change scheduling priority (nice); rules with a cgroup cap (see the 'capped' badge) are the only way to actually limit CPU."}>
              <span class="barwrap">
                <span class="bar" style="width: {cpuWidth(p.cpu_percent)};"></span>
              </span>
              <span class="val">{p.cpu_percent.toFixed(1)}%</span>
            </span>
            <span class="cell nice">{p.nice}</span>
            <span class="cell mem">{fmtMem(p.mem_kb)}</span>
            <span class="cell tier" role="presentation" onclick={(e) => e.stopPropagation()}>
              <TierDropdown value={p.tier} onPick={(t) => pickProc(p.pid, t)} />
            </span>
          </div>
        {/if}
      {/each}
    </div>
  {/if}
</div>

<style>
  .table {
    flex: 1;
    overflow-y: auto;
    position: relative;
  }
  .spacer {
    position: relative;
  }
  /* Column layout matches the header grid in App.svelte. */
  .row {
    position: absolute;
    top: 0;
    left: 0;
    right: 0;
    height: 36px;
    display: grid;
    grid-template-columns: 24px 1fr 130px 60px 90px 170px;
    gap: 6px;
    align-items: center;
    padding: 0 14px;
    border-bottom: 1px solid rgba(61, 61, 68, 0.5);
    font-size: 13px;
  }
  .group-row {
    background: var(--bg-alt);
    cursor: pointer;
  }
  .group-row:hover {
    background: var(--bg-raised);
  }
  .row.flagged {
    background: linear-gradient(90deg, var(--accent-soft), transparent 55%);
  }
  .cell {
    overflow: hidden;
    white-space: nowrap;
    text-overflow: ellipsis;
  }
  .chev {
    display: flex;
    align-items: center;
    justify-content: center;
  }
  .chev-svg {
    color: var(--text-faint);
    transition: transform 0.15s ease;
  }
  .group-row.open .chev-svg {
    transform: rotate(90deg);
  }
  .nice,
  .mem {
    font-variant-numeric: tabular-nums;
  }
  .name {
    display: flex;
    align-items: center;
    gap: 6px;
    min-width: 0;
  }
  .nm {
    overflow: hidden;
    text-overflow: ellipsis;
    flex-shrink: 0;
    max-width: 42%;
    font-weight: 500;
  }
  .nm.strong {
    font-weight: 700;
  }
  .app-ic {
    width: 20px;
    height: 20px;
    border-radius: 5px;
    object-fit: contain;
    flex-shrink: 0;
  }
  .app-ic.ph {
    background: var(--bg-raised);
    border: 1px solid var(--border);
    color: var(--text-dim);
    display: inline-flex;
    align-items: center;
    justify-content: center;
    font-size: 11px;
    font-weight: 700;
  }
  .child-count {
    flex-shrink: 0;
    min-width: 18px;
    height: 16px;
    padding: 0 5px;
    border-radius: 999px;
    background: var(--bg-raised);
    border: 1px solid var(--border);
    color: var(--text-dim);
    font-size: 10px;
    font-weight: 700;
    display: inline-flex;
    align-items: center;
    justify-content: center;
  }
  .user {
    color: var(--text-faint);
    font-size: 11.5px;
    margin-left: auto;
    overflow: hidden;
    text-overflow: ellipsis;
    max-width: 26%;
  }
  .badge {
    font-size: 9.5px;
    padding: 1px 7px;
    border-radius: 999px;
    background: var(--accent);
    color: #fff;
    font-weight: 700;
    letter-spacing: 0.4px;
    text-transform: uppercase;
    flex-shrink: 0;
  }
  .badge.cap {
    background: var(--amber);
    color: #1a1a1a;
  }
  .cpu {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .cpu .val {
    flex-shrink: 0;
    font-variant-numeric: tabular-nums;
    width: 44px;
    text-align: right;
  }
  .barwrap {
    flex: 1;
    height: 5px;
    border-radius: 999px;
    background: var(--bg-raised);
    border: 1px solid var(--border);
    overflow: hidden;
  }
  .bar {
    display: block;
    height: 100%;
    border-radius: 999px;
    background: var(--accent);
  }
  .tier {
    display: flex;
    align-items: center;
  }
  .empty {
    padding: 24px;
    color: var(--text-dim);
    text-align: center;
    font-size: 13px;
  }
</style>