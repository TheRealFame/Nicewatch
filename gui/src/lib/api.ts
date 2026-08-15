// Typed wrappers around the Tauri commands exposed by the Rust backend.
// The GUI never talks to the daemon socket directly — everything flows
// through these commands and the daemon events (see store.svelte.ts).
import { invoke } from "@tauri-apps/api/core";

export type Tier = "software" | "game" | "streaming" | "realtime";
export type GameAnswer = "yes" | "no" | "not-now";

export interface ProcessInfo {
  pid: number;
  ppid: number;
  name: string;
  user: string;
  cpu_percent: number;
  mem_kb: number;
  status: string;
  nice: number;
  ionice_class: "none" | "realtime" | "best-effort" | "idle";
  ionice_priority: number;
  tier: Tier | null;
  game_detected: boolean;
  /** Lives in a Nicewatch-managed cgroup (rule has cgroup limits binding). */
  capped: boolean;
  exe: string | null;
  start_secs: number;
}

export interface RuleInfo {
  name: string;
  match_name: string;
  tier: Tier | null;
  nice: number | null;
  /** Percent of one core this rule caps CPU at (null = no hard cap). */
  cpu_cap_percent: number | null;
}

export interface GamePrompt {
  name: string;
  pid: number;
}

export interface Snapshot {
  processes: ProcessInfo[];
  rules: RuleInfo[];
  prompts: GamePrompt[];
  poll_interval_ms: number;
  /** System-wide CPU usage as % of total capacity (system-monitor scale). */
  system_cpu: number;
  /** Total physical memory in KiB (/proc/meminfo MemTotal). */
  system_mem_total_kb: number;
  /** Used physical memory in KiB (MemTotal - MemAvailable). */
  system_mem_used_kb: number;
}

export interface Diff {
  added: ProcessInfo[];
  updated: ProcessInfo[];
  removed: number[];
  /** System stats travel with every frame, not just full snapshots. */
  system_cpu: number;
  system_mem_total_kb: number;
  system_mem_used_kb: number;
}

export interface AppInfo {
  app: string;
  display: string;
  version: string;
}

export interface NativeTheme {
  bg: string;
  sidebar: string;
  surface: string;
  surface_hover: string;
  text: string;
  muted: string;
  muted2: string;
  border: string;
  accent: string;
  dark: boolean;
}

export function getAppInfo(): Promise<AppInfo> {
  return invoke("app_info");
}

/** OS colors + accent, extracted on the Rust side (see src-tauri/theme.rs). */
export function getNativeTheme(): Promise<NativeTheme> {
  return invoke("get_native_theme");
}

export function getState(): Promise<Snapshot> {
  return invoke("get_state");
}

export function setTier(pid: number, tier: Tier): Promise<void> {
  return invoke("set_tier", { pid, tier });
}

export function setCap(name: string, pct: number | null): Promise<void> {
  return invoke("set_cap", { name, pct });
}

export function setPollInterval(pollIntervalMs: number): Promise<void> {
  return invoke("set_poll_interval", { pollIntervalMs });
}

export interface AppMeta {
  name: string;
  icon: string | null;
}

/** Resolve a process `exe` path to a display name + icon (cached). */
export function appMeta(exe: string): Promise<AppMeta> {
  return invoke("app_meta", { exe });
}

export interface Outcome {
  ok: boolean;
  detail: string;
}

/** One-click daemon on/off (systemctl --user start|stop nicewatch). */
export function setDaemonRunning(running: boolean): Promise<Outcome> {
  return invoke("set_daemon_running", { running });
}

/** One-click fix for the CANNOT WRITE ROOT CONFIG warning (pkexec). */
export function fixRootConfig(): Promise<Outcome> {
  return invoke("fix_root_config");
}

export interface GuiSettings {
  start_in_tray: boolean;
  minimize_to_tray: boolean;
}

export function getGuiSettings(): Promise<GuiSettings> {
  return invoke("get_gui_settings");
}

export function setGuiSettings(s: GuiSettings): Promise<void> {
  return invoke("set_gui_settings", { s });
}

export function confirmGame(name: string, answer: GameAnswer): Promise<void> {
  return invoke("confirm_game", { name, answer });
}

export interface InstallOutcome {
  ok: boolean;
  detail: string;
}

/** Install/start the daemon as a per-user service (systemctl user unit). */
export function installService(): Promise<InstallOutcome> {
  return invoke("install_service");
}

// ---------------------------------------------------------------------------
// Display helpers (single definition of the tier label set for the UI).
// ---------------------------------------------------------------------------

export const TIER_LABELS: Record<Tier, string> = {
  software: "Software",
  game: "Game",
  streaming: "Streaming",
  realtime: "Realtime",
};

export const TIER_ORDER: Tier[] = ["software", "game", "streaming", "realtime"];

export function tierLabel(tier: Tier | null): string {
  return tier ? TIER_LABELS[tier] : "Default";
}