<script lang="ts">
  // Generic custom dropdown (native <select> renders with the GTK theme on
  // WebKitGTK, which shows white-on-white in dark mode — so all selects are
  // custom, rounded, and follow our CSS variables).
  export interface Option {
    value: string;
    label: string;
  }

  let { options, value, onPick }: {
    options: Option[];
    value: string;
    onPick: (value: string) => void;
  } = $props();

  let open = $state(false);

  function current(): Option {
    return options.find((o) => o.value === value) ?? options[0];
  }

  function toggle() {
    open = !open;
  }

  function pick(o: Option) {
    open = false;
    if (o.value !== value) onPick(o.value);
  }
</script>

<div class="od">
  <button
    type="button"
    class="od-btn"
    class:open
    onclick={toggle}
    aria-haspopup="listbox"
    aria-expanded={open}
  >
    <span class="od-label">{current()?.label ?? ""}</span>
    <svg class="od-chev" width="10" height="10" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round"><path d="m6 9 6 6 6-6" /></svg>
  </button>

  {#if open}
    <div class="od-menu" role="listbox">
      {#each options as o (o.value)}
        <button
          type="button"
          class="od-item"
          class:sel={o.value === value}
          role="option"
          aria-selected={o.value === value}
          onclick={() => pick(o)}
        >
          {o.label}
        </button>
      {/each}
    </div>
  {/if}
</div>

<style>
  .od {
    position: relative;
    display: inline-block;
  }
  .od-btn {
    display: inline-flex;
    align-items: center;
    gap: 10px;
    min-width: 96px;
    padding: 6px 12px;
    background: var(--bg-raised);
    color: var(--text);
    border: 1px solid var(--border);
    border-radius: 999px;
    font-size: 13px;
    font-weight: 600;
    cursor: pointer;
    justify-content: space-between;
  }
  .od-btn:hover {
    border-color: var(--border-strong);
    background: var(--bg-alt);
  }
  .od-btn.open {
    border-color: var(--accent);
  }
  .od-chev {
    color: var(--text-dim);
    transition: transform 0.12s ease;
  }
  .od-btn.open .od-chev {
    transform: rotate(180deg);
  }
  .od-menu {
    position: absolute;
    top: calc(100% + 6px);
    left: 0;
    min-width: 100%;
    max-height: 260px;
    overflow-y: auto;
    background: var(--bg-raised);
    border: 1px solid var(--border-strong);
    border-radius: var(--radius-lg);
    box-shadow: var(--shadow);
    padding: 4px;
    z-index: 50;
  }
  .od-item {
    display: block;
    width: 100%;
    text-align: left;
    background: none;
    border: none;
    color: var(--text);
    padding: 7px 10px;
    border-radius: var(--radius);
    font-size: 13px;
    cursor: pointer;
    white-space: nowrap;
  }
  .od-item:hover {
    background: var(--accent-soft);
    color: var(--accent);
  }
  .od-item.sel {
    background: var(--accent-soft);
    color: var(--accent);
    font-weight: 700;
  }
</style>