// Cloud panel. Relay state (mqtt + http heartbeat), pairing code (masked,
// click-to-reveal), drone ID, "open in mission control" deep-link.
// Sources: status.cloud_choice + status.remote_access (the existing setup
// snapshot) and dashboard.cloud (live relay state).

import { el, panel, sparkline, copyText } from "../../components.js";
import { toast } from "../../components/toast.js";
import { pick, fmtNum, createRingBuffer, severityFromState, maskCode } from "../_util.js";

const MC_BASE = "https://mc.altnautica.com";

function rttSeverity(ms) {
  if (ms == null) return "idle";
  if (ms > 1000) return "err";
  if (ms > 300) return "warn";
  return "ok";
}

function pairingChip(code, revealed, onToggle) {
  const text = code ? (revealed ? code : maskCode(code)) : "-";
  return el("button", {
    type: "button",
    className: "pairing-chip mono",
    title: code ? (revealed ? "click to copy" : "click to reveal") : "no pairing code",
    onclick: async () => {
      if (!code) return;
      if (!revealed) { onToggle(); return; }
      await copyText(code);
    },
    text,
  });
}

export function renderCloudPanel(store, opts = {}) {
  const rttBuf = createRingBuffer(60);
  let revealed = false;

  const body = el("div", { className: "panel-stack" });
  const node = panel({
    title: "cloud",
    span: 6,
    expandable: true,
    severity: "idle",
    body,
  });

  const rerender = () => {
    const state = store.get();
    const status = state.status || {};
    const choice = pick(status, "cloud_choice", "altnautica");
    const remote = pick(status, "remote_access", null);
    const cloud = pick(state, "dashboard.cloud", null);

    const mqttState = pick(cloud, "mqtt_state", pick(remote, "mqtt_state", null));
    const httpState = pick(cloud, "http_state", pick(remote, "http_state", null));
    const rtt = pick(cloud, "rtt_ms", null);
    const droneId = pick(cloud, "drone_id", pick(status, "device_id", null));
    const pairing = pick(cloud, "pairing_code", pick(status, "pairing_code", null));

    if (rtt != null) rttBuf.push(rtt);

    const mqttSev = severityFromState(mqttState);
    const httpSev = severityFromState(httpState);
    const overall = (mqttSev === "err" || httpSev === "err") ? "err"
      : (mqttSev === "warn" || httpSev === "warn") ? "warn"
      : (mqttSev === "ok" && httpSev === "ok") ? "ok" : "idle";

    const dot = node.querySelector(".status-dot");
    if (dot) dot.className = `status-dot status-dot--${overall}`;

    const choiceRow = el("div", { className: "panel__row" },
      el("span", { className: "panel__row-label", text: "relay" }),
      el("span", { className: "panel__row-value mono", text: String(choice) }),
    );

    const mqttRow = el("div", { className: "panel__row" },
      el("span", { className: "panel__row-label" },
        el("span", { className: `dot dot--${mqttSev}`, "aria-hidden": "true" }),
        el("span", { text: " mqtt" }),
      ),
      el("span", { className: "panel__row-value mono", text: mqttState ? String(mqttState) : "-" }),
    );

    const httpRow = el("div", { className: "panel__row" },
      el("span", { className: "panel__row-label" },
        el("span", { className: `dot dot--${httpSev}`, "aria-hidden": "true" }),
        el("span", { text: " http" }),
      ),
      el("span", { className: "panel__row-value mono", text: httpState ? String(httpState) : "-" }),
    );

    const rttRow = el("div", { className: "panel__row" },
      el("span", { className: "panel__row-label", text: "rtt" }),
      el("span", { className: `panel__row-value mono text--${rttSeverity(rtt)}`, text: rtt != null ? `${fmtNum(rtt, 0)} ms` : "-" }),
    );

    const sparkRow = el("div", { className: "panel__row" },
      el("span", { className: "panel__row-label", text: "60s" }),
      el("span", { style: { width: "120px", height: "20px", display: "inline-block", color: "var(--info)" } },
        sparkline(rttBuf.points(), { width: 120, height: 20 }),
      ),
    );

    const droneRow = el("div", { className: "panel__row" },
      el("span", { className: "panel__row-label", text: "drone id" }),
      el("button", {
        type: "button",
        className: "panel__row-value mono btn btn--ghost btn--sm",
        text: droneId ? String(droneId) : "-",
        title: "click to copy",
        onclick: () => { if (droneId) copyText(droneId); },
      }),
    );

    const pairingRow = el("div", { className: "panel__row" },
      el("span", { className: "panel__row-label", text: "pairing" }),
      pairingChip(pairing, revealed, () => { revealed = true; rerender(); }),
    );

    const mcUrl = droneId ? `${MC_BASE}/command?drone=${encodeURIComponent(droneId)}` : MC_BASE;
    const mcBtn = el("a", {
      className: "btn btn--sm",
      href: mcUrl,
      target: "_blank",
      rel: "noopener",
      text: "open in mission control",
    });
    const actions = el("div", { className: "panel-actions-row" }, mcBtn);

    body.replaceChildren(choiceRow, mqttRow, httpRow, rttRow, sparkRow, droneRow, pairingRow, actions);
  };

  const unsub = store.subscribe(rerender);
  rerender();

  if (opts.palette && opts.palette.registry) {
    opts.palette.registry.register("cloud.copy_pairing", {
      label: "cloud: copy pairing code",
      verb: "copy",
      action: async () => {
        const code = pick(store.get(), "dashboard.cloud.pairing_code", pick(store.get(), "status.pairing_code", null));
        if (!code) { toast({ message: "no pairing code", severity: "warn" }); return; }
        await copyText(code);
      },
    });
    opts.palette.registry.register("cloud.tail_logs", {
      label: "cloud: tail relay logs",
      verb: "tail",
      action: () => {
        if (opts.router) opts.router.navigate("/logs/ados-cloud-relay");
      },
    });
  }

  return {
    node,
    dispose: () => {
      try { unsub && unsub(); } catch { /* noop */ }
      if (opts.palette && opts.palette.registry) {
        try { opts.palette.registry.unregister("cloud.copy_pairing"); } catch { /* noop */ }
        try { opts.palette.registry.unregister("cloud.tail_logs"); } catch { /* noop */ }
      }
    },
  };
}
