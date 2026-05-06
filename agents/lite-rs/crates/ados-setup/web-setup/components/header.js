// App header. Renders identity (hostname, profile, board, version, uptime,
// online dot) plus action chrome that varies by viewport. The bottom dock
// owns mobile chrome; the header owns tablet + desktop chrome.

import { el, chip, statusDot, copyText, sheet } from "../components.js";

function fmtUptime(seconds) {
  if (!seconds || seconds < 0) return "-";
  const d = Math.floor(seconds / 86400);
  const h = Math.floor((seconds % 86400) / 3600);
  const m = Math.floor((seconds % 3600) / 60);
  if (d > 0) return `${d}d${String(h).padStart(2, "0")}h`;
  if (h > 0) return `${h}h${String(m).padStart(2, "0")}m`;
  return `${m}m`;
}

function profileLabel(status) {
  if (!status) return "loading";
  return status.profile || status.detected_profile || "unconfigured";
}

function hostnameOf(status) {
  return (status && (status.hostname || status.host || status.device_name)) || location.hostname || "device";
}

function boardOf(status) {
  return (status && (status.board || status.board_id)) || "";
}

function versionOf(status) {
  return (status && (status.agent_version || status.version)) || "";
}

function uptimeOf(status) {
  return (status && (status.uptime_seconds || status.uptime || 0)) || 0;
}

function onlineSeverity(status) {
  if (!status) return "idle";
  if (status.online === false) return "err";
  if (status.degraded) return "warn";
  return "ok";
}

export function mountHeader(rootEl, { store, router, openCommandPalette }) {
  const node = el("header", { className: "app-header", role: "banner" });
  rootEl.appendChild(node);

  const render = () => {
    const status = store.get().status;
    const host = hostnameOf(status);
    const profile = profileLabel(status);
    const board = boardOf(status);
    const version = versionOf(status);
    const uptime = fmtUptime(uptimeOf(status));
    const sev = onlineSeverity(status);

    const hostBtn = el("button", {
      type: "button",
      className: "header-host",
      title: "copy hostname",
      onclick: () => copyText(`${host}.local`),
    }, statusDot(sev), el("span", { className: "mono", text: host }));

    const profileChip = chip({ variant: profile === "drone" ? "info" : profile === "ground_station" ? "accent" : "muted", label: profile });

    const ident = el("div", { className: "header-ident" },
      hostBtn,
      profileChip,
      board ? el("span", { className: "header-meta mono header-meta--board", text: board }) : null,
      version ? el("span", { className: "header-meta mono header-meta--version", text: version }) : null,
      el("span", { className: "header-meta mono header-meta--uptime", text: `up ${uptime}` }),
    );

    const actions = el("div", { className: "header-actions" },
      el("button", {
        type: "button",
        className: "header-btn",
        "aria-label": "open command palette",
        text: "⌘K",
        onclick: () => openCommandPalette && openCommandPalette(),
      }),
      el("button", {
        type: "button",
        className: "header-btn",
        "aria-label": "open settings",
        text: "⚙",
        onclick: () => router.navigate("/settings"),
      }),
      el("button", {
        type: "button",
        className: "header-btn",
        "aria-label": "reboot",
        text: "⏏",
        onclick: () => openRebootSheet(),
      }),
      el("button", {
        type: "button",
        className: "header-btn header-btn--theme",
        "aria-label": "toggle theme",
        text: store.get().theme === "light" ? "☾" : "☼",
        onclick: () => {
          const next = store.get().theme === "light" ? "dark" : "light";
          store.set({ theme: next });
          localStorage.setItem("ados.theme", next);
        },
      }),
    );

    node.replaceChildren(ident, actions);
  };

  const unsub = store.subscribe(render);
  render();
  return {
    rerender: render,
    dispose: () => { try { unsub && unsub(); } catch {} },
  };
}

function openRebootSheet() {
  const s = sheet({
    title: "reboot",
    body: el("p", { className: "sheet-body-text", text: "reboot scheduling lands in a later iteration." }),
    footer: el("button", { type: "button", className: "btn", text: "close", onclick: () => s.close() }),
  });
}
