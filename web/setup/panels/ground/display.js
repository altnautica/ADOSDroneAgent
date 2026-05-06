// Local display panel. Device path, pixel dims, refresh rate, currently
// rendered content, kiosk URL chip. Reads store.state.dashboard.display.
// Active on every ground role. The kiosk URL is clickable and opens in
// a new tab. Empty state when no display attached.

import { el, panel } from "../../components.js";
import { pick, fmtNum } from "../_util.js";

function row(label, value, severity) {
  const v = el("span", { className: "panel__row-value mono", text: value != null ? String(value) : "-" });
  if (severity) v.classList.add(`text--${severity}`);
  return el("div", { className: "panel__row" },
    el("span", { className: "panel__row-label", text: label }),
    v,
  );
}

export function renderDisplayPanel(store, opts = {}) {
  const body = el("div", { className: "panel-stack" });
  const node = panel({
    title: "local display",
    span: 6,
    expandable: true,
    severity: "idle",
    body,
  });

  const openKiosk = () => {
    const state = store.get();
    const url = pick(state, "dashboard.display.kiosk_url", null);
    if (!url) return;
    try {
      window.open(String(url), "_blank", "noopener");
    } catch { /* noop */ }
  };

  const rerender = () => {
    const state = store.get();
    const disp = pick(state, "dashboard.display", null);

    const device = pick(disp, "device", null);
    const kioskUrl = pick(disp, "kiosk_url", null);
    const w = pick(disp, "width", null);
    const h = pick(disp, "height", null);
    const refresh = pick(disp, "refresh_hz", null);
    const content = pick(disp, "content", null);

    const hasDisplay = device || (w && h) || kioskUrl;
    const overall = hasDisplay ? "ok" : "idle";
    const dot = node.querySelector(".status-dot");
    if (dot) dot.className = `status-dot status-dot--${overall}`;

    if (!hasDisplay) {
      body.replaceChildren(el("p", { className: "panel-empty text-faint", text: "No display attached" }));
      return;
    }

    const res = (w && h) ? `${w}x${h}` : "-";
    const refreshStr = refresh != null ? `${fmtNum(refresh, 0)} Hz` : "-";

    const rows = [
      row("device", device ? String(device) : "-"),
      row("res / refresh", `${res} ${refreshStr}`),
      row("content", content ? String(content) : "-"),
    ];

    const kioskRow = el("div", { className: "panel__row" },
      el("span", { className: "panel__row-label", text: "kiosk url" }),
      kioskUrl
        ? el("button", {
            type: "button",
            className: "panel__row-value mono btn btn--ghost btn--sm",
            text: String(kioskUrl),
            title: "open in new tab",
            onclick: openKiosk,
          })
        : el("span", { className: "panel__row-value mono", text: "-" }),
    );

    const actions = kioskUrl
      ? el("div", { className: "panel-actions-row" },
          el("button", { type: "button", className: "btn btn--sm", text: "open kiosk", onclick: openKiosk }),
        )
      : null;

    body.replaceChildren(...rows, kioskRow, actions);
  };

  const unsub = store.subscribe(rerender);
  rerender();

  if (opts.palette && opts.palette.registry) {
    opts.palette.registry.register("display.open_kiosk", {
      label: "display: open kiosk",
      verb: "open",
      action: openKiosk,
    });
  }

  return {
    node,
    dispose: () => {
      try { unsub && unsub(); } catch { /* noop */ }
      if (opts.palette && opts.palette.registry) {
        try { opts.palette.registry.unregister("display.open_kiosk"); } catch { /* noop */ }
      }
    },
  };
}
