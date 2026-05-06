// Sensor health panel. IMU / BARO / MAG / GPS / AIRSPEED / RANGEFINDER
// chips coloured by state. Reads store.state.dashboard.sensors which is
// an array of { name, state, detail }. Unknown sensors render as muted
// chips so the panel never hides upstream data.

import { el, panel } from "../../components.js";
import { pick, safeArr, severityFromState } from "../_util.js";

const ORDER = ["imu", "baro", "mag", "gps", "airspeed", "rangefinder", "optical_flow"];

function sortSensors(list) {
  const known = new Map();
  for (const s of list) {
    if (!s || !s.name) continue;
    known.set(String(s.name).toLowerCase(), s);
  }
  const out = [];
  for (const name of ORDER) {
    if (known.has(name)) {
      out.push(known.get(name));
      known.delete(name);
    }
  }
  for (const v of known.values()) out.push(v);
  return out;
}

function chip(sensor) {
  const sev = severityFromState(sensor.state);
  const name = String(sensor.name || "?").toUpperCase();
  const detail = sensor.detail ? String(sensor.detail) : "";
  return el("div", {
    className: `sensor-chip pill pill--${sev}`,
    title: detail || `${name} ${sensor.state || ""}`,
  },
    el("span", { className: `dot dot--${sev}`, "aria-hidden": "true" }),
    el("span", { className: "sensor-chip-name", text: name }),
    detail ? el("span", { className: "sensor-chip-detail mono text-faint", text: detail }) : null,
  );
}

export function renderSensorsPanel(store, opts = {}) {
  const body = el("div", { className: "sensor-grid" });
  const node = panel({
    title: "sensors",
    span: 4,
    expandable: true,
    severity: "idle",
    body,
  });

  const rerender = () => {
    const state = store.get();
    const list = safeArr(pick(state, "dashboard.sensors", null));

    let worst = "idle";
    for (const s of list) {
      const sev = severityFromState(s.state);
      if (sev === "err" && worst !== "err") worst = "err";
      else if (sev === "warn" && worst !== "err") worst = "warn";
      else if (sev === "ok" && worst === "idle") worst = "ok";
    }

    const dot = node.querySelector(".status-dot");
    if (dot) dot.className = `status-dot status-dot--${worst}`;

    if (!list.length) {
      body.replaceChildren(el("p", { className: "panel-empty text-faint", text: "no sensor data" }));
      return;
    }
    const sorted = sortSensors(list);
    body.replaceChildren(...sorted.map(chip));
  };

  const unsub = store.subscribe(rerender);
  rerender();

  if (opts.palette && opts.palette.registry) {
    opts.palette.registry.register("sensors.tail_logs", {
      label: "sensors: tail logs",
      verb: "tail",
      action: () => {
        if (opts.router) opts.router.navigate("/logs/ados-supervisor");
      },
    });
  }

  return {
    node,
    dispose: () => {
      try { unsub && unsub(); } catch { /* noop */ }
      if (opts.palette && opts.palette.registry) {
        try { opts.palette.registry.unregister("sensors.tail_logs"); } catch { /* noop */ }
      }
    },
  };
}
