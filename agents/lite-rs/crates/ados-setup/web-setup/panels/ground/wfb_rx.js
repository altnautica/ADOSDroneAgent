// WFB receive panel. Adapter, channel, frequency, RSSI, packet loss,
// FEC counters, bitrate. Reads store.state.dashboard.wfb_rx. Active on
// every ground role. RSSI sparkline is the worst-stream value over the
// last 60 seconds so the at-a-glance trend reflects link health.

import { el, panel, sparkline } from "../../components.js";
import { pick, fmtNum, fmtBitrate, createRingBuffer, safeArr } from "../_util.js";

function rssiSeverity(rssi) {
  if (rssi == null) return "idle";
  if (rssi < -85) return "err";
  if (rssi < -75) return "warn";
  return "ok";
}

function lossSeverity(pct) {
  if (pct == null) return "idle";
  if (pct >= 10) return "err";
  if (pct >= 2) return "warn";
  return "ok";
}

function row(label, value, severity) {
  const v = el("span", { className: "panel__row-value mono", text: value != null ? String(value) : "-" });
  if (severity) v.classList.add(`text--${severity}`);
  return el("div", { className: "panel__row" },
    el("span", { className: "panel__row-label", text: label }),
    v,
  );
}

function streamChip(stream) {
  const sev = rssiSeverity(pick(stream, "rssi_dbm", null));
  const name = String(pick(stream, "name", "?"));
  const rssi = pick(stream, "rssi_dbm", null);
  const loss = pick(stream, "packet_loss_pct", null);
  const detail = `${rssi != null ? `${fmtNum(rssi, 0)} dBm` : "-"} · ${loss != null ? `${fmtNum(loss, 1)}% loss` : "-"}`;
  return el("div", {
    className: `sensor-chip pill pill--${sev}`,
    title: `${name} ${detail}`,
  },
    el("span", { className: `dot dot--${sev}`, "aria-hidden": "true" }),
    el("span", { className: "sensor-chip-name", text: name }),
    el("span", { className: "sensor-chip-detail mono text-faint", text: detail }),
  );
}

function worstRssi(streams, fallback) {
  let worst = null;
  for (const s of streams) {
    const r = pick(s, "rssi_dbm", null);
    if (r == null) continue;
    if (worst == null || r < worst) worst = r;
  }
  return worst != null ? worst : fallback;
}

export function renderWfbRxPanel(store, opts = {}) {
  const rssiBuf = createRingBuffer(60);
  const body = el("div", { className: "panel-stack" });
  const node = panel({
    title: "wfb-rx",
    span: 6,
    expandable: true,
    severity: "idle",
    body,
  });

  const rerender = () => {
    const state = store.get();
    const wfb = pick(state, "dashboard.wfb_rx", null);

    const adapter = pick(wfb, "adapter", "-");
    const channel = pick(wfb, "channel", null);
    const freq = pick(wfb, "freq_mhz", null);
    const rssi = pick(wfb, "rssi_dbm", null);
    const loss = pick(wfb, "packet_loss_pct", null);
    const fecRecovered = pick(wfb, "fec_recovered", null);
    const fecFailed = pick(wfb, "fec_failed", null);
    const kbps = pick(wfb, "bitrate_kbps", null);
    const streams = safeArr(pick(wfb, "streams", null));

    const trend = worstRssi(streams, rssi);
    if (trend != null) rssiBuf.push(trend);

    const overall = (lossSeverity(loss) === "err" || rssiSeverity(trend) === "err") ? "err"
      : (lossSeverity(loss) === "warn" || rssiSeverity(trend) === "warn") ? "warn"
      : (rssiSeverity(trend) === "ok" ? "ok" : "idle");

    const dot = node.querySelector(".status-dot");
    if (dot) dot.className = `status-dot status-dot--${overall}`;

    const rows = [
      row("adapter", String(adapter || "-")),
      row("channel", channel != null ? `ch ${channel}${freq != null ? ` · ${fmtNum(freq, 0)} MHz` : ""}` : "-"),
      row("rssi", rssi != null ? `${fmtNum(rssi, 0)} dBm` : "-", rssiSeverity(rssi)),
      row("loss", loss != null ? `${fmtNum(loss, 1)}%` : "-", lossSeverity(loss)),
      row("fec ok / fail", `${fecRecovered != null ? fmtNum(fecRecovered, 0) : "-"} / ${fecFailed != null ? fmtNum(fecFailed, 0) : "-"}`,
        fecFailed != null && fecFailed > 0 ? "warn" : null),
      row("bitrate", fmtBitrate(kbps)),
    ];

    const streamRow = streams.length
      ? el("div", { className: "panel-chip-row" }, ...streams.map(streamChip))
      : null;

    const sparkRow = el("div", { className: "panel__row" },
      el("span", { className: "panel__row-label", text: "rssi 60s" }),
      el("span", { style: { width: "120px", height: "20px", display: "inline-block", color: "var(--info)" } },
        sparkline(rssiBuf.points(), { width: 120, height: 20 }),
      ),
    );

    body.replaceChildren(...rows, streamRow, sparkRow);
  };

  const unsub = store.subscribe(rerender);
  rerender();

  if (opts.palette && opts.palette.registry) {
    opts.palette.registry.register("wfb_rx.tail_logs", {
      label: "wfb-rx: tail logs",
      verb: "tail",
      action: () => {
        if (opts.router) opts.router.navigate("/logs/ados-wfb-rx");
      },
    });
  }

  return {
    node,
    dispose: () => {
      try { unsub && unsub(); } catch { /* noop */ }
      if (opts.palette && opts.palette.registry) {
        try { opts.palette.registry.unregister("wfb_rx.tail_logs"); } catch { /* noop */ }
      }
    },
  };
}
