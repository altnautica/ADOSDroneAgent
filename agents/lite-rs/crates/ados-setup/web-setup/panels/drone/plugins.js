// Plugin panel. Lists installed plugins with state pill and capability
// badges. Reads store.state.dashboard.plugins, an array of
// { id, name, state, capabilities[] }. Empty state renders a single
// muted message instead of blank space.

import { el, panel } from "../../components.js";
import { pick, safeArr, severityFromState } from "../_util.js";

function pluginRow(p) {
  const sev = severityFromState(p.state);
  const head = el("div", { className: "plugin-row-head" },
    el("span", { className: `dot dot--${sev}`, "aria-hidden": "true" }),
    el("span", { className: "plugin-row-name mono", text: p.name || p.id || "?" }),
    el("span", { className: `pill pill--${sev}`, text: String(p.state || "unknown") }),
  );

  const caps = safeArr(p.capabilities);
  const capRow = caps.length
    ? el("div", { className: "plugin-row-caps" },
      ...caps.slice(0, 8).map((c) => el("span", { className: "pill pill--idle pill--sm mono", text: String(c) })),
      caps.length > 8 ? el("span", { className: "text-faint mono", text: `+${caps.length - 8}` }) : null,
    )
    : null;

  return el("div", { className: "plugin-row" }, head, capRow);
}

export function renderPluginsPanel(store, opts = {}) {
  const body = el("div", { className: "panel-stack" });
  const node = panel({
    title: "plugins",
    span: 4,
    expandable: true,
    severity: "idle",
    body,
  });

  const rerender = () => {
    const state = store.get();
    const list = safeArr(pick(state, "dashboard.plugins", null));

    let worst = "idle";
    for (const p of list) {
      const sev = severityFromState(p.state);
      if (sev === "err") worst = "err";
      else if (sev === "warn" && worst !== "err") worst = "warn";
      else if (sev === "ok" && worst === "idle") worst = "ok";
    }
    const dot = node.querySelector(".status-dot");
    if (dot) dot.className = `status-dot status-dot--${worst}`;

    if (!list.length) {
      body.replaceChildren(el("p", { className: "panel-empty text-faint", text: "no plugins installed" }));
      return;
    }
    body.replaceChildren(...list.map(pluginRow));
  };

  const unsub = store.subscribe(rerender);
  rerender();

  if (opts.palette && opts.palette.registry) {
    opts.palette.registry.register("plugins.list", {
      label: "plugins: open list",
      verb: "list",
      action: () => {
        if (opts.router) opts.router.navigate("/settings/advanced");
      },
    });
  }

  return {
    node,
    dispose: () => {
      try { unsub && unsub(); } catch { /* noop */ }
      if (opts.palette && opts.palette.registry) {
        try { opts.palette.registry.unregister("plugins.list"); } catch { /* noop */ }
      }
    },
  };
}
