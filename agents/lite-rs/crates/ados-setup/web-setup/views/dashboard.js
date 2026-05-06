// Dashboard view. Renders a stat-tile row plus a panel grid. Profile-
// conditional panels swap based on store.status.profile. Panel bodies are
// placeholders; the live data sources land in a later iteration.

import { el, panel, statTile, cn } from "../components.js";
import { apiFetch } from "../state.js";
import { toast } from "../components/toast.js";
import { attachPullToRefresh, attachLongPress } from "../components/gestures.js";

const STAT_LABELS = ["MAV", "VID", "NET", "CLD", "PAIR"];

function placeholder(text) {
  return el("p", { className: "panel-empty text-faint", text });
}

function profileOf(status) {
  if (!status) return null;
  return status.profile || status.detected_profile || null;
}

function dronePanels() {
  return [
    panel({ title: "live video", span: 8, expandable: true, severity: "idle",
      body: placeholder("Live video lands in a later iteration.") }),
    panel({ title: "flight controller", span: 4, expandable: true, severity: "idle",
      body: placeholder("FC HUD lands in a later iteration.") }),
    panel({ title: "mavlink rates", span: 6, expandable: true, severity: "idle",
      body: placeholder("Per-message rate table lands in a later iteration.") }),
    panel({ title: "camera pipeline", span: 6, expandable: true, severity: "idle",
      body: placeholder("Camera pipeline detail lands in a later iteration.") }),
    panel({ title: "sensors", span: 4, expandable: true, severity: "idle",
      body: placeholder("Sensor health chips land in a later iteration.") }),
    panel({ title: "plugins", span: 4, expandable: true, severity: "idle",
      body: placeholder("Installed plugin list lands in a later iteration.") }),
  ];
}

function groundPanels() {
  return [
    panel({ title: "wfb-rx", span: 6, expandable: true, severity: "idle",
      body: placeholder("WFB receive stats land in a later iteration.") }),
    panel({ title: "mesh", span: 6, expandable: true, severity: "idle",
      body: placeholder("batman-adv peer list lands in a later iteration.") }),
    panel({ title: "stream sources", span: 6, expandable: true, severity: "idle",
      body: placeholder("Aggregated stream sources land in a later iteration.") }),
    panel({ title: "local display", span: 6, expandable: true, severity: "idle",
      body: placeholder("Display panel lands in a later iteration.") }),
    panel({ title: "oled + buttons", span: 6, expandable: true, severity: "idle",
      body: placeholder("OLED screen and button mapping lands in a later iteration.") }),
    panel({ title: "joystick", span: 6, expandable: true, severity: "idle",
      body: placeholder("Joystick HID preview lands in a later iteration.") }),
  ];
}

function commonPanels() {
  return [
    panel({ title: "cloud", span: 6, expandable: true, severity: "idle",
      body: placeholder("Cloud relay state lands in a later iteration.") }),
    panel({ title: "network", span: 6, expandable: true, severity: "idle",
      body: placeholder("Uplink matrix lands in a later iteration.") }),
    panel({ title: "services", span: 12, expandable: true, severity: "idle",
      body: placeholder("Per-service status lands in a later iteration.") }),
  ];
}

function renderStatRow() {
  const row = el("div", { className: "stat-row" });
  for (const label of STAT_LABELS) {
    row.appendChild(statTile({
      label,
      value: "-",
      sub: "loading",
      sparkPoints: [],
      severity: "idle",
      hotkey: String(STAT_LABELS.indexOf(label) + 1),
    }));
  }
  return row;
}

export function renderDashboard(targetEl, { store, palette }) {
  registerVerbs(palette);

  const grid = el("div", { className: "dashboard-grid" });
  const statRow = renderStatRow();

  const root = el("section", { className: "view view-dashboard", "data-view": "dashboard" },
    statRow,
    grid,
  );

  const rerender = () => {
    grid.replaceChildren();
    const profile = profileOf(store.get().status);
    const panels = profile === "ground_station" ? groundPanels() : dronePanels();
    for (const p of panels) grid.appendChild(p);
    for (const p of commonPanels()) grid.appendChild(p);
  };

  const unsub = store.subscribe(rerender);
  rerender();

  targetEl.replaceChildren(root);

  const detachPull = attachPullToRefresh(targetEl, {
    onRefresh: async () => {
      try {
        const data = await apiFetch("/api/v1/setup/status");
        store.set({ status: data });
        toast({ message: "snapshot refreshed", severity: "ok", ttlMs: 1500 });
      } catch (err) {
        toast({ message: `refresh failed: ${err.message}`, severity: "err" });
      }
    },
  });
  const detachLong = attachLongPress(grid, {
    onLongPress: (target) => {
      const panelEl = target && target.closest && target.closest(".panel");
      if (!panelEl) return;
      panelEl.classList.toggle("is-expanded");
    },
  });

  return {
    dispose: () => {
      try { unsub && unsub(); } catch {}
      try { detachPull && detachPull(); } catch {}
      try { detachLong && detachLong(); } catch {}
    },
  };
}

function registerVerbs(palette) {
  if (!palette || !palette.registry) return;
  palette.registry.register("dashboard.refresh", {
    label: "refresh status snapshot",
    verb: "refresh",
    action: async () => {
      try {
        await apiFetch("/api/v1/setup/status");
        toast({ message: "snapshot refreshed", severity: "ok" });
      } catch (err) {
        toast({ message: `refresh failed: ${err.message}`, severity: "err" });
      }
    },
  });
}
