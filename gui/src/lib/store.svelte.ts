// Reactive store fed by daemon events; the process list is updated via
// diffs only (the daemon sends the initial snapshot once per connection).
import { listen } from "@tauri-apps/api/event";
import { WebviewWindow } from "@tauri-apps/api/webviewWindow";
import {
  getAppInfo,
  getGuiSettings,
  getState,
  type AppInfo,
  type Diff,
  type GamePrompt,
  type GuiSettings,
  type ProcessInfo,
  type RuleInfo,
  type Snapshot,
} from "./api";
import { EVT_DIFF, EVT_HELLO, EVT_PROMPT, EVT_SNAPSHOT, EVT_WARN } from "./events";

/** Cap the warning log so an admitted root misconfiguration can't grow it unbounded. */
const MAX_WARNINGS = 50;

// Single reactive container; properties are proxied by `$state` so mutating
// them is legal (reassigning a module-level const binding is not).
export const state: {
  appInfo: AppInfo | null;
  connected: boolean;
  pollIntervalMs: number;
  systemCpu: number;
  systemMemTotalKb: number;
  systemMemUsedKb: number;
  processes: ProcessInfo[];
  rules: RuleInfo[];
  prompts: GamePrompt[];
  warnings: string[];
  settings: GuiSettings;
} = $state({
  appInfo: null,
  connected: false,
  pollIntervalMs: 2000,
  systemCpu: 0,
  systemMemTotalKb: 0,
  systemMemUsedKb: 0,
  processes: [],
  rules: [],
  prompts: [],
  warnings: [],
  settings: { start_in_tray: false, minimize_to_tray: false },
});

let started = false;

function applySystemStats(d: {
  system_cpu?: number;
  system_mem_total_kb?: number;
  system_mem_used_kb?: number;
}) {
  if (d.system_cpu !== undefined) state.systemCpu = d.system_cpu;
  if (d.system_mem_total_kb !== undefined && d.system_mem_total_kb > 0) {
    state.systemMemTotalKb = d.system_mem_total_kb;
  }
  if (d.system_mem_used_kb !== undefined) state.systemMemUsedKb = d.system_mem_used_kb;
}

/**
 * Subscribe to daemon events.  Idempotent; call once from the main window.
 */
export function start(): void {
  if (started) return;
  started = true;

  void getAppInfo().then((info) => {
    state.appInfo = info;
    // Window chrome title comes from the Rust constant anyway; giving the
    // HTML document title the same value keeps dev-browser tabs consistent.
    if (info.display) document.title = info.display;
  });

  void getGuiSettings().then((s) => {
    state.settings = s;
  });

  void listen(EVT_HELLO, (e) => {
    const h = e.payload as { connected?: boolean; poll_interval_ms?: number };
    state.connected = !!h.connected;
    if (h.poll_interval_ms) state.pollIntervalMs = h.poll_interval_ms;
  });

  void listen(EVT_SNAPSHOT, (e) => {
    const s = e.payload as Snapshot;
    state.processes = s.processes;
    state.rules = s.rules;
    state.prompts = s.prompts;
    if (s.poll_interval_ms) state.pollIntervalMs = s.poll_interval_ms;
    applySystemStats(s);
  });

  void listen(EVT_DIFF, (e) => {
    const d = e.payload as Diff;
    const map = new Map<number, ProcessInfo>(
      state.processes.map((p) => [p.pid, p]),
    );
    for (const pid of d.removed) map.delete(pid);
    for (const p of d.added) map.set(p.pid, p);
    for (const p of d.updated) map.set(p.pid, p);
    state.processes = [...map.values()].sort((a, b) => a.pid - b.pid);
    applySystemStats(d);
  });

  void listen(EVT_PROMPT, (e) => {
    const p = e.payload as GamePrompt;
    state.prompts = [...state.prompts.filter((x) => x.name !== p.name), p];
    showPrompt(p);
  });

  void listen(EVT_WARN, (e) => {
    const msg = (e.payload as { msg?: string })?.msg;
    if (!msg) return;
    // Same-message dedupe (a rule that fails to apply warns once per pid).
    state.warnings = [
      ...state.warnings.filter((w) => w !== msg).slice(-(MAX_WARNINGS - 1)),
      msg,
    ];
  });

  // In case this window opened after the initial snapshot was broadcast,
  // the Rust backend keeps the latest one and serves it via get_state.
  void getState().then((s) => {
    if (s.processes.length > 0 && state.processes.length === 0) {
      state.processes = s.processes;
      state.rules = s.rules;
      state.prompts = s.prompts;
      if (s.poll_interval_ms) state.pollIntervalMs = s.poll_interval_ms;
      applySystemStats(s);
    }
  });
}

/** Open (or refocus) the small confirmation window (OpenSnitch-style). */
export async function showPrompt(p: GamePrompt): Promise<void> {
  const label = "nw-prompt";
  const existing = await WebviewWindow.getByLabel(label);
  if (existing) {
    void existing.setFocus();
    return;
  }
  const titleBase = state.appInfo?.display ?? "Priority prompt";
  const win = new WebviewWindow(label, {
    url: `index.html?prompt=1&name=${encodeURIComponent(p.name)}&pid=${p.pid}`,
    title: `${titleBase} — game detected?`,
    width: 480,
    height: 400,
    resizable: false,
    center: true,
    alwaysOnTop: true,
    decorations: false,
  });
  win.once("tauri://error", (e) => {
    console.error("prompt window failed", e);
  });
}