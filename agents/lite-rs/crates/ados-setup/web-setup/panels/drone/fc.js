// Flight controller panel. Vehicle, firmware, mode, armed state, GPS,
// battery, link quality, RC, prearm pass/fail. Reads from
// store.state.dashboard.fc plus a fallback hop into status.mavlink for
// systems that haven't yet started the dashboard snapshot loop.
//
// Mode chip is colour-coded: stabilize/loiter/auto/guided -> ok,
// rtl/land/alt-hold -> info, manual/acro -> warn, failsafe -> err.

import { el, panel, sparkline } from "../../components.js";
import { pick, fmtNum, createRingBuffer } from "../_util.js";

const ARMED_MODES = new Set(["stabilize", "althold", "loiter", "auto", "guided", "poshold", "circle", "rtl", "land", "drift", "sport", "flip", "autotune"]);
const SAFE_MODES = new Set(["stabilize", "althold", "loiter", "auto", "guided", "poshold"]);
const RECOVERY_MODES = new Set(["rtl", "land", "smart_rtl", "brake"]);
const RAW_MODES = new Set(["manual", "acro", "sport", "drift"]);

function modeSeverity(mode) {
  if (!mode) return "idle";
  const m = String(mode).toLowerCase();
  if (m.includes("failsafe") || m.includes("hold") && m.includes("fence")) return "err";
  if (RECOVERY_MODES.has(m)) return "info";
  if (SAFE_MODES.has(m)) return "ok";
  if (RAW_MODES.has(m)) return "warn";
  if (ARMED_MODES.has(m)) return "info";
  return "idle";
}

function renderRow(label, value, severity) {
  const valEl = el("span", { className: "panel__row-value mono", text: value != null ? String(value) : "-" });
  if (severity) valEl.classList.add(`text--${severity}`);
  return el("div", { className: "panel__row" },
    el("span", { className: "panel__row-label", text: label }),
    valEl,
  );
}

function gpsLine(gps) {
  if (!gps) return "no fix";
  const fix = pick(gps, "fix_type", pick(gps, "fix", null));
  const sats = pick(gps, "satellites_visible", pick(gps, "sats", null));
  const fixLabel = fix == null ? "?" : (fix >= 3 ? `${fix}D` : `fix ${fix}`);
  const satStr = sats != null ? ` ${sats}` : "";
  return `${fixLabel}${satStr}`;
}

function gpsSeverity(gps) {
  if (!gps) return "idle";
  const fix = pick(gps, "fix_type", pick(gps, "fix", 0));
  if (fix == null || fix < 2) return "err";
  if (fix < 3) return "warn";
  return "ok";
}

function batterySeverity(remaining) {
  if (remaining == null) return "idle";
  if (remaining < 20) return "err";
  if (remaining < 40) return "warn";
  return "ok";
}

function linkSeverity(pct) {
  if (pct == null) return "idle";
  if (pct < 30) return "err";
  if (pct < 70) return "warn";
  return "ok";
}

export function renderFcPanel(store, opts = {}) {
  const linkBuf = createRingBuffer(60);
  const body = el("div", { className: "panel-stack" });
  const node = panel({
    title: "flight controller",
    span: 4,
    expandable: true,
    severity: "idle",
    body,
  });

  const rerender = () => {
    const state = store.get();
    const fc = pick(state, "dashboard.fc", null);
    const status = state.status || {};

    const vehicle = pick(fc, "vehicle", "-");
    const firmware = pick(fc, "firmware", pick(status, "mavlink.firmware", "-"));
    const mode = pick(fc, "mode", "-");
    const armed = pick(fc, "armed", null);
    const gps = pick(fc, "gps", null);
    const batt = pick(fc, "battery", null);
    const linkPct = pick(fc, "link_quality", null);
    const rc = pick(fc, "rc", null);
    const prearm = pick(fc, "prearm", null);

    if (linkPct != null) linkBuf.push(linkPct);

    const overall = armed ? "warn" : (prearm === false ? "err" : (mode && mode !== "-" ? "ok" : "idle"));
    const dot = node.querySelector(".status-dot");
    if (dot) dot.className = `status-dot status-dot--${overall}`;

    const modeChip = el("span", {
      className: `pill pill--${modeSeverity(mode)}`,
      text: String(mode || "-").toUpperCase(),
    });
    const armChip = el("span", {
      className: `pill pill--${armed ? "warn" : "idle"} pill--solid`,
      text: armed ? "ARMED" : "DISARMED",
    });

    const headRow = el("div", { className: "panel-chip-row" }, modeChip, armChip);

    const rows = [
      renderRow("vehicle", `${vehicle}`),
      renderRow("firmware", `${firmware}`),
      renderRow("gps", gpsLine(gps), gpsSeverity(gps)),
      renderRow(
        "battery",
        batt ? `${fmtNum(pick(batt, "voltage", null), 1)}V ${pick(batt, "remaining", null) != null ? `${fmtNum(batt.remaining, 0)}%` : ""}`.trim() : "-",
        batterySeverity(pick(batt, "remaining", null)),
      ),
      renderRow("link", linkPct != null ? `${fmtNum(linkPct, 0)}%` : "-", linkSeverity(linkPct)),
      renderRow("rc", rc ? String(rc) : "-"),
      renderRow("prearm", prearm == null ? "-" : (prearm ? "pass" : "fail"), prearm == null ? null : (prearm ? "ok" : "err")),
    ];

    const sparkRow = el("div", { className: "panel__row" },
      el("span", { className: "panel__row-label", text: "link 60s" }),
      el("span", { style: { width: "100px", height: "20px", display: "inline-block", color: "var(--info)" } },
        sparkline(linkBuf.points(), { width: 100, height: 20 }),
      ),
    );

    body.replaceChildren(headRow, ...rows, sparkRow);
  };

  const unsub = store.subscribe(rerender);
  rerender();

  if (opts.palette && opts.palette.registry) {
    opts.palette.registry.register("fc.tail_logs", {
      label: "fc: tail mavlink logs",
      verb: "logs",
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
        try { opts.palette.registry.unregister("fc.tail_logs"); } catch { /* noop */ }
      }
    },
  };
}
