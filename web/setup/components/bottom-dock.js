// Bottom navigation dock. Mobile only. Hidden on tablet+ via CSS container
// queries. Four slots: home, logs, command palette, settings.

import { el } from "../components.js";

export function mountBottomDock(rootEl, { router, openCommandPalette }) {
  const node = el("nav", { className: "bottom-dock", "aria-label": "primary" });
  rootEl.appendChild(node);

  const slot = (label, glyph, action) =>
    el("button", {
      type: "button",
      className: "dock-slot",
      "aria-label": label,
      onclick: action,
    },
      el("span", { className: "dock-glyph", "aria-hidden": "true", text: glyph }),
      el("span", { className: "dock-label", text: label }),
    );

  node.replaceChildren(
    slot("home", "⌂", () => router.navigate("/")),
    slot("logs", "≡", () => router.navigate("/logs")),
    slot("cmd", "⌘", () => openCommandPalette && openCommandPalette()),
    slot("settings", "⚙", () => router.navigate("/settings")),
  );

  return { node };
}
