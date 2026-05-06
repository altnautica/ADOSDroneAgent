// Command palette. Fuzzy-matches over a registry shared with the keyboard
// router. Mobile: full-height sheet. Desktop: centered modal. The palette
// itself is mounted lazily; mountCommandPalette returns the registry +
// open() helper.

import { el, cn } from "../components.js";

function fuzzyScore(needle, haystack) {
  if (!needle) return 1;
  const n = needle.toLowerCase();
  const h = haystack.toLowerCase();
  let i = 0;
  let score = 0;
  let lastIdx = -1;
  for (const ch of n) {
    const idx = h.indexOf(ch, lastIdx + 1);
    if (idx === -1) return 0;
    score += 1 + (idx === lastIdx + 1 ? 1 : 0);
    lastIdx = idx;
    i += 1;
  }
  return score / Math.max(haystack.length, n.length);
}

class Registry {
  constructor() {
    this.entries = new Map();
  }

  register(id, entry) {
    this.entries.set(id, { id, ...entry });
  }

  unregister(id) {
    this.entries.delete(id);
  }

  list() {
    return Array.from(this.entries.values());
  }
}

export function mountCommandPalette(rootEl) {
  const registry = new Registry();

  let host = null;
  let input = null;
  let listEl = null;
  let active = 0;
  let filtered = [];

  const close = () => {
    if (!host) return;
    document.removeEventListener("keydown", onKey, true);
    if (host.parentNode) host.parentNode.removeChild(host);
    host = null;
  };

  const render = () => {
    listEl.replaceChildren();
    filtered.forEach((entry, i) => {
      const li = el("li", {
        className: cn("palette-item", i === active ? "is-active" : null),
        role: "option",
        onclick: () => fire(i),
        onmouseenter: () => { active = i; render(); },
      },
        el("span", { className: "palette-label", text: entry.label || entry.id }),
        entry.verb ? el("span", { className: "palette-verb", text: entry.verb }) : null,
        entry.hotkey ? el("kbd", { className: "palette-hotkey", text: entry.hotkey }) : null,
      );
      listEl.appendChild(li);
    });
  };

  const refilter = (q) => {
    const items = registry.list();
    const scored = items
      .map((entry) => {
        const hay = `${entry.label || ""} ${entry.verb || ""} ${entry.id}`;
        return { entry, score: fuzzyScore(q, hay) };
      })
      .filter((r) => r.score > 0)
      .sort((a, b) => b.score - a.score)
      .map((r) => r.entry);
    filtered = scored.length ? scored : items;
    active = 0;
    render();
  };

  const fire = (i) => {
    const entry = filtered[i];
    if (!entry) return;
    close();
    try { entry.action && entry.action(); }
    catch (err) { console.warn("command failed", err); }
  };

  const trapTab = (ev) => {
    if (!host || ev.key !== "Tab") return;
    const focusables = host.querySelectorAll(
      'input, button, [tabindex]:not([tabindex="-1"])'
    );
    if (!focusables.length) {
      ev.preventDefault();
      return;
    }
    const first = focusables[0];
    const last = focusables[focusables.length - 1];
    const activeEl = document.activeElement;
    if (ev.shiftKey && activeEl === first) {
      ev.preventDefault();
      last.focus();
    } else if (!ev.shiftKey && activeEl === last) {
      ev.preventDefault();
      first.focus();
    }
  };

  const onKey = (ev) => {
    if (ev.key === "Escape") { ev.preventDefault(); close(); return; }
    if (ev.key === "ArrowDown") { ev.preventDefault(); active = Math.min(filtered.length - 1, active + 1); render(); return; }
    if (ev.key === "ArrowUp") { ev.preventDefault(); active = Math.max(0, active - 1); render(); return; }
    if (ev.key === "Enter") { ev.preventDefault(); fire(active); return; }
    if (ev.key === "Tab") { trapTab(ev); return; }
  };

  const open = () => {
    if (host) return;
    input = el("input", {
      type: "text",
      className: "palette-input mono",
      placeholder: "type to search",
      "aria-label": "search commands",
      autocomplete: "off",
      spellcheck: "false",
      oninput: (ev) => refilter(ev.target.value),
    });
    listEl = el("ul", { className: "palette-list", role: "listbox" });

    const inner = el("div", { className: "palette-inner" },
      el("div", { className: "palette-head" }, input),
      listEl,
    );

    host = el("div", {
      className: "palette-host",
      role: "dialog",
      "aria-modal": "true",
      "aria-label": "command palette",
      onclick: (ev) => { if (ev.target === host) close(); },
    }, inner);
    rootEl.appendChild(host);
    document.addEventListener("keydown", onKey, true);
    refilter("");
    queueMicrotask(() => input && input.focus());
  };

  return { registry, open, close };
}
