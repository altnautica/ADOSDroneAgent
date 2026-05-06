// Dashboard view. Renders a stat-tile row plus a panel grid. Profile-
// conditional panels swap based on store.status.profile. Drone, ground,
// and common panels are all data-driven and read from
// store.state.dashboard.

import { el, statTile } from "../components.js";
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
  renderWfbRxPanel,
  renderMeshPanel,
  renderSourcesPanel,
  renderDisplayPanel,
  renderOledButtonsPanel,
  renderJoystickPanel,
  renderCloudPanel,
  renderNetworkPanel,
  renderServicesPanel,
} from "../panels/index.js";

const STAT_LABELS = ["MAV", "VID", "NET", "CLD", "PAIR"];

function profileOf(status) {
  if (!status) return null;
  return status.profile || status.detected_profile || null;
}

function groundRoleOf(status) {
  if (!status) return "direct";
  const role = status.ground_role;
  if (typeof role !== "string") return "direct";
  const r = role.toLowerCase();
  if (r === "direct" || r === "relay" || r === "receiver") return r;
  return "direct";
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

function groundPanels(store, opts, role) {
  // Always-on ground panels.
  const out = [
    renderWfbRxPanel(store, opts),
    renderDisplayPanel(store, opts),
    renderOledButtonsPanel(store, opts),
    renderJoystickPanel(store, opts),
  ];
  // Mesh panel only makes sense for relay and receiver roles.
  if (role === "relay" || role === "receiver") {
    out.splice(1, 0, renderMeshPanel(store, opts));
  }
  // Sources panel is receiver-only.
  if (role === "receiver") {
    out.splice(2, 0, renderSourcesPanel(store, opts));
  }
  return out;
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
  // the profile or ground role flips, so steady-state re-renders happen
  // inside the panels themselves and not in the view.
  let activePanels = [];
  let activeProfile = null;
  let activeRole = null;

  const panelOpts = { palette, router };

  const disposeActive = () => {
    for (const p of activePanels) {
      try { p.dispose && p.dispose(); } catch (err) { console.warn(err); }
    }
    activePanels = [];
  };

  const buildFor = (profile, role) => {
    disposeActive();
    grid.replaceChildren();
    const profilePanels = profile === "ground_station"
      ? groundPanels(store, panelOpts, role)
      : dronePanels(store, panelOpts);
    const common = commonPanels(store, panelOpts);
    activePanels = [...profilePanels, ...common];
    for (const p of activePanels) grid.appendChild(p.node);
    activeProfile = profile;
    activeRole = role;
  };

  const onState = () => {
    const status = store.get().status;
    const profile = profileOf(status);
    const role = groundRoleOf(status);
    if (profile !== activeProfile || (profile === "ground_station" && role !== activeRole)) {
      buildFor(profile, role);
    }
  };

  const unsub = store.subscribe(onState);
  {
    const initStatus = store.get().status;
    buildFor(profileOf(initStatus), groundRoleOf(initStatus));
  }

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
