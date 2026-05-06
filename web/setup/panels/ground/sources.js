// Stream sources panel. Aggregated bitrate plus per-source rows showing
// FEC recovery counts and frame counts. Reads
// store.state.dashboard.sources. Receiver role only; other roles see an
// empty-state card so the panel still renders during a role flip.

import { el, panel, sparkline } from "../../components.js";
import { pick, safeArr, fmtNum, fmtBitrate, createRingBuffer } from "../_util.js";

function fecSeverity(failed) {
  if (failed == null) return "idle";
  if (failed > 10) return "err";
  if (failed > 0) return "warn";
  return "ok";
}

function sourceRow(src) {
  const adapter = String(pick(src, "adapter", "?"));
  const recovered = pick(src, "fec_recovered", null);
  const failed = pick(src, "fec_failed", null);
  const framesIn = pick(src, "frames_in", null);
  const sev = fecSeverity(failed);

  const left = el("span", { className: "panel__row-label mono" },
    el("span", { className: `dot dot--${sev}`, "aria-hidden": "true" }),
    el("span", { text: ` ${adapter}` }),
  );
  const right = el("span", { className: "panel__row-value mono", text:
    `${framesIn != null ? fmtNum(framesIn, 0) : "-"} in · ${recovered != null ? fmtNum(recovered, 0) : "-"} ok / ${failed != null ? fmtNum(failed, 0) : "-"} fail`
  });
  return el("div", { className: "panel__row" }, left, right);
}

function row(label, value, severity) {
  const v = el("span", { className: "panel__row-value mono", text: value != null ? String(value) : "-" });
  if (severity) v.classList.add(`text--${severity}`);
  return el("div", { className: "panel__row" },
    el("span", { className: "panel__row-label", text: label }),
    v,
  );
}

export function renderSourcesPanel(store, opts = {}) {
  const kbpsBuf = createRingBuffer(60);
  const body = el("div", { className: "panel-stack" });
  const node = panel({
    title: "stream sources",
    span: 6,
    expandable: true,
    severity: "idle",
    body,
  });

  const rerender = () => {
    const state = store.get();
    const role = String(pick(state, "status.ground_role", "direct") || "direct").toLowerCase();
    const sources = pick(state, "dashboard.sources", null);

    if (role !== "receiver") {
      const dot = node.querySelector(".status-dot");
      if (dot) dot.className = "status-dot status-dot--idle";
      body.replaceChildren(el("p", { className: "panel-empty text-faint", text: "Receiver role only" }));
      return;
    }

    const aggKbps = pick(sources, "aggregated_kbps", null);
    const combined = pick(sources, "frames_combined", null);
    const dedup = pick(sources, "frames_dedup", null);
    const perSource = safeArr(pick(sources, "per_source", null));

    if (aggKbps != null) kbpsBuf.push(aggKbps);

    let worstFail = 0;
    for (const s of perSource) {
      const f = pick(s, "fec_failed", null);
      if (f != null && f > worstFail) worstFail = f;
    }
    const overall = worstFail > 10 ? "err" : (worstFail > 0 ? "warn" : (perSource.length > 0 ? "ok" : "idle"));

    const dot = node.querySelector(".status-dot");
    if (dot) dot.className = `status-dot status-dot--${overall}`;

    const summary = [
      row("aggregated", fmtBitrate(aggKbps)),
      row("frames combined", combined != null ? fmtNum(combined, 0) : "-"),
      row("frames dedup", dedup != null ? fmtNum(dedup, 0) : "-"),
    ];

    const sparkRow = el("div", { className: "panel__row" },
      el("span", { className: "panel__row-label", text: "kbps 60s" }),
      el("span", { style: { width: "120px", height: "20px", display: "inline-block", color: "var(--info)" } },
        sparkline(kbpsBuf.points(), { width: 120, height: 20 }),
      ),
    );

    const sourceHead = el("div", { className: "panel__row mono", style: { color: "var(--text-dim)", fontSize: "11px" } },
      el("span", { className: "panel__row-label", text: "adapter" }),
      el("span", { className: "panel__row-value", text: "frames · fec ok / fail" }),
    );

    const sourceRows = perSource.length
      ? perSource.slice(0, 8).map(sourceRow)
      : [el("p", { className: "panel-empty text-faint", text: "no sources" })];

    body.replaceChildren(...summary, sparkRow, sourceHead, ...sourceRows);
  };

  const unsub = store.subscribe(rerender);
  rerender();

  if (opts.palette && opts.palette.registry) {
    opts.palette.registry.register("sources.tail_logs", {
      label: "sources: tail receiver logs",
      verb: "tail",
      action: () => {
        if (opts.router) opts.router.navigate("/logs/ados-wfb-receiver");
      },
    });
  }

  return {
    node,
    dispose: () => {
      try { unsub && unsub(); } catch { /* noop */ }
      if (opts.palette && opts.palette.registry) {
        try { opts.palette.registry.unregister("sources.tail_logs"); } catch { /* noop */ }
      }
    },
  };
}
