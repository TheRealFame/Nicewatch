// Applies the OS-native palette (KDE/GTK/bare) as CSS custom properties so
// the GUI matches the user's desktop instead of a hardcoded look.  The Rust
// backend extracts the colors (see src-tauri/src/theme.rs — a port of the
// @nearcade/native-palette + @nearcade/accent-color detection ladders);
// everything here just converts hex values into the CSS variables the
// stylesheets already consume, so the fallback palette in style.css remains
// fully functional when the extraction fails (e.g. running in a plain
// browser tab during development).
import { getNativeTheme, type NativeTheme } from "./api";

function parseHex(hex: string): [number, number, number] | null {
  const h = hex.trim().replace("#", "");
  if (h.length !== 6 || !/^[0-9a-fA-F]{6}$/.test(h)) return null;
  return [
    parseInt(h.slice(0, 2), 16),
    parseInt(h.slice(2, 4), 16),
    parseInt(h.slice(4, 6), 16),
  ];
}

function rgba(hex: string, alpha: number): string {
  const rgb = parseHex(hex);
  if (!rgb) return "";
  return `rgba(${rgb[0]}, ${rgb[1]}, ${rgb[2]}, ${alpha})`;
}

/** Linear mix toward a second hex; t=0 keeps hex1, t=1 becomes hex2. */
function mix(hex1: string, hex2: string, t: number): string {
  const a = parseHex(hex1);
  const b = parseHex(hex2);
  if (!a || !b) return hex1;
  const ch = (i: number) =>
    Math.round(a[i] + (b[i] - a[i]) * t)
      .toString(16)
      .padStart(2, "0");
  return `#${ch(0)}${ch(1)}${ch(2)}`;
}

export async function applyNativeTheme(): Promise<void> {
  let t: NativeTheme;
  try {
    t = await getNativeTheme();
  } catch (e) {
    console.warn("native theme unavailable, using fallback palette", e);
    return;
  }
  const s = document.documentElement.style;
  s.setProperty("--bg", t.bg);
  s.setProperty("--bg-alt", t.sidebar);
  s.setProperty("--card", t.surface);
  // Hover/raised surfaces: a small nudge of the surface toward the text
  // color (lighter on dark themes, darker on light ones).  There's no
  // reliable hover color in the KDE scheme (alternates can be the accent),
  // so we always derive it instead of trusting one.
  s.setProperty("--bg-raised", mix(t.surface, t.text, t.dark ? 0.06 : 0.05));
  s.setProperty("--text", t.text);
  s.setProperty("--text-dim", t.muted);
  // Faint text (placeholders, hints): dim the muted tone toward the canvas.
  s.setProperty("--text-faint", mix(t.muted, t.bg, 0.45));
  s.setProperty("--accent", t.accent);
  s.setProperty("--accent-soft", rgba(t.accent, 0.16));
  s.setProperty("--accent-border", rgba(t.accent, 0.45));
  // Hover on primary buttons: nudge the accent toward white (dark) or black
  // (light) instead of a hand-picked shade that only fits one theme.
  s.setProperty(
    "--accent-hover",
    mix(t.accent, t.dark ? "#ffffff" : "#000000", t.dark ? 0.14 : 0.08),
  );
  // Strong borders: nudge the OS border color toward the text color so it
  // separates correctly on both schemes (darker on light, lighter on dark).
  s.setProperty("--border-strong", mix(t.border, t.text, 0.18));
}
