export type Theme = "light" | "dark" | "system";
export type Accent = "indigo" | "teal" | "violet" | "amber" | "slate";

const THEME_KEY = "hs-admin:theme";
const ACCENT_KEY = "hs-admin:accent";

export const DEFAULT_ACCENT: Accent = "indigo";

/**
 * Persists and applies the operator's theme and accent choice. Mirrors the
 * blocking inline script in `index.html`, which applies the same storage
 * keys before first paint to avoid a flash of the wrong theme.
 */
export function applyTheme(theme: Theme): void {
  const root = document.documentElement;
  if (theme === "system") {
    root.removeAttribute("data-theme");
    safeStorage()?.removeItem(THEME_KEY);
  } else {
    root.setAttribute("data-theme", theme);
    safeStorage()?.setItem(THEME_KEY, theme);
  }
}

export function readTheme(): Theme {
  const stored = safeStorage()?.getItem(THEME_KEY);
  return stored === "light" || stored === "dark" ? stored : "system";
}

export function applyAccent(accent: Accent): void {
  document.documentElement.setAttribute("data-accent", accent);
  safeStorage()?.setItem(ACCENT_KEY, accent);
}

export function readAccent(): Accent {
  const stored = safeStorage()?.getItem(ACCENT_KEY);
  const valid: readonly Accent[] = ["indigo", "teal", "violet", "amber", "slate"];
  return (valid as readonly string[]).includes(stored ?? "") ? (stored as Accent) : DEFAULT_ACCENT;
}

function safeStorage(): Storage | null {
  try {
    return window.localStorage;
  } catch {
    return null;
  }
}
