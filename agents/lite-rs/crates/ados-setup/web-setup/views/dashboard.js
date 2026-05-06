// Dashboard view. Renders a stat-tile row plus a panel grid. Profile-
// conditional panels swap based on store.status.profile. Drone-profile
// and common panels are now data-driven; ground-profile panels remain
// placeholders until the next iteration.

import { el, panel, statTile } from "../components.js";
import { apiFetch } from "../state.js";
import { toast } from "../components/toast.js";
import { attachPullToRefresh, attachLongPress } from "../components/gestures.js";
import {
  renderVideoPanel,
  renderFcPanel,
  renderMavlinkPanel,
  renderCameraPanel,
  renderSensorsPanel,
  renderPluginsPanel,
  renderCloudPanel,
  renderNetworkPanel,
  renderServicesPanel,
} from "../panels/index.js";

const STAT_LABELS = ["MAV", "VID", "NET", "CLD", "PAIR"];

function placeholder(text) {
  return el("p", { className: "panel-empty text-faint", text });
}

function profileOf(status) {
  if (!status) return null;
  return status.profile || status.detected_profile || null;
}

function dronePanels(store, opts) {
  return [
    renderVideoPanel(store, opts),
    renderFcPanel(store, opts),
    renderMavlinkPanel(store, opts),
    renderCameraPanel(store, opts),
    renderSensorsPanel(store, opts),
    renderPluginsPanel(store, opts),
  ];
}

function groundPanels() {
  // Ground-profile panels are still placeholders. The next iteration
  // wires WFB-RX, mesh, sources, display, OLED+buttons, and joystick.
  return [
    { node: panel({ title: "wfb-rx", span: 6, expandable: true, severity: "idle",
      body: placeholder("WFB receive stats land in a later iteration.") }), dispose: () => {} },
    { node: panel({ title: "mesh", span: 6, expandable: true, severity: "idle",
      body: placeholder("batman-adv peer list lands in a later iteration.") }), dispose: () => {} },
    { node: panel({ title: "stream sources", span: 6, expandable: true, severity: "idle",
      body: placeholder("Aggregated stream sources land in a later iteration.") }), dispose: () => {} },
    { node: panel({ title: "local display", span: 6, expandable: true, severity: "idle",
      body: placeholder("Display panel lands in a later iteration.") }), dispose: () => {} },
    { node: panel({ title: "oled + buttons", span: 6, expandable: true, severity: "idle",
      body: placeholder("OLED screen and button mapping lands in a later iteration.") }), dispose: () => {} },
    { node: panel({ title: "joystick", span: 6, expandable: true, severity: "idle",
      body: placeholder("Joystick HID preview lands in a later iteration.") }), dispose: () => {} },
  ];
}

function commonPanels(store, opts) {
  return [
    renderCloudPanel(store, opts),
    renderNetworkPanel(store, opts),
    renderServicesPanel(store, opts),
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

export function renderDashboard(targetEl, { store, palette, router }) {
  registerVerbs(palette);

  const grid = el("div", { className: "dashboard-grid" });
  const statRow = renderStatRow();

  const root = el("section", { className: "view view-dashboard", "data-view": "dashboard" },
    statRow,
    grid,
  );

  // The live panels each own a subscription. We rebuild them only when
  // the profile flips, so steady-state re-renders happen inside the
  // panels themselves and not in the view.
  let activePanels = [];
  let activeProfile = null;

  const panelOpts = { palette, router };

  const disposeActive = () => {
    for (const p of activePanels) {
      try { p.dispose && p.dispose(); } catch (err) { console.warn(err); }
    }
    activePanels = [];
  };

  const buildForProfile = (profile) => {
    disposeActive();
    grid.replaceChildren();
    const profilePanels = profile === "ground_station" ? groundPanels() : dronePanels(store, panelOpts);
    const common = commonPanels(store, panelOpts);
    activePanels = [...profilePanels, ...common];
    for (const p of activePanels) grid.appendChild(p.node);
    activeProfile = profile;
  };

  const onState = () => {
    const profile = profileOf(store.get().status);
    if (profile !== activeProfile) buildForProfile(profile);
  };

  const unsub = store.subscribe(onState);
  buildForProfile(profileOf(store.get().status));

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
      try { unsub && unsub(); } catch { /* noop */ }
      try { detachPull && detachPull(); } catch { /* noop */ }
      try { detachLong && detachLong(); } catch { /* noop */ }
      disposeActive();
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
