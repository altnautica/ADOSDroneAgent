// Settings shell. Mobile / tablet: full-screen route. Desktop: right-
// docked 480px split-view alongside the dashboard. Section accordions
// render real form controls bound to a per-section dirty tracker; the
// Apply button collects every dirty payload and posts ONCE to
// /api/v1/setup/apply, then surfaces per-section results as toasts.

import { el, toast } from "../../components.js";
import { apiFetch } from "../../state.js";
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

  // Mount each section. Each render returns
  // { node, tracker, payload, reset }. The shell aggregates dirty
  // counts across all five trackers.
  const mounted = SECTIONS.map((s) => {
    const ctx = { store, onChange: () => updateApplyLabel() };
    const handle = s.render(ctx);
    return { id: s.id, label: s.label, ...handle };
  });

  const totalDirty = () => mounted.reduce(
    (n, m) => n + (m.tracker?.dirtyCount?.() || 0),
    0,
  );

  const apply = el("button", {
    type: "button",
    className: "settings-apply",
    onclick: () => doApply(),
  });

  const cancel = el("button", {
    type: "button",
    className: "settings-cancel",
    text: "cancel",
    onclick: () => {
      for (const m of mounted) m.reset?.();
      updateApplyLabel();
    },
  });

  const updateApplyLabel = () => {
    const n = totalDirty();
    apply.textContent = `apply (${n} changes)`;
    if (n === 0) {
      apply.setAttribute("disabled", "true");
      apply.setAttribute("aria-disabled", "true");
    } else {
      apply.removeAttribute("disabled");
      apply.removeAttribute("aria-disabled");
    }
  };

  const collectPayload = () => {
    const out = {};
    for (const m of mounted) {
      const slice = m.payload?.();
      if (slice && Object.keys(slice).length) out[m.id] = slice;
    }
    return out;
  };

  const doApply = async () => {
    const body = collectPayload();
    if (!Object.keys(body).length) return;

    apply.setAttribute("disabled", "true");
    apply.setAttribute("aria-disabled", "true");
    const previousLabel = apply.textContent;
    apply.textContent = "applying…";

    try {
      const res = await apiFetch("/api/v1/setup/apply", {
        method: "POST",
        body: JSON.stringify(body),
      });
      handleResponse(res);
    } catch (err) {
      toast({
        message: `apply failed: ${err.message || err}`,
        severity: "err",
      });
      apply.textContent = previousLabel;
      updateApplyLabel();
    }
  };

  const handleResponse = (res) => {
    const sections = res?.sections || {};
    for (const [name, result] of Object.entries(sections)) {
      const ok = !!result.ok;
      toast({
        message: `${name}: ${result.message || (ok ? "ok" : "failed")}`,
        severity: ok ? "ok" : "err",
      });
    }
    const rolledBack = Array.isArray(res?.rolled_back) ? res.rolled_back : [];
    if (rolledBack.length) {
      toast({
        message: `rolled back: ${rolledBack.join(", ")}`,
        severity: "warn",
      });
    }
    if (res?.overall) {
      // Reset trackers so the dashboard picks up clean state next render.
      for (const m of mounted) m.reset?.();
      updateApplyLabel();
      try {
        history.pushState({}, "", "/");
        window.dispatchEvent(new PopStateEvent("popstate"));
      } catch {
        // navigation is best-effort; the toast already confirmed success.
      }
    } else {
      updateApplyLabel();
    }
  };

  const sections = mounted.map((m) => accordion({
    id: m.id,
    label: m.label,
    body: m.node,
    open: focusSection ? focusSection === m.id : true,
  }));

  const root = el("section", { className: "view view-settings", "data-view": "settings" },
    el("header", { className: "view-head" },
      el("h1", { className: "view-title", text: "settings" }),
      el("p", { className: "view-sub text-faint", text: "section accordions are open by default. apply posts every dirty field in one shot." }),
    ),
    el("div", { className: "settings-stack" }, ...sections),
    el("footer", { className: "settings-foot" }, cancel, apply),
  );

  targetEl.replaceChildren(root);
  updateApplyLabel();

  if (focusSection) {
    const target = root.querySelector(`[data-section="${focusSection}"]`);
    if (target && target.scrollIntoView) {
      target.scrollIntoView({ block: "start" });
    }
  }

  return { dispose: () => {} };
}
