// Settings shell. Mobile / tablet: full-screen route. Desktop: right-
// docked 480px split-view alongside the dashboard. Section accordions
// render real form controls bound to a per-section dirty tracker; the
// Apply button collects every dirty payload and posts ONCE to
// /api/v1/setup/apply, then surfaces per-section results as toasts.

import { el, toast, sheet } from "../../components.js";
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

    const profileResult = sections.profile;
    const restartAttempted = !!(
      profileResult?.data?.auto_restart_attempted &&
      profileResult.data.auto_restart_ok
    );
    const requestedProfile = profileResult?.data?.profile;

    if (res?.overall && restartAttempted && requestedProfile) {
      for (const m of mounted) m.reset?.();
      updateApplyLabel();
      reconnectThenGoHome(requestedProfile);
      return;
    }

    if (res?.overall) {
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

  const reconnectThenGoHome = (requestedProfile) => {
    // Profile flipped and the agent is restarting its supervisor. Show a
    // sheet that polls /api/v1/setup/status until the new profile is
    // reported, then navigates back to the dashboard. Cap the wait so a
    // permanently broken restart does not freeze the UI.
    let attempts = 0;
    const maxAttempts = 30;
    const delayMs = 2000;

    const status = el("p", {
      className: "sheet-body-text mono",
      text: `waiting for agent... 0 / ${maxAttempts}`,
    });
    const sheetCtl = sheet({
      title: `restarting agent (${requestedProfile})`,
      body: el("div", {},
        el("p", { text: "the supervisor is restarting to apply the new profile." }),
        status,
      ),
      footer: el("button", {
        type: "button",
        className: "btn",
        text: "go to dashboard now",
        onclick: () => {
          sheetCtl.close();
          go("/");
        },
      }),
      dismissable: false,
    });

    const go = (path) => {
      try {
        history.pushState({}, "", path);
        window.dispatchEvent(new PopStateEvent("popstate"));
      } catch {
        // best-effort; user can navigate manually
      }
    };

    const poll = async () => {
      attempts += 1;
      status.textContent = `waiting for agent... ${attempts} / ${maxAttempts}`;
      try {
        const data = await apiFetch("/api/v1/setup/status");
        if (data && data.profile === requestedProfile) {
          status.textContent = "agent online with new profile.";
          setTimeout(() => {
            sheetCtl.close();
            go("/");
          }, 600);
          return;
        }
      } catch {
        // restart in flight, keep polling
      }
      if (attempts >= maxAttempts) {
        toast({
          message: "agent did not come back in time. check service logs.",
          severity: "err",
        });
        sheetCtl.close();
        return;
      }
      setTimeout(poll, delayMs);
    };

    setTimeout(poll, delayMs);
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
