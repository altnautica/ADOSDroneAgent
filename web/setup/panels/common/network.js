// Network panel. Uplink matrix: WiFi AP, WiFi client, Ethernet, USB
// tether, 4G modem. One row each, with state + IP + speed. Reads from
// status.network and dashboard.network so the panel works pre-snapshot.

import { el, panel } from "../../components.js";
import { pick, severityFromState, fmtNum, safeObj } from "../_util.js";

const SLOTS = [
  { key: "wifi_ap", label: "wifi ap" },
  { key: "wifi_client", label: "wifi client" },
  { key: "ethernet", label: "ethernet" },
  { key: "usb_tether", label: "usb tether" },
  { key: "modem_4g", label: "4g modem" },
];

function speedLabel(slot) {
  if (!slot) return null;
  const tx = pick(slot, "tx_kbps", null);
  const rx = pick(slot, "rx_kbps", null);
  if (tx == null && rx == null) {
    const link = pick(slot, "link_speed_mbps", null);
    return link != null ? `${fmtNum(link, 0)} mbps` : null;
  }
  const txS = tx != null ? `↑${fmtNum(tx / 1000, 1)}M` : "↑-";
  const rxS = rx != null ? `↓${fmtNum(rx / 1000, 1)}M` : "↓-";
  return `${txS} ${rxS}`;
}

function row(slotDef, slot) {
  const sev = severityFromState(pick(slot, "state", null));
  const ip = pick(slot, "ip", pick(slot, "address", null));
  const ssid = pick(slot, "ssid", null);
  const speed = speedLabel(slot);

  const right = el("span", { className: "panel__row-value mono", text: ip || ssid ? `${ssid ? `${ssid} ` : ""}${ip || ""}`.trim() : "-" });

  const meta = el("div", { className: "net-row-meta" });
  if (speed) meta.appendChild(el("span", { className: "text-faint mono", text: speed }));

  return el("div", { className: "panel__row net-row" },
    el("span", { className: "panel__row-label" },
      el("span", { className: `dot dot--${sev}`, "aria-hidden": "true" }),
      el("span", { text: ` ${slotDef.label}` }),
    ),
    el("div", { className: "net-row-value" }, right, meta),
  );
}

export function renderNetworkPanel(store, opts = {}) {
  const body = el("div", { className: "panel-stack" });
  const node = panel({
    title: "network",
    span: 6,
    expandable: true,
    severity: "idle",
    body,
  });

  const rerender = () => {
    const state = store.get();
    const fromStatus = safeObj(pick(state, "status.network", null));
    const fromSnap = safeObj(pick(state, "dashboard.network", null));
    const merged = { ...fromStatus, ...fromSnap };

    let worst = "idle";
    const rows = SLOTS.map((slot) => {
      const data = merged[slot.key];
      const sev = severityFromState(pick(data, "state", null));
      if (sev === "err") worst = "err";
      else if (sev === "warn" && worst !== "err") worst = "warn";
      else if (sev === "ok" && worst === "idle") worst = "ok";
      return row(slot, data);
    });

    const dot = node.querySelector(".status-dot");
    if (dot) dot.className = `status-dot status-dot--${worst}`;

    body.replaceChildren(...rows);
  };

  const unsub = store.subscribe(rerender);
  rerender();

  if (opts.palette && opts.palette.registry) {
    opts.palette.registry.register("network.settings", {
      label: "network: open settings",
      verb: "settings",
      action: () => {
        if (opts.router) opts.router.navigate("/settings/network");
      },
    });
  }

  return {
    node,
    dispose: () => {
      try { unsub && unsub(); } catch { /* noop */ }
      if (opts.palette && opts.palette.registry) {
        try { opts.palette.registry.unregister("network.settings"); } catch { /* noop */ }
      }
    },
  };
}
