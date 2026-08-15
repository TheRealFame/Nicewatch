<script lang="ts">
  import type { Tier } from "./api";
  import { TIER_LABELS, TIER_ORDER } from "./api";

  // Custom dropdown: native <select> popups cannot be rounded on Linux, so
  // this renders a button + floating rounded menu instead.
  let { value, label, onPick, disabled = false } = $props<{
    value: Tier | null;
    label?: string;
    onPick: (tier: Tier) => void;
    disabled?: boolean;
  }>();

  let open = $state(false);
  let btn = $state<HTMLButtonElement | null>(null);
  let menu = $state<HTMLDivElement | null>(null);

  $effect(() => {
    if (!open) return;
    const onDown = (e: PointerEvent) => {
      const t = e.target as Node;
      if (btn && !btn.contains(t) && menu && !menu.contains(t)) {
        open = false;
      }
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") open = false;
    };
    document.addEventListener("pointerdown", onDown);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("pointerdown", onDown);
      document.removeEventListener("keydown", onKey);
    };
  });

  function pick(t: Tier) {
    open = false;
    onPick(t);
  }

  const cls = $derived(
    value ? `tier-drop tier-${value}` : "tier-drop tier-none",
  );
</script>

<div class="td-wrap">
  <button
    bind:this={btn}
    class={cls}
    class:open={open}
    type="button"
    disabled={disabled}
    onclick={() => (open = !open)}
    aria-haspopup="listbox"
    aria-expanded={open}
  >
    <span class="td-label">{label ?? (value ? TIER_LABELS[value] : "Default")}</span>
    <svg class="td-chev" width="10" height="10" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round">
      <path d="m6 9 6 6 6-6" />
    </svg>
  </button>
  {#if open}
    <div class="td-menu" bind:this={menu} role="listbox">
      {#each TIER_ORDER as t (t)}
        <button
          class="td-item"
          class:sel={value === t}
          type="button"
          role="option"
          aria-selected={value === t}
          onclick={() => pick(t)}
        >
          <span class="td-dot tier-{t}"></span>
          <span class="td-item-label">{TIER_LABELS[t]}</span>
          {#if value === t}
            <svg class="td-check" width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="3" stroke-linecap="round" stroke-linejoin="round">
              <path d="M20 6 9 17l-5-5" />
            </svg>
          {/if}
        </button>
      {/each}
    </div>
  {/if}
</div>

<style>
  .td-wrap {
    position: relative;
    display: inline-flex;
  }
  .tier-drop {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    width: 100%;
    min-width: 0;
    font-size: 12px;
    font-weight: 600;
    padding: 4px 10px;
    border-radius: 999px;
    border: 1px solid var(--border);
    background-color: var(--bg-alt);
    color: var(--text-dim);
    cursor: pointer;
    white-space: nowrap;
  }
  .tier-drop:hover,
  .tier-drop.open {
    border-color: var(--accent);
    color: var(--text);
  }
  .tier-drop:disabled {
    opacity: 0.55;
    cursor: not-allowed;
  }
  .td-label {
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .td-chev {
    flex-shrink: 0;
    opacity: 0.7;
    transition: transform 0.15s ease;
  }
  .tier-drop.open .td-chev {
    transform: rotate(180deg);
  }
  .td-menu {
    position: absolute;
    top: calc(100% + 6px);
    left: 0;
    z-index: 40;
    min-width: 100%;
    background: var(--card);
    border: 1px solid var(--border-strong);
    border-radius: 14px;
    box-shadow: 0 12px 32px rgba(0, 0, 0, 0.35);
    padding: 5px;
    display: flex;
    flex-direction: column;
    gap: 1px;
  }
  .td-item {
    display: flex;
    align-items: center;
    gap: 9px;
    width: 100%;
    padding: 7px 10px;
    border: none;
    background: none;
    border-radius: 10px;
    color: var(--text);
    font-size: 12.5px;
    font-weight: 500;
    text-align: left;
    cursor: pointer;
  }
  .td-item:hover {
    background: var(--bg-raised);
  }
  .td-item.sel {
    color: var(--accent);
    font-weight: 700;
  }
  .td-dot {
    width: 9px;
    height: 9px;
    border-radius: 50%;
    flex-shrink: 0;
  }
  .td-item-label {
    flex: 1;
  }
  .td-check {
    color: var(--accent);
  }
</style>