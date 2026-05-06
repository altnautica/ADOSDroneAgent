// MAVLink rate panel. Per-message rate table with current Hz + 60s
// sparkline per row. Reads from store.state.dashboard.mavlink_rates,
// keyed by msg-name -> { hz, last_ms, recent: [hz, hz, ...] }. We keep
// our own ring buffer per message because the snapshot endpoint only
// returns current Hz; the buffer here is the at-a-glance trend.
//
// Unknown messages are folded into an "other" row so the table never
// scrolls in the dense default panel size.

import { el, panel, sparkline } from "../../components.js";
import { pick, fmtNum, createRingBuffer, safeObj } from "../_util.js";

const TRACKED = ["HEARTBEAT", "ATTITUDE", "GLOBAL_POSITION_INT", "RC_CHANNELS", "SYS_STATUS"];

function rowSeverity(name, hz) {
  if (hz == null) return "idle";
  if (name === "HEARTBEAT") return hz >= 0.5 && hz <= 2 ? "ok" : "warn";
  if (name === "ATTITUDE") return hz >= 10 ? "ok" : (hz > 0 ? "warn" : "err");
  if (name === "GLOBAL_POSITION_INT") return hz >= 1 ? "ok" : (hz > 0 ? "warn" : "idle");
  if (name === "RC_CHANNELS") return hz >= 1 ? "ok" : "idle";
  if (name === "SYS_STATUS") return hz >= 0.5 ? "ok" : "warn";
  return "idle";
}

export function renderMavlinkPanel(store, opts = {}) {
  const buffers = new Map();
  for (const name of TRACKED) buffers.set(name, createRingBuffer(60));

  const body = el("div", { className: "panel-stack" });
  const node = panel({
    title: "mavlink rates",
    span: 6,
    expandable: true,
    severity: "idle",
    body,
  });

  const headerRow = el("div", { className: "panel__row mono", style: { color: "var(--text-dim)", fontSize: "11px" } },
    el("span", { className: "panel__row-label", text: "msg · hz · last" }),
    el("span", { className: "panel__row-value", text: "60s" }),
  );

  const rerender = () => {
    const state = store.get();
    const rates = safeObj(pick(state, "dashboard.mavlink_rates", null));

    let worst = "idle";
    const rows = [headerRow];

    for (const name of TRACKED) {
      const entry = rates[name] || rates[name.toLowerCase()] || null;
      const hz = pick(entry, "hz", null);
      const lastMs = pick(entry, "last_ms", null);
      const buf = buffers.get(name);
      if (hz != null) buf.push(hz);

      const sev = rowSeverity(name, hz);
      if (sev === "err" && worst !== "err") worst = "err";
      else if (sev === "warn" && worst === "idle") worst = "warn";
      else if (sev === "ok" && worst === "idle") worst = "ok";

      const left = el("span", { className: "panel__row-label mono" },
        el("span", { className: `dot dot--${sev}`, "aria-hidden": "true" }),
        el("span", { text: ` ${name}` }),
      );
      const mid = el("span", { className: "panel__row-value mono", text: `${fmtNum(hz, hz != null && hz < 10 ? 1 : 0)} Hz · ${lastMs != null ? `${fmtNum(lastMs, 0)}ms` : "-"}` });
      const sparkBox = el("span", { style: { width: "80px", height: "16px", display: "inline-block", color: "var(--info)" } },
        sparkline(buf.points(), { width: 80, height: 16 }),
      );

      rows.push(el("div", { className: "panel__row" }, left, mid, sparkBox));
    }

    const dot = node.querySelector(".status-dot");
    if (dot) dot.className = `status-dot status-dot--${worst}`;

    body.replaceChildren(...rows);
  };

  const unsub = store.subscribe(rerender);
  rerender();

  if (opts.palette && opts.palette.registry) {
    opts.palette.registry.register("mavlink.tail_logs", {
      label: "mavlink: tail logs",
      verb: "tail",
      action: () => {
        if (opts.router) opts.router.navigate("/logs/ados-mavlink");
      },
    });
  }

  return {
    node,
    dispose: () => {
      try { unsub && unsub(); } catch { /* noop */ }
      if (opts.palette && opts.palette.registry) {
        try { opts.palette.registry.unregister("mavlink.tail_logs"); } catch { /* noop */ }
      }
    },
  };
}
