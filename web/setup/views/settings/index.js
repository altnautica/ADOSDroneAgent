// Settings shell. Mobile / tablet: full-screen route. Desktop: right-docked
// 480px split-view alongside the dashboard. Section accordions all open by
// default. The Apply button is wired in a later iteration.

import { el, toast } from "../../components.js";
import { renderProfileSection } from "./profile.js";
import { renderCloudSection } from "./cloud.js";
import { renderNetworkSection } from "./network.js";
import { renderDisplaySection } from "./display.js";
import { renderAdvancedSection } from "./advanced.js";

const SECTIONS = [
  { id: "profile", label: "profile", render: renderProfileSection },
  { id: "cloud", label: "cloud", render: renderCloudSection },
  { id: "network", label: "network", render: renderNetworkSection },
  { id: "display", label: "display", render: renderDisplaySection },
  { id: "advanced", label: "advanced", render: renderAdvancedSection },
];

function accordion({ id, label, body, open }) {
  const head = el("button", {
    type: "button",
    className: "accordion-head",
    "aria-expanded": open ? "true" : "false",
    onclick: (ev) => {
      const wrap = ev.currentTarget.closest(".accordion");
      if (!wrap) return;
      wrap.classList.toggle("is-open");
      const expanded = wrap.classList.contains("is-open");
      ev.currentTarget.setAttribute("aria-expanded", expanded ? "true" : "false");
    },
  },
    el("span", { className: "accordion-label", text: label }),
    el("span", { className: "accordion-glyph", "aria-hidden": "true", text: "▾" }),
  );

  const bodyEl = el("div", { className: "accordion-body" });
  if (body instanceof Node) bodyEl.appendChild(body);

  return el("section", {
    className: `accordion ${open ? "is-open" : ""}`.trim(),
    "data-section": id,
  }, head, bodyEl);
}

export function renderSettings(targetEl, { store, params }) {
  const focusSection = params && params.section ? params.section : null;

  const dirtyCount = 0; // wiring lands in a later iteration

  const sections = SECTIONS.map((s) => accordion({
    id: s.id,
    label: s.label,
    body: s.render({ store }),
    open: focusSection ? focusSection === s.id : true,
  }));

  const apply = el("button", {
    type: "button",
    className: "settings-apply",
    disabled: true,
    "aria-disabled": "true",
    title: "settings apply lands in a later iteration",
    text: `apply (${dirtyCount} changes)`,
    onclick: () => {
      toast({
        message: "settings apply lands in a later iteration",
        severity: "info",
      });
    },
  });

  const root = el("section", { className: "view view-settings", "data-view": "settings" },
    el("header", { className: "view-head" },
      el("h1", { className: "view-title", text: "settings" }),
      el("p", { className: "view-sub text-faint", text: "section accordions are open by default; apply lands in a later iteration." }),
    ),
    el("div", { className: "settings-stack" }, ...sections),
    el("footer", { className: "settings-foot" }, apply),
  );

  targetEl.replaceChildren(root);

  if (focusSection) {
    const target = root.querySelector(`[data-section="${focusSection}"]`);
    if (target && target.scrollIntoView) {
      target.scrollIntoView({ block: "start" });
    }
  }

  return { dispose: () => {} };
}
