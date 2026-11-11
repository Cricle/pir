import { state } from "./state.js";
import { applyThemePalette } from "./ui.js";

const STORAGE_KEY = "pi-phone-theme";

export const THEME_OPTIONS = [
  { value: "system", label: "System", hint: "Match device" },
  { value: "light", label: "Light", hint: "Day" },
  { value: "dark", label: "Dark", hint: "Night" },
];

export function getThemePreference() {
  try {
    const saved = localStorage.getItem(STORAGE_KEY);
    return THEME_OPTIONS.some((option) => option.value === saved) ? saved : "system";
  } catch {
    return "system";
  }
}

function systemTheme() {
  return typeof window.matchMedia === "function" &&
    window.matchMedia("(prefers-color-scheme: dark)").matches
    ? "dark"
    : "light";
}

export function resolveTheme(preference) {
  return preference === "system" ? systemTheme() : preference;
}

export function applyTheme(preference, { persist = true } = {}) {
  const next = THEME_OPTIONS.some((option) => option.value === preference)
    ? preference
    : "system";
  state.themePreference = next;
  if (persist) {
    try {
      localStorage.setItem(STORAGE_KEY, next);
    } catch {
      // Storage can be unavailable in private mode; the in-memory choice still works.
    }
  }
  const actual = resolveTheme(next);
  document.documentElement.dataset.theme = actual;
  document.documentElement.style.colorScheme = actual;
  if (actual === "light") applyThemePalette(null);
  const meta = document.querySelector('meta[name="theme-color"]');
  if (meta) meta.setAttribute("content", actual === "light" ? "#fafaf8" : "#0a0a0a");
}

export function initializeTheme() {
  const preference = getThemePreference();
  applyTheme(preference, { persist: false });
  if (typeof window.matchMedia !== "function") return;
  const media = window.matchMedia("(prefers-color-scheme: dark)");
  const onChange = () => {
    if (state.themePreference === "system") {
      applyTheme("system", { persist: false });
    }
  };
  if (media.addEventListener) media.addEventListener("change", onChange);
  else media.addListener?.(onChange);
}
