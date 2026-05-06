// Global keyboard router. Binds the standard verbs:
//  - ⌘K / Ctrl-K  open command palette
//  - g d / g s / g l  route jump to dashboard / settings / logs
//  - 1-9  panel focus jump (sets store.focusedPanel)
//  - r    refresh focused panel (no-op if no rerender registered)
//  - Esc  close drawers (handled per-component)
//  - p    pause / resume polling
//  - ?    cheatsheet sheet
//
// Each binding is also pushed onto the command palette registry so the
// palette and the keyboard share one truth.

import { sheet } from "../components.js";

const TYPING_TAGS = new Set(["INPUT", "TEXTAREA", "SELECT"]);

function isTyping(ev) {
  const t = ev.target;
  if (!t) return false;
  if (t.isContentEditable) return true;
  return TYPING_TAGS.has(t.tagName);
}

export function mountKeyboard({ store, router, palette }) {
  let pendingG = false;
  let pendingTimer = null;

  const verbs = [
    { id: "open.palette", label: "open command palette", verb: "palette", hotkey: "⌘K", action: () => palette.open() },
    { id: "go.dashboard", label: "go to dashboard", verb: "go d", hotkey: "g d", action: () => router.navigate("/") },
    { id: "go.settings", label: "go to settings", verb: "go s", hotkey: "g s", action: () => router.navigate("/settings") },
    { id: "go.logs", label: "go to logs", verb: "go l", hotkey: "g l", action: () => router.navigate("/logs") },
    { id: "toggle.pause", label: "pause / resume polling", verb: "pause", hotkey: "p", action: () => store.set({ paused: !store.get().paused }) },
    { id: "show.cheatsheet", label: "show keyboard cheatsheet", verb: "?", hotkey: "?", action: showCheatsheet },
  ];
  for (const v of verbs) palette.registry.register(v.id, v);

  const onKey = (ev) => {
    if (isTyping(ev)) return;

    // ⌘K / Ctrl-K
    if ((ev.metaKey || ev.ctrlKey) && ev.key.toLowerCase() === "k") {
      ev.preventDefault();
      palette.open();
      return;
    }

    // Pending g-prefix?
    if (pendingG) {
      pendingG = false;
      if (pendingTimer) { clearTimeout(pendingTimer); pendingTimer = null; }
      if (ev.key === "d") { ev.preventDefault(); router.navigate("/"); return; }
      if (ev.key === "s") { ev.preventDefault(); router.navigate("/settings"); return; }
      if (ev.key === "l") { ev.preventDefault(); router.navigate("/logs"); return; }
      return;
    }

    if (ev.key === "g") {
      pendingG = true;
      pendingTimer = setTimeout(() => { pendingG = false; }, 800);
      return;
    }

    if (ev.key === "?") {
      ev.preventDefault();
      showCheatsheet();
      return;
    }

    if (ev.key === "p") {
      ev.preventDefault();
      store.set({ paused: !store.get().paused });
      return;
    }

    if (/^[1-9]$/.test(ev.key)) {
      const idx = Number(ev.key);
      store.set({ focusedPanel: idx });
      return;
    }
  };

  document.addEventListener("keydown", onKey);
  return { dispose: () => document.removeEventListener("keydown", onKey) };
}

function showCheatsheet() {
  const rows = [
    ["⌘K / Ctrl-K", "open command palette"],
    ["g d", "dashboard"],
    ["g s", "settings"],
    ["g l", "logs"],
    ["1-9", "focus panel"],
    ["p", "pause polling"],
    ["?", "this help"],
    ["Esc", "close drawer"],
  ];
  const list = document.createElement("dl");
  list.className = "cheatsheet";
  for (const [k, v] of rows) {
    const dt = document.createElement("dt");
    dt.className = "mono";
    dt.textContent = k;
    const dd = document.createElement("dd");
    dd.textContent = v;
    list.appendChild(dt);
    list.appendChild(dd);
  }
  sheet({ title: "shortcuts", body: list });
}
