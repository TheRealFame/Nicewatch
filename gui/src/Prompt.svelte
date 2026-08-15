<script lang="ts">
  // Small confirmation window shown when the daemon's game-detection
  // heuristic flags a never-before-seen process (OpenSnitch-style).
  import { getCurrentWindow } from "@tauri-apps/api/window";
  import { confirmGame, type GameAnswer } from "./lib/api";
  import { state as ui, start } from "./lib/store.svelte.ts";
  import { applyNativeTheme } from "./lib/theme";

  const params = new URLSearchParams(window.location.search);
  const name = params.get("name") ?? "";
  const pid = params.get("pid") ?? "";

  const win = getCurrentWindow();

  let busy = $state(false);

  $effect(() => {
    void applyNativeTheme();
    start();
    if (ui.appInfo?.display) document.title = `${ui.appInfo.display} — game detected?`;
  });

  async function answer(a: GameAnswer) {
    if (busy) return;
    busy = true;
    try {
      await confirmGame(name, a);
    } catch (e) {
      console.error("confirm_game failed", e);
    } finally {
      await win.close();
    }
  }
</script>

<div class="shell">
  <header class="tbar" data-tauri-drag-region>
    <span class="dot"></span>
    <span class="tname" data-tauri-drag-region>{ui.appInfo?.display ?? "…"}</span>
    <span class="tspacer" data-tauri-drag-region></span>
    <button class="icon-btn" title="Close" onclick={() => win.close()}>✕</button>
  </header>

  <main class="prompt">
    <div class="icon-circle">?</div>
    <h1>Game Detected</h1>
    <p class="proc">
      <span class="pname">{name}</span>
      <span class="pmeta">pid {pid}</span>
    </p>
    <p class="sub">This process looks like a game (Steam env / DRM fd heuristic).
      Apply the <b>Game</b> priority tier?</p>

    <div class="actions">
      <button type="button" class="primary" disabled={busy} onclick={() => answer("yes")}>
        Yes — Game
      </button>
      <button type="button" disabled={busy} onclick={() => answer("no")}>
        No — Software
      </button>
      <button type="button" class="flat" disabled={busy} onclick={() => answer("not-now")}>
        Not now
      </button>
    </div>
    <p class="hint">
      "Yes"/"No" persist a rule for every future launch; "Not now" only affects
      this running instance and we'll ask again next time it appears.
    </p>
  </main>
</div>

<style>
  .shell {
    display: flex;
    flex-direction: column;
    height: 100%;
    background: var(--bg);
  }
  .tbar {
    display: flex;
    align-items: center;
    gap: 8px;
    height: 34px;
    padding: 0 4px 0 12px;
    background: var(--bg-alt);
    border-bottom: 1px solid var(--border);
    flex-shrink: 0;
  }
  .dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--amber);
  }
  .tname {
    font-size: 12.5px;
    font-weight: 600;
    color: var(--text-dim);
  }
  .tspacer {
    flex: 1;
    align-self: stretch;
  }
  .icon-btn {
    background: none;
    border: none;
    border-radius: 6px;
    color: var(--text-dim);
    width: 30px;
    height: 26px;
    padding: 0;
    font-size: 14px;
    cursor: pointer;
  }
  .icon-btn:hover {
    background: var(--warn);
    color: #fff;
  }
  .prompt {
    flex: 1;
    display: flex;
    flex-direction: column;
    align-items: center;
    text-align: center;
    padding: 20px 24px 16px;
    min-width: 0;
  }
  .icon-circle {
    width: 42px;
    height: 42px;
    border-radius: 50%;
    background: var(--amber);
    color: #1e1e22;
    font-size: 22px;
    font-weight: 700;
    display: flex;
    align-items: center;
    justify-content: center;
    margin-bottom: 12px;
  }
  h1 {
    font-size: 16px;
    margin: 0 0 8px;
    font-weight: 650;
  }
  .proc {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    justify-content: center;
    gap: 8px;
    margin: 0 0 10px;
    max-width: 100%;
    min-width: 0;
  }
  .pname {
    font-family: ui-monospace, monospace;
    font-size: 14px;
    font-weight: 700;
    color: var(--amber);
    background: rgba(217, 160, 60, 0.14);
    border: 1px solid rgba(217, 160, 60, 0.45);
    padding: 3px 10px;
    border-radius: 999px;
    max-width: 100%;
    overflow-wrap: anywhere;
    word-break: break-word;
    white-space: normal;
  }
  .pmeta {
    font-size: 12px;
    color: var(--text-faint);
    font-variant-numeric: tabular-nums;
  }
  .sub {
    font-size: 13px;
    color: var(--text-dim);
    margin: 0 0 16px;
    line-height: 1.45;
    max-width: 380px;
    width: 100%;
    overflow-wrap: anywhere;
    word-break: break-word;
  }
  .sub b {
    color: var(--accent);
  }
  .actions {
    display: flex;
    gap: 8px;
    align-self: stretch;
  }
  .actions button {
    flex: 1;
    font-size: 13px;
  }
  .hint {
    margin: 14px 0 0;
    font-size: 11px;
    color: var(--text-faint);
    line-height: 1.4;
    max-width: 380px;
    width: 100%;
    overflow-wrap: anywhere;
    word-break: break-word;
  }
</style>