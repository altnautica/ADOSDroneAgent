// ADOS dashboard SPA. Vanilla ES modules, no bundler. Mounts header,
// bottom dock, toast host, command palette, keyboard handlers, theme.
// Defines routes for dashboard / settings / logs and starts polling.

import { Router } from "./router.js";
import { store, Polling, apiFetch } from "./state.js";
import { mountHeader } from "./components/header.js";
import { mountBottomDock } from "./components/bottom-dock.js";
import { mountToastHost } from "./components/toast.js";
import { mountCommandPalette } from "./components/command-palette.js";
import { mountKeyboard } from "./components/keyboard.js";
import { mountTheme } from "./components/theme.js";
import { renderDashboard } from "./views/dashboard.js";
import { renderSettings } from "./views/settings/index.js";
import { renderLogs } from "./views/logs.js";

const app = document.getElementById("app");

mountTheme(document.documentElement, store);
mountToastHost(app);

const headerHost = document.createElement("div");
const main = document.createElement("main");
main.className = "app-main";
const dockHost = document.createElement("div");
app.appendChild(headerHost);
app.appendChild(main);
app.appendChild(dockHost);

const router = new Router();
const palette = mountCommandPalette(app);

mountHeader(headerHost, { store, router, openCommandPalette: () => palette.open() });
mountBottomDock(dockHost, { router, openCommandPalette: () => palette.open() });
mountKeyboard({ store, router, palette });

let activeView = null;
const renderRoute = (renderer, args) => {
  if (activeView && typeof activeView.dispose === "function") {
    try { activeView.dispose(); } catch (err) { console.warn(err); }
  }
  activeView = renderer(main, args) || null;
};

router
  .route("/", () => renderRoute(renderDashboard, { store, palette, router }))
  .route("/settings", () => renderRoute(renderSettings, { store, palette, router, params: {} }))
  .route("/settings/:section", (params) => renderRoute(renderSettings, { store, palette, router, params }))
  .route("/logs", () => renderRoute(renderLogs, { store, palette, router, params: {} }))
  .route("/logs/:service", (params) => renderRoute(renderLogs, { store, palette, router, params }))
  .route("*", () => renderRoute(renderDashboard, { store, palette, router }));

router.start();

// Seed both snapshots before polling kicks in. Keep going on failure so a
// fresh agent without one or both endpoints still boots the dashboard.
apiFetch("/api/v1/setup/status")
  .then((data) => store.set({ status: data }))
  .catch((err) => console.warn("seed status failed", err && err.message));
apiFetch("/api/v1/dashboard/snapshot")
  .then((data) => store.set({ dashboard: data }))
  .catch((err) => console.warn("seed dashboard failed", err && err.message));

const statusPolling = new Polling({
  url: "/api/v1/setup/status",
  intervalMs: 5000,
  hiddenIntervalMs: 30000,
  store,
  key: "status",
});
statusPolling.start();

const dashboardPolling = new Polling({
  url: "/api/v1/dashboard/snapshot",
  intervalMs: 1000,
  hiddenIntervalMs: 15000,
  store,
  key: "dashboard",
});
dashboardPolling.start();

window.addEventListener("beforeunload", () => {
  try { statusPolling.dispose(); } catch {}
  try { dashboardPolling.dispose(); } catch {}
});
