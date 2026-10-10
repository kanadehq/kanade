// Colour theme picker: system -> light -> dark -> system.
//
// The choice is stored in localStorage and mirrored as `data-theme` on
// <html> (absent = follow the OS). index.html restores it with an inline
// script before first paint; the key and values below must match it.
// style.css does the actual theming via `color-scheme` / `light-dark()`.

const KEY = "kanade-client.theme";

type Theme = "system" | "light" | "dark";

const NEXT: Record<Theme, Theme> = {
  system: "light",
  light: "dark",
  dark: "system",
};
const LABEL: Record<Theme, string> = {
  system: "システム",
  light: "ライト",
  dark: "ダーク",
};
const ICON: Record<Theme, string> = {
  system: "monitor",
  light: "sun",
  dark: "moon",
};

function load(): Theme {
  try {
    const v = localStorage.getItem(KEY);
    if (v === "light" || v === "dark") return v;
  } catch {
    // storage unavailable: behave as system
  }
  return "system";
}

function apply(t: Theme): void {
  const root = document.documentElement;
  if (t === "system") root.removeAttribute("data-theme");
  else root.dataset.theme = t;
}

function save(t: Theme): void {
  try {
    if (t === "system") localStorage.removeItem(KEY);
    else localStorage.setItem(KEY, t);
  } catch {
    // not persisted; still applies for this session
  }
}

export function initThemeToggle(hydrateIcons: () => void): void {
  const btn = document.getElementById("theme-toggle");
  if (!btn) return;
  let current = load();
  apply(current);

  const render = (): void => {
    const text = `テーマ: ${LABEL[current]}（クリックで${LABEL[NEXT[current]]}に切替）`;
    btn.title = text;
    btn.setAttribute("aria-label", text);
    // Swap only the icon so the button keeps keyboard focus.
    btn.replaceChildren();
    const i = document.createElement("i");
    i.setAttribute("data-lucide", ICON[current]);
    btn.appendChild(i);
    hydrateIcons();
  };

  btn.addEventListener("click", () => {
    current = NEXT[current];
    apply(current);
    save(current);
    render();
  });
  render();
}
