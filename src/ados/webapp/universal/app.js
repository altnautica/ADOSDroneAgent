// ADOS universal setup webapp.
// Single ES module SPA dispatcher. Renders one shared shell per HTML page
// based on document.body.dataset.page, then delegates to a per-page
// renderer. All API data is rendered with textContent / DOM creation; no
// API string is ever passed to innerHTML.

import {
  chip,
  statusDot,
  liveRow,
  streamConsole,
  verifyButton,
  parseMavlinkFrame,
  decodeMavlinkPayload,
} from "./components.js";

const SETUP_TOKEN_KEY = "ados.setup.token";
const POLL_INTERVAL_MS = 5000;

const NAV = [
  { id: "dashboard", href: "/", label: "Dashboard" },
  { id: "setup", href: "/setup.html", label: "Setup" },
  { id: "mavlink", href: "/mavlink.html", label: "MAVLink" },
  { id: "video", href: "/video.html", label: "Video" },
  { id: "network", href: "/network.html", label: "Network" },
  { id: "remote", href: "/remote.html", label: "Remote access" },
  { id: "ground", href: "/ground.html", label: "Ground station" },
  { id: "system", href: "/system.html", label: "System" },
  { id: "advanced", href: "/advanced.html", label: "Advanced" },
];

const subscribers = new Set();
let currentStatus = null;
let pollTimer = null;

// ---------------------------------------------------------------------------
// DOM helpers
// ---------------------------------------------------------------------------

function el(tag, props, ...children) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(props || {})) {
    if (value == null || value === false) continue;
    if (key === "className") node.className = value;
    else if (key === "text") node.textContent = String(value);
    else if (key === "html") node.innerHTML = value; // only used for static markup, never API data
    else if (key === "dataset") Object.assign(node.dataset, value);
    else if (key === "style") Object.assign(node.style, value);
    else if (key.startsWith("on") && typeof value === "function") {
      node.addEventListener(key.slice(2).toLowerCase(), value);
    } else if (key.startsWith("aria-") || key === "role" || key === "for") {
      node.setAttribute(key, String(value));
    } else {
      node[key] = value;
    }
  }
  for (const child of children.flat()) {
    if (child == null || child === false) continue;
    if (typeof child === "string" || typeof child === "number") {
      node.appendChild(document.createTextNode(String(child)));
    } else {
      node.appendChild(child);
    }
  }
  return node;
}

function dot(severity) {
  return el("span", { className: `dot ${severity || "muted"}`, "aria-hidden": "true" });
}

function pill(severity, label) {
  return el("span", { className: `pill ${severity || ""}`.trim() }, label);
}

function btn(text, opts = {}) {
  const variant = opts.variant === "primary" ? "btn primary" : opts.variant === "ghost" ? "btn ghost" : "btn";
  if (opts.href) {
    return el("a", {
      className: variant,
      href: opts.href,
      target: opts.external ? "_blank" : null,
      rel: opts.external ? "noopener noreferrer" : null,
      onclick: opts.onclick,
    }, text);
  }
  return el("button", { className: variant, type: opts.type || "button", onclick: opts.onclick, disabled: !!opts.disabled }, text);
}

// ---------------------------------------------------------------------------
// API
// ---------------------------------------------------------------------------

async function apiFetch(path, init = {}) {
  const headers = new Headers(init.headers || {});
  const token = sessionStorage.getItem(SETUP_TOKEN_KEY);
  if (token) headers.set("X-ADOS-Setup-Token", token);
  if (init.body && !headers.has("Content-Type")) headers.set("Content-Type", "application/json");
  const res = await fetch(path, { ...init, headers });
  const ct = res.headers.get("content-type") || "";
  if (!res.ok) {
    let detail = `${res.status} ${res.statusText}`;
    try {
      if (ct.includes("application/json")) {
        const body = await res.json();
        if (body && typeof body.detail === "string") detail = body.detail;
      } else {
        const text = await res.text();
        if (text) detail = `${detail}: ${text.slice(0, 200)}`;
      }
    } catch { /* ignore */ }
    const err = new Error(detail);
    err.status = res.status;
    throw err;
  }
  if (ct.includes("application/json")) return res.json();
  return res.text();
}

async function tryFetch(path, init) {
  try {
    return await apiFetch(path, init);
  } catch {
    return null;
  }
}

async function loadStatus() {
  try {
    const data = await apiFetch("/api/v1/setup/status");
    currentStatus = data;
    subscribers.forEach((fn) => {
      try { fn(data); } catch (e) { console.error(e); }
    });
  } catch (err) {
    console.error("setup status load failed:", err);
  }
}

function startPolling() {
  if (pollTimer) return;
  loadStatus();
  pollTimer = setInterval(() => {
    if (!document.hidden) loadStatus();
  }, POLL_INTERVAL_MS);
  document.addEventListener("visibilitychange", () => {
    if (!document.hidden) loadStatus();
  });
}

function subscribe(fn) {
  subscribers.add(fn);
  if (currentStatus) {
    try { fn(currentStatus); } catch (e) { console.error(e); }
  }
  return () => subscribers.delete(fn);
}

// ---------------------------------------------------------------------------
// Status helpers
// ---------------------------------------------------------------------------

function severityForState(state) {
  if (state === "complete" || state === "running" || state === "ok") return "ok";
  if (state === "needs_action" || state === "stopped" || state === "configured" || state === "warn" || state === "starting") return "warn";
  if (state === "blocked" || state === "error" || state === "danger" || state === "circuit_open") return "err";
  return "muted";
}

function severityForMavlink(s) { return s?.connected ? "ok" : "warn"; }
function severityForVideo(s) {
  const state = s?.video?.state;
  if (state === "running") return "ok";
  if (state === "not_initialized") return "muted";
  return "warn";
}
function severityForNetwork(s) {
  const n = s?.network;
  return (n?.local_ips?.length || n?.hotspot_enabled) ? "ok" : "warn";
}
function severityForRemote(s) { return severityForState(s?.remote_access?.status); }
function severityForGround(s) { return s?.profile === "ground_station" ? "ok" : "muted"; }

function pageSeverity(pageId, status) {
  if (!status) return "muted";
  switch (pageId) {
    case "dashboard": return "ok";
    case "setup": return status.setup_complete ? "ok" : "warn";
    case "mavlink": return severityForMavlink(status.mavlink);
    case "video": return severityForVideo(status);
    case "network": return severityForNetwork(status);
    case "remote": return severityForRemote(status);
    case "ground": return severityForGround(status);
    case "system": {
      const services = status.services || [];
      if (!services.length) return "muted";
      const errored = services.some((svc) => svc.state && severityForState(svc.state) === "err");
      if (errored) return "err";
      const allOk = services.every((svc) => severityForState(svc.state) === "ok" || severityForState(svc.state) === "muted");
      return allOk ? "ok" : "warn";
    }
    case "advanced": return "muted";
    default: return "muted";
  }
}

function pretty(value) {
  if (value == null) return "—";
  if (value === "") return "—";
  if (typeof value === "boolean") return value ? "Yes" : "No";
  if (typeof value === "number") {
    if (!Number.isFinite(value)) return "—";
    return Math.abs(value) >= 100 ? value.toFixed(0) : value.toFixed(2);
  }
  if (typeof value === "object") return JSON.stringify(value);
  return String(value).replace(/_/g, " ");
}

function findUrl(status, predicate) {
  return (status?.access_urls || []).find(predicate) || null;
}

// ---------------------------------------------------------------------------
// Shell
// ---------------------------------------------------------------------------

function renderShell(activePage, content) {
  const root = document.getElementById("app");
  if (!root) return;

  const status = currentStatus;
  const completion = status ? `${status.completion_percent || 0}%` : "—";
  const navItems = NAV.map((item) => {
    const sev = pageSeverity(item.id, status);
    return el(
      "a",
      {
        className: "nav-link" + (item.id === activePage ? " active" : ""),
        href: item.href,
      },
      el("span", { className: "label" }, item.label),
      dot(sev),
    );
  });

  root.replaceChildren(
    el("div", { className: "app-shell" },
      el("aside", { className: "sidebar", "aria-label": "Setup navigation" },
        el("div", { className: "sidebar-brand" },
          el("div", { className: "mark" }, el("img", { src: "/brand.svg", alt: "" })),
          el("div", { className: "title" },
            el("strong", {}, "ADOS Setup"),
            el("span", {}, status?.device_name || "Drone Agent"),
          ),
        ),
        el("div", { className: "sidebar-section-label" }, "Onboarding"),
        el("nav", { className: "sidebar-nav" }, ...navItems),
        el("div", { className: "sidebar-footer" },
          el("div", { className: "meta" },
            el("span", {}, "Version"),
            el("span", {}, status?.version ? `v${status.version}` : "—"),
          ),
          el("div", { className: "meta" },
            el("span", {}, "Profile"),
            el("span", {}, status ? pretty(status.profile) : "—"),
          ),
          el("div", { className: "meta" },
            el("span", {}, "Setup"),
            el("span", {}, completion),
          ),
        ),
      ),
      el("div", { className: "mobile-bar" },
        el("button", {
          className: "menu",
          type: "button",
          "aria-label": "Toggle menu",
          onclick: () => document.body.classList.toggle("menu-open"),
        }, "≡"),
        el("span", { className: "title" }, "ADOS Setup"),
        el("span", { className: "completion" }, completion),
      ),
      el("div", {
        className: "sidebar-backdrop",
        onclick: () => document.body.classList.remove("menu-open"),
      }),
      el("main", { className: "content" }, content),
    ),
  );
}

function pageHeader({ eyebrow, title, subtitle, actions }) {
  return el("header", { className: "page-header" },
    el("div", { className: "titles" },
      eyebrow ? el("div", { className: "eyebrow" }, eyebrow) : null,
      el("h1", {}, title),
      subtitle ? el("div", { className: "subtitle" }, subtitle) : null,
    ),
    actions ? el("div", { className: "page-actions" }, ...actions) : null,
  );
}

function card({ title, subtitle, severity, actions, body, callout }) {
  const head = (title || subtitle || actions)
    ? el("div", { className: "card-head" },
        el("div", {},
          el("h2", {},
            severity ? dot(severity) : null,
            el("span", {}, title || ""),
          ),
          subtitle ? el("div", { className: "sub" }, subtitle) : null,
        ),
        actions ? el("div", { className: "btn-row" }, ...actions) : null,
      )
    : null;
  return el("section", { className: "card" + (callout ? " card-callout" : "") },
    head,
    body,
  );
}

function statTile({ label, value, hint, dotSeverity, href }) {
  const tile = el(
    href ? "a" : "div",
    {
      className: "stat-tile" + (href ? " linked" : ""),
      href: href || null,
    },
    el("div", { className: "label" },
      dotSeverity ? dot(dotSeverity) : null,
      el("span", {}, label),
    ),
    el("div", { className: "value" }, value),
    hint ? el("div", { className: "hint" }, hint) : null,
  );
  return tile;
}

function dlRow(label, value) {
  return el("div", { className: "dl-row" },
    el("dt", {}, label),
    el("dd", {}, value == null || value === "" ? "—" : String(value)),
  );
}

function copyButton(value) {
  const button = btn("Copy", {
    onclick: async () => {
      try {
        await navigator.clipboard.writeText(value);
        button.textContent = "Copied";
        setTimeout(() => { button.textContent = "Copy"; }, 1200);
      } catch {
        button.textContent = "Failed";
        setTimeout(() => { button.textContent = "Copy"; }, 1500);
      }
    },
  });
  return button;
}

function urlRow(label, url, hint) {
  return el("div", { className: "list-row" },
    el("div", { className: "label-block" },
      el("span", { className: "primary-text" }, label),
      el("span", { className: "url" }, url),
      hint ? el("span", { className: "secondary-text" }, hint) : null,
    ),
    el("div", { className: "actions" },
      copyButton(url),
      btn("Open", { href: url, external: true, variant: "primary" }),
    ),
  );
}

// ---------------------------------------------------------------------------
// Page: dashboard
// ---------------------------------------------------------------------------

function renderDashboard() {
  let logsCardBody = null;
  let logsLoaded = false;

  subscribe(async (status) => {
    const services = status.services || [];
    const accessLinks = (status.access_urls || []).filter(
      (u) => u.kind === "setup" || u.kind === "mission_control",
    );
    const primarySetup = findUrl(status, (u) => u.kind === "setup" && u.primary);

    const content = [
      pageHeader({
        eyebrow: "Dashboard",
        title: status.device_name || "Drone Agent",
        subtitle: `Profile ${pretty(status.profile)}. Version ${status.version || "?"}.`,
        actions: [
          primarySetup ? btn("Share setup link", { onclick: () => navigator.clipboard?.writeText(primarySetup.url) }) : null,
        ].filter(Boolean),
      }),
    ];

    // Priority banner
    if (!status.setup_complete) {
      const nextStep = (status.steps || []).find((s) => s.state === "needs_action");
      content.push(card({
        callout: true,
        body: el("div", { className: "card-pad" },
          el("div", { className: "list-row", style: { padding: 0, borderBottom: 0 } },
            el("div", { className: "label-block" },
              el("span", { className: "secondary-text" }, "Next action"),
              el("span", { className: "primary-text" }, status.next_action || "Continue setup"),
            ),
            el("div", { className: "actions" },
              el("div", { className: "progress" },
                el("div", { className: "progress-meta" },
                  el("span", {}, "Setup progress"),
                  el("strong", {}, `${status.completion_percent || 0}%`),
                ),
                el("div", { className: "progress-track" },
                  el("div", { className: "progress-fill", style: { width: `${status.completion_percent || 0}%` } }),
                ),
              ),
              nextStep?.href ? btn(nextStep.action_label || "Continue", { href: nextStep.href, variant: "primary" }) : null,
            ),
          ),
        ),
      }));
    }

    // Status grid
    content.push(el("div", { className: "grid cols-4" },
      statTile({
        label: "MAVLink",
        value: status.mavlink?.connected ? "Connected" : "Idle",
        hint: status.mavlink?.connected
          ? `${status.mavlink.port || "?"} @ ${status.mavlink.baud || "?"}`
          : "No flight controller",
        dotSeverity: severityForMavlink(status.mavlink),
        href: "/mavlink.html",
      }),
      statTile({
        label: "Video",
        value: pretty(status.video?.state),
        hint: status.video?.recording ? "Recording" : status.video?.whep_url ? "WHEP available" : "No source",
        dotSeverity: severityForVideo(status),
        href: "/video.html",
      }),
      statTile({
        label: "Network",
        value: status.network?.hotspot_enabled
          ? "Hotspot up"
          : (status.network?.local_ips || []).length
            ? `${(status.network?.local_ips || []).length} interface(s)`
            : "No network",
        hint: status.network?.mdns_host || "—",
        dotSeverity: severityForNetwork(status),
        href: "/network.html",
      }),
      statTile({
        label: "Remote access",
        value: pretty(status.remote_access?.status),
        hint: status.remote_access?.public_urls?.length
          ? `${status.remote_access.public_urls.length} URL(s)`
          : "Optional",
        dotSeverity: severityForRemote(status),
        href: "/remote.html",
      }),
    ));

    // Access links
    if (accessLinks.length) {
      content.push(card({
        title: "Open setup",
        subtitle: "Use whichever address is reachable from your phone or laptop.",
        body: el("div", { className: "list" },
          ...accessLinks.map((u) => urlRow(u.label, u.url, u.source)),
        ),
      }));
    }

    // Two-up MAVLink + Video summary
    content.push(el("div", { className: "grid cols-2" },
      card({
        title: "MAVLink",
        severity: severityForMavlink(status.mavlink),
        actions: [btn("Open", { href: "/mavlink.html" })],
        body: el("div", { className: "dl-rows" },
          dlRow("Port", status.mavlink?.port),
          dlRow("Baud", status.mavlink?.baud),
          dlRow("WebSocket", status.mavlink?.websocket_url),
          status.mavlink?.public_websocket_url
            ? dlRow("Tunnel", status.mavlink.public_websocket_url)
            : null,
        ),
      }),
      card({
        title: "Video",
        severity: severityForVideo(status),
        actions: [btn("Open", { href: "/video.html" })],
        body: el("div", { className: "dl-rows" },
          dlRow("State", pretty(status.video?.state)),
          dlRow("WHEP URL", status.video?.whep_url),
          dlRow("Recording", status.video?.recording ? "On" : "Off"),
          status.video?.public_whep_url
            ? dlRow("Tunnel", status.video.public_whep_url)
            : null,
        ),
      }),
    ));

    // Service summary
    if (services.length) {
      const running = services.filter((s) => severityForState(s.state) === "ok").length;
      const errored = services.filter((s) => severityForState(s.state) === "err").length;
      content.push(card({
        title: "Services",
        severity: errored > 0 ? "err" : running === services.length ? "ok" : "warn",
        subtitle: `${running} of ${services.length} running${errored ? `, ${errored} errored` : ""}`,
        actions: [btn("View all", { href: "/system.html" })],
        body: el("div", { className: "list" },
          ...services.slice(0, 6).map((svc) => el("div", { className: "list-row" },
            el("div", { className: "label-block" },
              el("span", { className: "primary-text" }, svc.name),
              el("span", { className: "secondary-text" }, pretty(svc.state)),
            ),
            el("div", { className: "actions" },
              pill(severityForState(svc.state), pretty(svc.state)),
            ),
          )),
        ),
      }));
    }

    // Recent logs (lazy fetched once)
    if (!logsLoaded) {
      logsLoaded = true;
      const logsResp = await tryFetch("/api/logs?limit=8");
      const lines = Array.isArray(logsResp?.entries) ? logsResp.entries : Array.isArray(logsResp) ? logsResp : [];
      if (lines.length) {
        logsCardBody = el("div", { className: "log" },
          ...lines.slice(-8).map((line) => {
            const text = typeof line === "string" ? line : line?.message || JSON.stringify(line);
            const lvl = (line?.level || "").toLowerCase();
            const cls = lvl === "warning" || lvl === "warn" ? "warn" : lvl === "error" || lvl === "critical" ? "err" : "";
            return el("span", { className: `log-line ${cls}`.trim() }, text);
          }),
        );
      }
    }
    if (logsCardBody) {
      content.push(card({
        title: "Recent activity",
        subtitle: "Last 8 log entries from the agent.",
        actions: [btn("Open System", { href: "/system.html" })],
        body: logsCardBody,
      }));
    }

    renderShell("dashboard", content);
  });
}

// ---------------------------------------------------------------------------
// Page: setup (wizard or revisit chrome)
// ---------------------------------------------------------------------------

function setupModeFor(status) {
  // The gate guarantees wizard mode whenever setup_finalized is false,
  // regardless of the URL. Once finalized, honor an explicit ?mode=
  // override and otherwise default to revisit.
  if (!status?.setup_finalized) return "wizard";
  const params = new URLSearchParams(window.location.search);
  const explicit = params.get("mode");
  if (explicit === "wizard" || explicit === "revisit") return explicit;
  return "revisit";
}

function setupStepFor(status, mode) {
  const params = new URLSearchParams(window.location.search);
  const requested = params.get("step");
  const steps = status?.steps || [];
  if (requested && steps.some((s) => s.id === requested)) return requested;
  if (mode === "wizard") {
    const firstIncomplete = steps.find((s) => s.state === "needs_action");
    if (firstIncomplete) return firstIncomplete.id;
    return steps[0]?.id || "welcome";
  }
  return null; // revisit chrome shows the full list
}

// Tracks the last rendered wizard signature so the 5s status poll does
// not tear down stable in-step DOM (chiefly the WebRTC iframe in the
// video step). Re-render only when the operator-visible state changes.
let _lastWizardSignature = null;

function _wizardSignature(status, stepId) {
  // Anything that changes the meaningful contents of the step body, the
  // stepper dots, or the footer state. NOT polled-but-incidental fields
  // like RSSI or counters.
  return JSON.stringify({
    step: stepId,
    setup_complete: !!status.setup_complete,
    setup_finalized: !!status.setup_finalized,
    profile: status.profile || "",
    ground_role: status.ground_role || "",
    skipped: (status.skipped_steps || []).slice().sort(),
    states: (status.steps || []).map((s) => `${s.id}:${s.state}`),
    video_state: status.video?.state || "",
    cloud_mode: status.cloud_choice?.mode || "",
    cloud_paired: !!status.cloud_choice?.paired,
    mavlink_connected: !!status.mavlink?.connected,
    remote_status: status.remote_access?.status || "",
  });
}

function renderSetup() {
  subscribe((status) => {
    const mode = setupModeFor(status);
    if (mode === "wizard") {
      const stepId = setupStepFor(status, mode);
      const sig = _wizardSignature(status, stepId);
      if (sig === _lastWizardSignature) return;
      _lastWizardSignature = sig;
      renderWizard(status, stepId);
    } else {
      _lastWizardSignature = null;
      renderRevisit(status);
    }
  });
}

function renderRevisit(status) {
  const steps = status.steps || [];
  const next = steps.find((s) => s.state === "needs_action");

  const rerunSetup = async () => {
    try {
      await apiFetch("/api/v1/setup/reset", { method: "POST" });
    } catch (err) {
      console.error("reset failed:", err);
    }
    window.location.assign("/setup.html?mode=wizard");
  };

  const content = [
    pageHeader({
      eyebrow: "Setup",
      title: "Setup checklist",
      subtitle: status.setup_complete
        ? "All required steps are complete. Re-run any step from the list below."
        : "Walk through the remaining steps to bring this device online.",
      actions: [
        btn("Re-run setup", {
          onclick: rerunSetup,
          variant: status.setup_complete ? "primary" : null,
        }),
      ],
    }),
    card({
      callout: true,
      body: el("div", { className: "card-pad" },
        el("div", { className: "list-row", style: { padding: 0, borderBottom: 0 } },
          el("div", { className: "label-block" },
            el("span", { className: "secondary-text" }, status.setup_complete ? "Status" : "Next action"),
            el("span", { className: "primary-text" }, status.next_action || "—"),
          ),
          el("div", { className: "actions" },
            el("div", { className: "progress" },
              el("div", { className: "progress-meta" },
                el("span", {}, "Progress"),
                el("strong", {}, `${status.completion_percent || 0}%`),
              ),
              el("div", { className: "progress-track" },
                el("div", { className: "progress-fill", style: { width: `${status.completion_percent || 0}%` } }),
              ),
            ),
            next?.href ? btn(next.action_label || "Continue", { href: next.href, variant: "primary" }) : null,
          ),
        ),
      ),
    }),
    card({
      title: "Steps",
      body: el("div", { className: "list" },
        ...(steps.length
          ? steps.map((s) => el("a", { className: "step-row", href: s.href || "#" },
              dot(severityForState(s.state)),
              el("div", { className: "body" },
                el("strong", {}, s.label || s.id),
                s.detail ? el("p", {}, s.detail) : null,
              ),
              pill(severityForState(s.state), pretty(s.state)),
            ))
          : [el("div", { className: "empty" }, "No setup steps reported.")]
        ),
      ),
    }),
  ];

  renderShell("setup", content);
}

function renderWizardShell(content) {
  // Sidebar-less full-bleed shell used while setup_finalized is false.
  // Once finalized the sidebar comes back via renderShell.
  const root = document.getElementById("app");
  if (!root) return;
  root.replaceChildren(
    el("div", { className: "wizard-page" },
      el("div", { className: "wizard-page-inner" }, ...content),
    ),
  );
}

// Steps that need to run an async preflight (save form, validate input,
// hit a POST endpoint) before the wizard advances register a callback
// here. Continue awaits it; truthy result advances, falsy halts. Cleared
// on each wizard render so a stale hook from a prior step never fires.
let wizardBeforeNextHook = null;

// Per-step cleanup callbacks (timers, WebSocket disposers, AbortControllers).
// Steps push handlers in via wizardOnDispose(...). The wizard fires every
// queued handler on the next render so stale intervals do not leak when a
// status update re-renders the body.
let _wizardDisposers = [];
function wizardOnDispose(fn) {
  if (typeof fn === "function") _wizardDisposers.push(fn);
}
function _runWizardDisposers() {
  const all = _wizardDisposers;
  _wizardDisposers = [];
  for (const fn of all) {
    try { fn(); } catch (err) { console.error("wizard disposer failed:", err); }
  }
}

function renderWizard(status, currentStepId) {
  _runWizardDisposers();
  wizardBeforeNextHook = null;
  const steps = status.steps || [];
  const currentIdx = Math.max(0, steps.findIndex((s) => s.id === currentStepId));
  const currentStep = steps[currentIdx] || steps[0];
  if (!currentStep) {
    renderWizardShell([pageHeader({ eyebrow: "Setup", title: "No steps reported" })]);
    return;
  }
  const total = steps.length;
  const isFirst = currentIdx === 0;
  const isLast = currentIdx === total - 1;
  const finalized = !!status.setup_finalized;
  const isSkippable = !isLast && (
    currentStep.state === "optional"
    || currentStep.state === "not_applicable"
    || ["mavlink", "video", "remote_access", "ground_receiver", "hardware_check"].includes(currentStep.id)
  );
  // Pair has its own "Pair later" affordance inside the step body. Skipping
  // it from the wizard header would bypass the auto-flip-to-local helper
  // that keeps cloud_choice and pairing intent in sync.
  const skippedCount = (status.skipped_steps || []).length;

  const stepperDots = steps.map((s, idx) => {
    // Dot state is position-only: dots ahead of the current step always read
    // as "todo" even when their underlying step is auto-satisfied. Surfacing
    // auto-satisfaction inside the future step's body is clearer than lighting
    // a dot the user has not yet visited.
    const cls = idx === currentIdx ? "current" : idx < currentIdx ? "done" : "todo";
    return el("span", { className: `wizard-pip ${cls}`, "aria-label": `Step ${idx + 1}` });
  });

  const goTo = (id) => {
    const params = new URLSearchParams(window.location.search);
    params.set("mode", "wizard");
    if (id) params.set("step", id); else params.delete("step");
    window.location.assign(`/setup.html?${params.toString()}`);
  };

  const skipCurrent = async () => {
    try {
      await apiFetch(`/api/v1/setup/step/${encodeURIComponent(currentStep.id)}/skip`, {
        method: "POST",
      });
    } catch (err) {
      console.error("skip failed:", err);
    }
    const nextStep = steps[currentIdx + 1];
    if (nextStep) goTo(nextStep.id);
  };

  const finishWizard = async () => {
    try {
      await apiFetch("/api/v1/setup/finish", { method: "POST" });
    } catch (err) {
      console.error("finish failed:", err);
    }
    // Land on the dashboard now that the gate is open.
    window.location.assign("/");
  };

  const finishLabel = isLast && skippedCount > 0
    ? `Finish anyway (${skippedCount} skipped)`
    : isLast
      ? "Finish setup"
      : "Continue";

  const stepBody = renderWizardStepBody(currentStep, status, () => loadStatus());

  const content = [
    el("header", { className: "wizard-header" },
      el("div", { className: "wizard-brand" },
        el("img", { src: "/brand.svg", alt: "" }),
        el("div", { className: "wizard-brand-titles" },
          el("strong", {}, "ADOS Setup"),
          el("span", {}, status.device_name || "Drone Agent"),
        ),
      ),
      el("div", { className: "wizard-stepper" },
        el("span", { className: "wizard-step-count" }, `Step ${currentIdx + 1} of ${total}`),
        el("div", { className: "wizard-pips" }, ...stepperDots),
      ),
      el("div", { className: "wizard-header-actions" },
        isSkippable
          ? btn("Skip for now", { variant: "ghost", onclick: skipCurrent })
          : null,
        // Exit only available once the operator finalized at least once;
        // first-boot users have no escape hatch from the wizard.
        finalized
          ? btn("Exit", { variant: "ghost", onclick: () => window.location.assign("/setup.html?mode=revisit") })
          : null,
      ),
    ),
    el("div", { className: "wizard-body" },
      pageHeader({
        eyebrow: "Setup wizard",
        title: currentStep.label || currentStep.id,
        subtitle: currentStep.detail || "",
      }),
      stepBody,
    ),
    el("footer", { className: "wizard-footer" },
      btn("Back", {
        variant: "ghost",
        disabled: isFirst,
        onclick: () => {
          if (!isFirst) goTo(steps[currentIdx - 1].id);
        },
      }),
      (() => {
        // Double-click guard: while a step's beforeNext hook is in flight,
        // disable the button + swap the label so the operator sees state.
        // Without this, two quick clicks fire the hook twice and produce
        // ghost saves on cloud-choice and profile.
        const continueBtn = btn(finishLabel, {
          variant: "primary",
          onclick: async () => {
            if (continueBtn.dataset.busy === "true") return;
            continueBtn.dataset.busy = "true";
            const restoreLabel = continueBtn.textContent;
            try {
              continueBtn.disabled = true;
              continueBtn.textContent = isLast ? "Finishing…" : "Saving…";
              if (isLast) {
                await finishWizard();
                return;
              }
              if (wizardBeforeNextHook) {
                const ok = await wizardBeforeNextHook();
                if (!ok) {
                  continueBtn.disabled = false;
                  continueBtn.textContent = restoreLabel;
                  continueBtn.dataset.busy = "false";
                  return;
                }
              }
              goTo(steps[currentIdx + 1].id);
            } catch (err) {
              continueBtn.disabled = false;
              continueBtn.textContent = restoreLabel;
              continueBtn.dataset.busy = "false";
              throw err;
            }
          },
        });
        return continueBtn;
      })(),
    ),
  ];

  renderWizardShell(content);
}

function renderWizardStepBody(step, status, onMutate) {
  switch (step.id) {
    case "welcome":
      return renderWelcomeStep(status);
    case "profile":
      return renderProfileStep(status, onMutate);
    case "cloud_choice":
      return renderCloudChoiceStep(status, onMutate);
    case "hardware_check":
      return renderHardwareCheckStep(status, onMutate);
    case "mavlink":
      return renderMavlinkStepInline(status);
    case "video":
      return renderVideoStepInline(status);
    case "ground_receiver":
      return renderGroundStepInline(status);
    case "remote_access":
      return renderRemoteStepInline(status);
    case "pair":
      return renderPairStep(status, onMutate);
    case "finish":
      return renderFinishStep(status);
    default:
      return renderGenericStep(step, status);
  }
}

function renderWelcomeStep(status) {
  const isGround = status.profile === "ground_station";
  const network = status.network || {};
  const localIps = network.local_ips || [];
  const networkOk = localIps.length > 0 || network.hotspot_enabled;
  const hostname = network.hostname || status.device_name || "this device";
  const mdns = network.mdns_host || "";
  const hotspotLabel = network.hotspot_enabled
    ? `hotspot · ${network.hotspot_ssid || "active"}`
    : "hotspot off";

  const networkChips = [
    chip({ variant: "muted", label: hostname, icon: "host" }),
    mdns ? chip({ variant: "muted", label: mdns }) : null,
    chip({ variant: network.hotspot_enabled ? "ok" : "muted", dot: true, label: hotspotLabel }),
    ...(localIps.length
      ? localIps.slice(0, 3).map((ip) => chip({ variant: "ok", dot: true, label: ip }))
      : [chip({ variant: "warn", dot: true, label: "no LAN" })]),
  ].filter(Boolean);

  const identityChips = [
    chip({ variant: "info", label: pretty(status.profile) || (isGround ? "ground" : "drone") }),
    chip({ variant: "muted", label: `v${status.version || "?"}` }),
    chip({ variant: "muted", label: `id ${(status.device_id || "—").slice(0, 8)}` }),
  ];

  return el("div", { className: "page-body" },
    card({
      title: "What this wizard does",
      severity: networkOk ? "ok" : "warn",
      body: el("div", { className: "card-pad" },
        el("p", { style: { color: "var(--text-secondary)", fontSize: "13.5px", marginBottom: "10px", lineHeight: "1.55" } },
          `This is the one-time setup for ${status.device_name || "this device"}, an ADOS ` +
          `${isGround ? "ground station" : "drone"} agent. Each step picks a piece of the runtime: which role the device plays, what hardware it has, where it talks to Mission Control, and how it streams video and telemetry.`),
        el("p", { style: { color: "var(--text-secondary)", fontSize: "13.5px", marginBottom: "10px", lineHeight: "1.55" } },
          "Setup is local-first. MAVLink and video work over LAN, hotspot, or USB tether before any cloud step is required. Every choice you make here can be changed later from Settings."),
        !networkOk
          ? el("p", { style: { color: "var(--status-warning)", fontSize: "12.5px", marginTop: "8px", marginBottom: "0" } },
              "No usable network detected yet. Bring up Wi-Fi, plug in a USB tether, or join a LAN before continuing.")
          : null,
      ),
    }),
    card({
      title: "This device",
      body: el("div", {},
        liveRow({ label: "Identity", chips: identityChips }),
        liveRow({ label: "Network", chips: networkChips, hint: networkOk ? null : "Mission Control reaches the wizard at any of the IPs above." }),
      ),
    }),
  );
}

function renderCloudChoiceStep(status, onMutate) {
  const current = status.cloud_choice?.mode || "cloud";
  let selected = current;

  // Self-hosted form fields. Held outside buildForm so the values persist
  // across mode changes and re-renders.
  const fields = {
    url: status.cloud_choice?.backend_url || "",
    mqtt_broker: "",
    mqtt_port: "8883",
    api_key: "",
  };

  const errorChip = el("div", { style: { minHeight: "0" } });
  const clearError = () => errorChip.replaceChildren();
  const setError = (label) => {
    errorChip.replaceChildren(chip({ variant: "err", dot: true, label }));
  };

  const buildSelfHostedForm = () => {
    const wrap = el("div", { className: "wizard-form" });
    const urlInput = el("input", { type: "url", name: "url", placeholder: "https://convex.your-domain.com", autocomplete: "off", value: fields.url });
    const brokerInput = el("input", { type: "text", name: "mqtt_broker", placeholder: "mqtt.your-domain.com", autocomplete: "off", value: fields.mqtt_broker });
    const portInput = el("input", { type: "number", name: "mqtt_port", min: "1", max: "65535", value: fields.mqtt_port });
    const apiKeyInput = el("input", { type: "password", name: "api_key", placeholder: "Optional. Stored 0600 on device.", autocomplete: "off" });

    urlInput.addEventListener("input", () => { fields.url = urlInput.value; clearError(); });
    brokerInput.addEventListener("input", () => { fields.mqtt_broker = brokerInput.value; });
    portInput.addEventListener("input", () => { fields.mqtt_port = portInput.value; });
    apiKeyInput.addEventListener("input", () => { fields.api_key = apiKeyInput.value; });

    wrap.append(
      el("label", {}, el("span", {}, "Deployment URL"), urlInput),
      el("label", {}, el("span", {}, "MQTT broker"), brokerInput),
      el("label", {}, el("span", {}, "MQTT port"), portInput),
      el("label", {}, el("span", {}, "API key (optional)"), apiKeyInput),
      el("div", { style: { padding: "0 0 4px 0" } },
        chip({ variant: "info", label: "API key is written to a root-owned file and never echoed back" })),
    );
    return wrap;
  };

  const formContainer = el("div", {});
  const updateForm = () => {
    formContainer.replaceChildren(selected === "self_hosted" ? buildSelfHostedForm() : el("div"));
  };
  updateForm();

  const renderRadio = (mode, title, blurb, accentChip) => {
    const isSelected = selected === mode;
    return el("label", { className: `cloud-card ${isSelected ? "selected" : ""}`.trim() },
      el("input", {
        type: "radio",
        name: "cloud_mode",
        value: mode,
        checked: isSelected,
        onchange: () => {
          selected = mode;
          clearError();
          updateForm();
          renderCardClasses();
        },
      }),
      el("div", { className: "cloud-card-body" },
        el("strong", { style: { display: "inline-flex", alignItems: "center", gap: "8px" } }, title, accentChip || null),
        el("p", {}, blurb),
      ),
    );
  };

  const cards = el("div", { className: "cloud-cards" });
  const renderCardClasses = () => {
    Array.from(cards.children).forEach((node) => {
      const radio = node.querySelector("input[type=radio]");
      node.classList.toggle("selected", radio?.checked);
    });
  };
  cards.append(
    renderRadio(
      "cloud",
      "Altnautica cloud",
      "Connects this device to Altnautica's managed backend. Mission Control sees your fleet from anywhere. The next step generates a code so you can pair it with a Mission Control account.",
      chip({ variant: "info", label: "dev preview", size: "sm" }),
    ),
    renderRadio(
      "self_hosted",
      "Self-hosted backend",
      "Point this device at your own Convex deployment and MQTT broker. The pair step still uses a 6-character code, but it goes through your deployment instead of the Altnautica backend.",
      null,
    ),
    renderRadio(
      "local",
      "Local only",
      "No cloud relay. Mission Control reaches this device directly over LAN, hotspot, or USB tether. The pair step is hidden in this mode. You can still enable Cloudflare Tunnel later for remote access.",
      chip({ variant: "muted", label: "no cloud", size: "sm" }),
    ),
  );

  // Single beforeNext hook drives both validation and the save POST so the
  // wizard's Continue button is the one and only commit affordance on this
  // step. No second "Save cloud posture" button.
  wizardBeforeNextHook = async () => {
    clearError();
    if (!selected) {
      setError("Pick a cloud posture before continuing.");
      return false;
    }
    const body = { mode: selected };
    if (selected === "self_hosted") {
      const url = (fields.url || "").trim();
      if (!url) {
        setError("Self-hosted mode needs a deployment URL.");
        return false;
      }
      try { new URL(url); } catch {
        setError("Deployment URL must include https:// and a hostname.");
        return false;
      }
      body.self_hosted = {
        url,
        mqtt_broker: (fields.mqtt_broker || "").trim(),
        mqtt_port: parseInt(fields.mqtt_port || "8883", 10),
        api_key: fields.api_key || "",
      };
    }
    try {
      const res = await apiFetch("/api/v1/setup/cloud-choice", {
        method: "POST",
        body: JSON.stringify(body),
      });
      // Wipe the typed key out of memory once the agent has it on disk.
      fields.api_key = "";
      if (res?.ok === false) {
        setError(res.message || "Could not save the choice.");
        return false;
      }
      await onMutate();
      return true;
    } catch (err) {
      setError(`Could not save: ${err.message || err}`);
      return false;
    }
  };

  return el("div", { className: "page-body" },
    card({
      title: "Where does this device send telemetry?",
      subtitle: "Pick one. Continue saves the choice.",
      body: el("div", { className: "card-pad" }, cards),
    }),
    selected === "self_hosted" ? card({
      title: "Deployment endpoints",
      body: formContainer,
    }) : null,
    card({
      body: el("div", { className: "card-pad", style: { display: "flex", flexDirection: "column", gap: "8px" } },
        el("div", { style: { display: "flex", gap: "8px", flexWrap: "wrap" } },
          chip({ variant: "info", icon: "i", label: "All settings can be changed later from Settings → Cloud" }),
        ),
        errorChip,
      ),
    }),
  );
}

function renderMavlinkStepInline(status) {
  const m = status.mavlink || {};
  const portChip = chip({ variant: "muted", label: `port ${m.port || "—"}`, size: "sm" });
  const baudChip = chip({ variant: "muted", label: `${m.baud || "—"} baud`, size: "sm" });
  const linkChip = chip({
    variant: m.connected ? "ok" : "warn",
    dot: true,
    pulse: m.connected,
    label: m.connected ? "Connected" : "FC not detected",
  });

  // Live chip row. Updated by the MAVLink WebSocket subscriber below.
  const slots = {
    heartbeat: el("span", {}, chip({ variant: "muted", label: "heartbeat —", size: "sm" })),
    mode: el("span", {}, chip({ variant: "muted", label: "mode —", size: "sm" })),
    armed: el("span", {}, chip({ variant: "muted", label: "armed —", size: "sm" })),
    gps: el("span", {}, chip({ variant: "muted", label: "GPS —", size: "sm" })),
    sats: el("span", {}, chip({ variant: "muted", label: "sats —", size: "sm" })),
    battery: el("span", {}, chip({ variant: "muted", label: "battery —", size: "sm" })),
    attitude: el("span", {}, chip({ variant: "muted", label: "attitude —", size: "sm" })),
  };
  const replace = (slot, opts) => slot.replaceChildren(chip({ size: "sm", ...opts }));

  // Raw frame console reuses the same WebSocket. The streamConsole helper
  // does ANSI strip + autoscroll + reconnect; we provide a parser hook
  // that turns binary MAVLink frames into one-line summaries.
  const wsUrl = m.websocket_url || `ws://${location.hostname}:8765`;
  let lastHbAt = 0;
  let hbWindow = [];
  const observed = (msgId) => {
    const names = { 0: "HEARTBEAT", 1: "SYS_STATUS", 24: "GPS_RAW_INT", 30: "ATTITUDE", 148: "AUTOPILOT_VERSION" };
    return names[msgId] || `MSG ${msgId}`;
  };

  const parser = (data) => {
    let bytes;
    if (data instanceof ArrayBuffer) bytes = new Uint8Array(data);
    else if (data instanceof Uint8Array) bytes = data;
    else if (typeof data === "string") return data;
    else return null;
    const frame = parseMavlinkFrame(bytes);
    if (!frame) return null;
    const decoded = decodeMavlinkPayload(frame);
    const ts = new Date().toISOString().split("T")[1].replace("Z", "");
    if (!decoded) return `${ts}  ${observed(frame.msgId)}`;
    if (decoded.type === "heartbeat") {
      const now = Date.now();
      hbWindow.push(now);
      hbWindow = hbWindow.filter((t) => now - t < 5000);
      const rate = hbWindow.length / 5;
      lastHbAt = now;
      replace(slots.heartbeat, { variant: rate > 0.5 ? "ok" : "warn", dot: true, label: `heartbeat ${rate.toFixed(1)} Hz` });
      replace(slots.mode, { variant: "info", label: `${decoded.autopilot} · ${decoded.vehicle}` });
      replace(slots.armed, { variant: decoded.armed ? "warn" : "ok", dot: true, label: decoded.armed ? "ARMED" : "disarmed" });
      return `${ts}  HEARTBEAT  ${decoded.autopilot}  ${decoded.armed ? "ARMED" : "disarmed"}`;
    }
    if (decoded.type === "gps") {
      const okFix = decoded.fix >= 3;
      replace(slots.gps, { variant: okFix ? "ok" : "warn", dot: true, label: `GPS ${decoded.fix_label}` });
      replace(slots.sats, { variant: decoded.sats >= 6 ? "ok" : "warn", label: `${decoded.sats} sats` });
      return `${ts}  GPS_RAW_INT  ${decoded.fix_label}  sats=${decoded.sats}`;
    }
    if (decoded.type === "sys_status") {
      const v = decoded.voltage_v;
      replace(slots.battery, {
        variant: v > 14 ? "ok" : v > 11 ? "warn" : "err",
        label: `${v.toFixed(1)} V${decoded.battery_remaining != null ? ` · ${decoded.battery_remaining}%` : ""}`,
      });
      return `${ts}  SYS_STATUS  ${v.toFixed(2)}V  ${decoded.current_a.toFixed(1)}A`;
    }
    if (decoded.type === "attitude") {
      const deg = (r) => (r * 180 / Math.PI).toFixed(0);
      replace(slots.attitude, { variant: "info", label: `r${deg(decoded.roll)}° p${deg(decoded.pitch)}° y${deg(decoded.yaw)}°` });
      return null; // attitude is too chatty for the console
    }
    if (decoded.type === "autopilot_version") {
      return `${ts}  AUTOPILOT_VERSION  ${decoded.supported.length} capabilities`;
    }
    return `${ts}  ${observed(frame.msgId)}`;
  };

  // Heartbeat staleness tick — if no HEARTBEAT for 6s, surface that.
  const stalenessTimer = setInterval(() => {
    if (!lastHbAt) return;
    if (Date.now() - lastHbAt > 6000) {
      replace(slots.heartbeat, { variant: "warn", dot: true, label: "heartbeat stale" });
    }
  }, 2000);

  // Short console: this is a "data is flowing" indicator, not a full
  // analyser. The standalone /mavlink page is for that.
  const console_ = streamConsole({ wsUrl, height: 120, parser });
  wizardOnDispose(() => { if (console_.dispose) console_.dispose(); });
  wizardOnDispose(() => clearInterval(stalenessTimer));

  return el("div", { className: "page-body" },
    card({
      title: "Flight controller",
      severity: severityForMavlink(m),
      body: el("div", {},
        el("div", { className: "card-pad", style: { display: "flex", flexDirection: "column", gap: "10px" } },
          el("p", { style: { color: "var(--text-secondary)", fontSize: "13px", margin: 0 } },
            m.connected
              ? "MAVLink is live. The chips below update from frames received over the FC link."
              : "No flight controller is talking yet. Power the FC, plug in the USB or UART cable, and the link will come up automatically."),
          el("div", { style: { display: "flex", flexWrap: "wrap", gap: "6px" } },
            linkChip, portChip, baudChip,
          ),
        ),
        el("div", {},
          liveRow({ label: "Heartbeat", chips: [slots.heartbeat, slots.mode, slots.armed] }),
          liveRow({ label: "GPS", chips: [slots.gps, slots.sats] }),
          liveRow({ label: "Battery", chips: [slots.battery] }),
          liveRow({ label: "Attitude", chips: [slots.attitude] }),
        ),
      ),
    }),
    card({
      title: "MAVLink stream",
      subtitle: "Live frames from the FC. Scroll up to pause auto-follow.",
      body: el("div", { className: "card-pad" },
        console_,
        el("div", { className: "btn-row", style: { marginTop: "10px" } },
          btn("Open MAVLink page", { href: "/mavlink.html" }),
        ),
      ),
    }),
  );
}

function renderVideoStepInline(status) {
  const v = status.video || {};
  const isRunning = v.state === "running" && !!v.whep_url;

  // mediamtx serves a built-in WebRTC test player at http://host:webrtc_port/<path>/.
  // Reuse that as an inline confirmation preview rather than writing a WHEP
  // client here. The whep_url shape is http://host:8889/main/whep, so the
  // viewer URL is the same minus the trailing /whep.
  const previewUrl = isRunning
    ? v.whep_url.replace(/\/whep$/, "/")
    : null;

  // Lazy fetch of /api/video so the wizard step can enumerate cameras
  // without bloating /api/v1/setup/status. Slot updates in place when
  // the request returns.
  const camerasSlot = el("div", { className: "dl-rows" });
  const renderCameraRows = (rows) => {
    if (!rows || rows.length === 0) {
      camerasSlot.replaceChildren(
        dlRow("Cameras", "None detected"),
      );
      return;
    }
    const items = rows.map((c) => {
      const dims = (c.width && c.height) ? `${c.width}x${c.height}` : "";
      const label = c.role && c.role !== "camera"
        ? `${c.name} (${c.role})`
        : c.name;
      const value = [c.type, c.device_path, dims].filter(Boolean).join(" · ");
      return dlRow(label || "Camera", value || "—");
    });
    camerasSlot.replaceChildren(...items);
  };
  renderCameraRows(null);
  camerasSlot.appendChild(el("div", { style: { fontSize: "11px", color: "var(--text-tertiary)", marginTop: "4px" } }, "Loading camera list…"));
  (async () => {
    try {
      const resp = await apiFetch("/api/video");
      const flat = [];
      const c = resp?.cameras;
      // /api/video returns either a flat array or {cameras: [...], assignments: {...}}
      if (Array.isArray(c)) {
        flat.push(...c);
      } else if (c && Array.isArray(c.cameras)) {
        flat.push(...c.cameras);
      }
      renderCameraRows(flat);
    } catch {
      camerasSlot.replaceChildren(dlRow("Cameras", "Could not query agent"));
    }
  })();

  // Start button + status — only shown when the pipeline is not running.
  // It triggers the same supervised restart the systemd unit already
  // performs on failure, so the operator has agency without leaving
  // the wizard.
  const startStatus = el("div", {
    style: { fontSize: "12px", color: "var(--text-tertiary)", marginTop: "8px", minHeight: "1.2em" },
  });
  const startBtn = btn("Start video", {
    variant: "primary",
    onclick: async () => {
      startStatus.textContent = "Starting…";
      try {
        await apiFetch("/api/services/ados-video/restart", { method: "POST" });
        // Poll setup status for up to 15s for the pipeline to come up.
        const deadline = Date.now() + 15000;
        while (Date.now() < deadline) {
          await new Promise((r) => setTimeout(r, 1000));
          await loadStatus();
          const cur = currentStatus?.video;
          if (cur?.state === "running") {
            startStatus.textContent = "Pipeline is running.";
            // Force a re-render to swap in the preview iframe. Reset
            // the signature gate so the subscribe path sees the change
            // and does not skip the same status as a no-op.
            _lastWizardSignature = null;
            renderWizard(currentStatus, "video");
            return;
          }
        }
        startStatus.textContent = "Did not reach running state within 15s. Check /api/video and journalctl -u ados-video.";
      } catch (err) {
        startStatus.textContent = `Restart failed: ${err.message || err}`;
      }
    },
  });

  return card({
    title: "Video pipeline",
    severity: severityForVideo(status),
    body: el("div", {},
      el("div", { className: "card-pad" },
        el("p", { style: { color: "var(--text-secondary)", fontSize: "13px", marginBottom: "12px" } },
          isRunning
            ? "Live preview below. The same WHEP stream is what Mission Control will receive."
            : "Pipeline is not streaming yet. Start it to begin pushing the camera through WebRTC."),
        isRunning && previewUrl
          ? el("div", {
              style: { marginBottom: "12px", border: "1px solid var(--border-default)", borderRadius: "4px", overflow: "hidden", background: "#000" },
            },
              el("iframe", {
                src: previewUrl,
                title: "Live WHEP preview",
                allow: "autoplay",
                style: { display: "block", width: "100%", aspectRatio: "16 / 9", border: "0" },
              }),
            )
          : null,
        el("div", { className: "btn-row" },
          isRunning ? null : startBtn,
          btn("Open Video", { href: "/video.html" }),
        ),
        isRunning ? null : startStatus,
      ),
      el("div", { className: "dl-rows" },
        dlRow("State", pretty(v.state)),
        dlRow("WHEP URL", v.whep_url),
        dlRow("Recording", v.recording ? "On" : "Off"),
      ),
      el("div", { className: "card-pad", style: { borderTop: "1px solid var(--border-default)", paddingTop: "12px" } },
        el("div", { style: { fontSize: "11px", color: "var(--text-tertiary)", marginBottom: "6px" } }, "DETECTED CAMERAS"),
        camerasSlot,
      ),
    ),
  });
}

function renderGroundStepInline(status) {
  return card({
    title: "Ground receiver",
    body: el("div", { className: "card-pad" },
      el("p", { style: { color: "var(--text-secondary)", fontSize: "13px", marginBottom: "12px" } },
        "Pairing, WFB receiver, uplink, and mesh role are exposed on the Ground station page and through the agent REST API."),
      el("div", { className: "btn-row" }, btn("Open Ground station", { href: "/ground.html" })),
    ),
  });
}

function renderRemoteStepInline(status) {
  const remote = status.remote_access || {};
  const cf = remote.cloudflare || {};
  const tokenInstalled = remote.configured;
  const running = remote.status === "running";
  const setupUrl = (cf.setup_url || "").trim();

  const tokenChip = chip({ variant: tokenInstalled ? "ok" : "muted", dot: true, label: tokenInstalled ? "token installed" : "no token" });
  const runChip = chip({ variant: running ? "ok" : "warn", dot: true, pulse: running, label: running ? "cloudflared running" : "cloudflared stopped" });
  const reachChipSlot = el("span", {}, chip({ variant: "muted", label: "reachability unchecked" }));

  // Token textarea + install button.
  const tokenInput = el("textarea", {
    placeholder: "Paste the connector token from your Cloudflare dashboard, or paste the full install command.",
    spellcheck: false,
    autocomplete: "off",
  });
  const installResult = el("div", { style: { minHeight: "20px" } });
  const installBtn = btn("Install token", {
    variant: "primary",
    onclick: async () => {
      const value = (tokenInput.value || "").trim();
      if (!value) {
        installResult.replaceChildren(chip({ variant: "warn", dot: true, label: "Paste a token first" }));
        return;
      }
      installResult.replaceChildren(chip({ variant: "info", dot: true, pulse: true, label: "Installing…" }));
      try {
        const res = await apiFetch("/api/v1/setup/remote-access/cloudflare", {
          method: "POST",
          body: JSON.stringify({ token_or_script: value }),
        });
        tokenInput.value = "";
        if (res?.ok === false) {
          installResult.replaceChildren(chip({ variant: "err", dot: true, label: res.message || "Install failed" }));
        } else {
          installResult.replaceChildren(chip({ variant: "ok", dot: true, label: res?.message || "Token installed. Restart cloudflared to connect." }));
        }
      } catch (err) {
        installResult.replaceChildren(chip({ variant: "err", dot: true, label: err.message || "Install failed" }));
      }
    },
  });

  // Verify reachability button — uses the new GET endpoint.
  const verify = verifyButton({
    label: "Verify reachability",
    busyLabel: "Probing tunnel…",
    successLabel: "Reachable",
    endpoint: "/api/v1/setup/cloudflare/verify",
    onResult: (body, ok) => {
      if (ok && body?.reachable) {
        reachChipSlot.replaceChildren(chip({ variant: "ok", dot: true, label: `reachable · ${body.latency_ms ?? "?"}ms` }));
      } else if (ok) {
        reachChipSlot.replaceChildren(chip({ variant: "warn", dot: true, label: body?.error || "unreachable" }));
      } else {
        reachChipSlot.replaceChildren(chip({ variant: "err", dot: true, label: body?.error || "verify failed" }));
      }
    },
  });

  // Live cloudflared journal log.
  const console_ = streamConsole({ wsUrl: "/api/v1/setup/cloudflare/logs", height: 220 });
  wizardOnDispose(() => { if (console_.dispose) console_.dispose(); });

  return el("div", { className: "page-body" },
    card({
      title: "Remote access (optional)",
      severity: severityForRemote(status),
      body: el("div", {},
        el("div", { className: "card-pad", style: { display: "flex", flexDirection: "column", gap: "10px" } },
          el("p", { style: { color: "var(--text-secondary)", fontSize: "13.5px", margin: 0 } },
            "Cloudflare Tunnel exposes this device to Mission Control without opening router ports. Optional. Skip the step if you only need LAN access."),
          el("div", { style: { display: "flex", gap: "6px", flexWrap: "wrap" } }, tokenChip, runChip, reachChipSlot),
          setupUrl
            ? liveRow({ label: "Public URL", chips: [chip({ variant: "info", label: setupUrl })] })
            : null,
        ),
      ),
    }),
    card({
      title: "Install or rotate the token",
      subtitle: "Create a tunnel in your Cloudflare dashboard, copy the connector token, paste it below.",
      body: el("div", { className: "card-pad", style: { display: "flex", flexDirection: "column", gap: "10px" } },
        tokenInput,
        el("div", { className: "btn-row" }, installBtn,
          btn("Cloudflare dashboard", { href: "https://one.dash.cloudflare.com/", external: true }),
        ),
        installResult,
      ),
    }),
    tokenInstalled ? card({
      title: "Verify reachability",
      subtitle: "Confirms the public URL routes back to this device.",
      body: el("div", { className: "card-pad", style: { display: "flex", flexDirection: "column", gap: "10px" } },
        verify,
        setupUrl
          ? null
          : chip({ variant: "warn", dot: true, label: "Configure your tunnel hostname before verifying." }),
      ),
    }) : null,
    card({
      title: "cloudflared journal",
      subtitle: "Live tail of the cloudflared service.",
      body: el("div", { className: "card-pad" }, console_),
    }),
  );
}

function renderPairStep(status, onMutate) {
  const cc = status.cloud_choice || {};
  if (cc.mode === "local") {
    return card({
      title: "Pairing not required",
      body: el("div", { className: "card-pad" },
        el("p", { style: { color: "var(--text-secondary)", fontSize: "13px" } },
          "Local-only mode is active. Mission Control reaches this device directly over the LAN."),
      ),
    });
  }

  if (cc.paired) {
    return card({
      title: "Paired",
      severity: "ok",
      body: el("div", { className: "card-pad", style: { display: "flex", flexDirection: "column", gap: "10px" } },
        chip({ variant: "ok", dot: true, label: "Mission Control is connected" }),
        el("p", { style: { color: "var(--text-secondary)", fontSize: "13px" } },
          "This device is already paired with a Mission Control account. You can continue, or unpair from Settings → Devices on Mission Control."),
      ),
    });
  }

  const network = status.network || {};
  const mdnsHost = network.mdns_host || "";
  const isSelfHosted = cc.mode === "self_hosted";
  const deploymentUrl = (cc.backend_url || "").replace(/\/$/, "");
  const altnauticaPair = "https://altnautica.com/pair";
  const deepLink = (code) => {
    const params = new URLSearchParams();
    if (code) params.set("code", code);
    if (mdnsHost) params.set("host", mdnsHost);
    const base = isSelfHosted
      ? (deploymentUrl ? `${deploymentUrl}/pair` : "")
      : altnauticaPair;
    return base ? `${base}?${params.toString()}` : "";
  };

  // Agent-generated code panel (left). Live countdown + copy + deep link.
  const codeBox = el("div", { className: "code-chip", text: "------" });
  const countdownChip = el("span", {}, chip({ variant: "muted", label: "expires in —", size: "sm" }));
  const copyBtn = btn("Copy code", { onclick: async () => {
    if (!codeBox.textContent || codeBox.textContent === "------") return;
    try { await navigator.clipboard.writeText(codeBox.textContent); } catch { /* clipboard denied */ }
  }});
  const deepLinkSlot = el("div", { className: "btn-row", style: { marginTop: "10px" } });
  const renderDeepLinkButton = (code) => {
    deepLinkSlot.replaceChildren();
    const url = deepLink(code);
    if (url) {
      deepLinkSlot.appendChild(btn("Open in ADOS GCS", { href: url, external: true, variant: "primary" }));
      deepLinkSlot.appendChild(copyBtn);
    } else {
      deepLinkSlot.appendChild(chip({ variant: "warn", dot: true, label: "Open Mission Control on your deployment to claim this code" }));
      deepLinkSlot.appendChild(copyBtn);
    }
  };
  renderDeepLinkButton(null);

  let codeState = { code: null, expiresAt: null };
  const refreshCode = async () => {
    try {
      const res = await apiFetch("/api/pairing/code");
      if (res?.code) {
        codeBox.textContent = res.code;
        codeState = { code: res.code, expiresAt: Date.now() + 15 * 60 * 1000 };
        renderDeepLinkButton(res.code);
      }
    } catch (err) {
      codeBox.textContent = "------";
      countdownChip.replaceChildren(chip({ variant: "err", dot: true, label: "Could not load code", size: "sm" }));
    }
  };

  let countdownTimer = null;
  const tickCountdown = () => {
    if (!codeState.expiresAt) return;
    const remaining = Math.max(0, codeState.expiresAt - Date.now());
    if (remaining <= 0) {
      countdownChip.replaceChildren(chip({ variant: "warn", dot: true, label: "regenerating…", size: "sm" }));
      refreshCode();
      return;
    }
    const mm = String(Math.floor(remaining / 60000)).padStart(2, "0");
    const ss = String(Math.floor((remaining % 60000) / 1000)).padStart(2, "0");
    countdownChip.replaceChildren(chip({ variant: "muted", label: `expires in ${mm}:${ss}`, size: "sm" }));
  };
  refreshCode().then(tickCountdown);
  countdownTimer = setInterval(tickCountdown, 1000);
  wizardOnDispose(() => { if (countdownTimer) clearInterval(countdownTimer); });

  // GCS-pre-generated code panel (right). Operator types the code, agent
  // claims itself.
  const acceptInput = el("input", {
    className: "code-input",
    type: "text",
    maxlength: "8",
    placeholder: "------",
    autocomplete: "off",
    spellcheck: false,
    inputmode: "text",
  });
  const acceptStatus = el("div", { style: { minHeight: "20px", marginTop: "8px" } });
  let acceptInFlight = false;
  let acceptAbort = null;
  const setAcceptStatus = (variant, label, withDot = true) => {
    acceptStatus.replaceChildren(chip({ variant, dot: withDot, label, size: "sm" }));
  };
  const submitAccepted = async () => {
    const raw = (acceptInput.value || "").toUpperCase().replace(/[^A-Z0-9]/g, "");
    if (raw.length !== 6) {
      setAcceptStatus("warn", "Code must be 6 characters");
      return;
    }
    if (acceptInFlight) return;
    acceptInFlight = true;
    acceptInput.disabled = true;
    setAcceptStatus("info", "Verifying…", true);
    acceptAbort = new AbortController();
    try {
      const res = await fetch("/api/pairing/accept", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ code: raw }),
        signal: acceptAbort.signal,
      });
      const body = res.headers.get("content-type")?.includes("application/json") ? await res.json() : null;
      if (res.ok && body?.ok) {
        setAcceptStatus("ok", "Paired");
        await onMutate();
      } else {
        setAcceptStatus("err", body?.message || `Failed (${res.status})`);
      }
    } catch (err) {
      if (err.name !== "AbortError") {
        setAcceptStatus("err", err.message || "Network error");
      }
    } finally {
      acceptInFlight = false;
      acceptInput.disabled = false;
    }
  };
  acceptInput.addEventListener("input", () => {
    const cleaned = (acceptInput.value || "").toUpperCase().replace(/[^A-Z0-9]/g, "").slice(0, 6);
    acceptInput.value = cleaned;
    if (cleaned.length === 6) submitAccepted();
  });
  acceptInput.addEventListener("keydown", (ev) => {
    if (ev.key === "Enter") submitAccepted();
  });

  // Polling watches the agent-generated code path. If Mission Control
  // claims via the deep link, this catches it and auto-advances. We stop
  // the timer when the wizard tab leaves the step (the wizard re-renders
  // on every step change, which clears this body and its timers via the
  // disposal pattern wired into wizardBeforeNextHook below).
  const pollTimer = setInterval(async () => {
    try {
      const info = await apiFetch("/api/pairing/info");
      if (info?.paired) {
        clearInterval(pollTimer);
        if (countdownTimer) clearInterval(countdownTimer);
        setAcceptStatus("ok", "Paired");
        await onMutate();
      }
    } catch { /* keep polling */ }
  }, 3000);
  wizardOnDispose(() => clearInterval(pollTimer));
  wizardOnDispose(() => { if (acceptAbort) acceptAbort.abort(); });

  const pairLater = btn("Pair later (switch to local mode)", {
    variant: "ghost",
    onclick: async () => {
      if (acceptAbort) acceptAbort.abort();
      if (countdownTimer) clearInterval(countdownTimer);
      clearInterval(pollTimer);
      try {
        await apiFetch("/api/v1/setup/cloud-choice", {
          method: "POST",
          body: JSON.stringify({ mode: "local" }),
        });
        await onMutate();
      } catch (err) {
        setAcceptStatus("err", `Could not switch to local: ${err.message || err}`);
      }
    },
  });

  return el("div", { className: "page-body" },
    card({
      body: el("div", { className: "card-pad", style: { display: "flex", flexDirection: "column", gap: "10px" } },
        el("p", { style: { color: "var(--text-secondary)", fontSize: "13.5px" } },
          isSelfHosted
            ? "Pair this device with your self-hosted Mission Control. Use either side: show this device's code in your deployment, or paste a code from your deployment into this device."
            : "Pair this device with Mission Control. Use either side: open Altnautica in a browser to claim this device's code, or paste a code Mission Control gave you."),
      ),
    }),
    el("div", { className: "two-pane" },
      card({
        title: "Show this device's code",
        subtitle: "Copy or open in Mission Control to claim it.",
        body: el("div", { className: "card-pad", style: { display: "flex", flexDirection: "column", gap: "10px" } },
          el("div", { style: { display: "flex", justifyContent: "center" } }, codeBox),
          el("div", { style: { display: "flex", justifyContent: "center" } }, countdownChip),
          deepLinkSlot,
        ),
      }),
      card({
        title: "Got a code from Mission Control?",
        subtitle: "Paste it here.",
        body: el("div", { className: "card-pad", style: { display: "flex", flexDirection: "column", gap: "10px" } },
          acceptInput,
          acceptStatus,
        ),
      }),
    ),
    card({
      body: el("div", { className: "card-pad", style: { display: "flex", justifyContent: "space-between", alignItems: "center", gap: "12px", flexWrap: "wrap" } },
        chip({ variant: "muted", label: "Pairing waits for either side to complete. Polling every 3s." }),
        pairLater,
      ),
    }),
  );
}

function renderFinishStep(status) {
  const setupUrl = findUrl(status, (u) => u.kind === "setup" && u.primary);
  const mc = findUrl(status, (u) => u.kind === "mission_control");
  return card({
    title: "Finish",
    body: el("div", { className: "card-pad" },
      el("p", { style: { color: "var(--text-secondary)", fontSize: "13px", marginBottom: "12px" } },
        "Setup is complete. The Setup page is reachable any time from the sidebar; re-run any step to change values."),
      el("div", { className: "btn-row" },
        setupUrl ? btn("Copy setup URL", { onclick: () => navigator.clipboard?.writeText(setupUrl.url) }) : null,
        mc?.url ? btn("Open Mission Control", { variant: "primary", href: mc.url, external: true }) : null,
      ),
    ),
  });
}

function renderProfileStep(status, onMutate) {
  const suggestion = status.profile_suggestion || {};
  const currentProfile = status.profile === "ground_station" ? "ground_station" : "drone";
  const currentRole = status.ground_role || suggestion.ground_role_hint || "direct";

  // Pre-select: if the operator already confirmed a profile, surface that.
  // Otherwise pick the live-detected suggestion.
  let selectedKey;
  if (suggestion.confirmed) {
    selectedKey = currentProfile === "ground_station" ? `gs_${currentRole}` : "drone";
  } else if (suggestion.detected === "ground_station") {
    selectedKey = `gs_${suggestion.ground_role_hint || "direct"}`;
  } else if (suggestion.detected === "drone") {
    selectedKey = "drone";
  } else {
    selectedKey = currentProfile === "ground_station" ? `gs_${currentRole}` : "drone";
  }

  const signalLine = (signals) => {
    const entries = Object.entries(signals || {});
    if (!entries.length) return "No live signals reported.";
    return entries
      .map(([name, present]) => `${name}: ${present ? "yes" : "no"}`)
      .join("  ·  ");
  };

  const detected = suggestion.detected || "unconfigured";
  const detectedLabel = detected === "ground_station"
    ? `ground station (${suggestion.ground_role_hint || "direct"})`
    : detected === "drone" ? "drone" : "unconfigured";

  const cards = el("div", { className: "cloud-cards" });
  const renderRadio = (key, title, blurb, isDetected) => {
    const isSelected = selectedKey === key;
    return el("label", { className: `cloud-card ${isSelected ? "selected" : ""}`.trim() },
      el("input", {
        type: "radio",
        name: "profile_choice",
        value: key,
        checked: isSelected,
        onchange: () => {
          selectedKey = key;
          renderCardClasses();
        },
      }),
      el("div", { className: "cloud-card-body" },
        el("strong", { style: { display: "inline-flex", alignItems: "center", gap: "8px" } }, title,
          isDetected ? chip({ variant: "ok", dot: true, pulse: true, label: "Detected", size: "sm" }) : null,
        ),
        el("p", {}, blurb),
      ),
    );
  };

  const renderCardClasses = () => {
    Array.from(cards.children).forEach((node) => {
      const radio = node.querySelector("input[type=radio]");
      node.classList.toggle("selected", radio?.checked);
    });
  };

  const isGroundDetected = (role) =>
    detected === "ground_station" && (suggestion.ground_role_hint || "direct") === role;

  cards.append(
    renderRadio(
      "drone",
      "Drone (air-side companion)",
      "Companion computer mounted on the aircraft. MAVLink to the FC, camera capture, optional 4G uplink.",
      detected === "drone",
    ),
    renderRadio(
      "gs_direct",
      "Ground station — Direct",
      "Single-radio receiver. WFB-ng directly into mediamtx; no mesh.",
      isGroundDetected("direct"),
    ),
    renderRadio(
      "gs_relay",
      "Ground station — Relay",
      "Forwards WFB fragments to a receiver over batman-adv. Needs a second USB wireless adapter.",
      isGroundDetected("relay"),
    ),
    renderRadio(
      "gs_receiver",
      "Ground station — Receiver",
      "Aggregates relay streams + local NIC, FEC-combined for the mediamtx pipeline. Needs a second USB wireless adapter.",
      isGroundDetected("receiver"),
    ),
  );

  // Inline status slot. Shown while saving + on save failure. The save
  // happens when the operator clicks Continue (wired via the
  // wizardBeforeNextHook), not via a separate button — one CTA, not two.
  const statusEl = el("div", {
    className: "form-result",
    style: { marginTop: "12px", minHeight: "1.2em" },
  });

  wizardBeforeNextHook = async () => {
    const body = selectedKey === "drone"
      ? { profile: "drone" }
      : { profile: "ground_station", ground_role: selectedKey.slice(3) };
    statusEl.textContent = "Saving profile…";
    statusEl.className = "form-result";
    try {
      const res = await apiFetch("/api/v1/setup/profile", {
        method: "POST",
        body: JSON.stringify(body),
      });
      if (res?.ok === false) {
        statusEl.textContent = res.message || "Failed to save profile.";
        statusEl.className = "form-result err";
        return false;
      }
      statusEl.textContent = res?.message || "Profile saved.";
      statusEl.className = "form-result ok";
      await onMutate();
      return true;
    } catch (err) {
      statusEl.textContent = `Failed: ${err.message || err}`;
      statusEl.className = "form-result err";
      return false;
    }
  };

  return el("div", { className: "page-body" },
    card({
      title: "Profile",
      subtitle: `Auto-detected: ${detectedLabel}. Air score ${suggestion.air_score ?? 0}, ground score ${suggestion.ground_score ?? 0}.`,
      body: el("div", { className: "card-pad" },
        el("p", { style: { color: "var(--text-secondary)", fontSize: "13px", marginBottom: "12px" } },
          "Pick the role this device should run as, then click Continue. The wizard branches the rest of the steps based on this choice."),
        cards,
        statusEl,
      ),
    }),
    card({
      title: "Live signals",
      subtitle: "Sensors the boot-time fingerprint observed on this device.",
      body: el("div", { className: "card-pad" },
        el("p", { style: { color: "var(--text-tertiary)", fontSize: "12px", fontFamily: "var(--mono, monospace)" } },
          signalLine(suggestion.signals)),
        suggestion.mesh_capable
          ? el("p", { style: { color: "var(--text-secondary)", fontSize: "13px", marginTop: "8px" } },
              "Second USB wireless adapter detected. Relay and Receiver roles are eligible.")
          : el("p", { style: { color: "var(--text-tertiary)", fontSize: "12px", marginTop: "8px" } },
              "Only one wireless adapter detected. Mesh roles need a second USB WiFi dongle."),
      ),
    }),
  );
}

function renderHardwareCheckStep(status, onMutate) {
  let snapshot = status.hardware_check || { items: [], profile: status.profile, last_run: "" };

  const variantFor = (state) => {
    if (state === "ok") return "ok";
    if (state === "missing") return "err";
    if (state === "warning") return "warn";
    if (state === "checking") return "info";
    return "muted";
  };
  const labelFor = (state) => {
    if (state === "ok") return "OK";
    if (state === "missing") return "Missing";
    if (state === "warning") return "Warning";
    if (state === "checking") return "Checking";
    return "Unknown";
  };

  const itemRow = (item) => el("div", { className: "live-table-row" },
    statusDot(variantFor(item.state), item.state === "checking"),
    el("div", { className: "live-table-name" },
      el("span", {}, item.label,
        item.required ? chip({ variant: "info", label: "required", size: "sm" }) : null,
      ),
      item.fix_hint
        ? el("span", { className: "secondary", text: item.fix_hint })
        : null,
    ),
    el("div", { className: "live-table-detail", text: item.detail || "—" }),
    el("div", { className: "live-table-actions" },
      chip({ variant: variantFor(item.state), dot: true, label: labelFor(item.state), size: "sm" }),
    ),
  );

  const counts = () => {
    const items = snapshot.items || [];
    const required = items.filter((i) => i.required);
    const ok = required.filter((i) => i.state === "ok").length;
    return { required: required.length, ok };
  };

  const summaryChips = () => {
    const c = counts();
    const profileLabel = `${snapshot.profile || "?"}${snapshot.ground_role ? ` · ${snapshot.ground_role}` : ""}`;
    const allOk = c.required === 0 || c.ok === c.required;
    return [
      chip({ variant: "info", label: profileLabel }),
      chip({ variant: allOk ? "ok" : "warn", dot: true, label: `${c.ok} / ${c.required} required` }),
      snapshot.last_run ? chip({ variant: "muted", label: `last run ${snapshot.last_run}`, size: "sm" }) : null,
    ].filter(Boolean);
  };

  const itemsContainer = el("div", { className: "live-table" },
    ...((snapshot.items || []).map(itemRow)),
  );
  const summarySlot = el("div", { className: "live-row-chips", style: { justifyContent: "flex-start" } }, ...summaryChips());

  const refreshBtn = el("button", {
    type: "button",
    className: "btn",
    text: "Refresh",
    onclick: async () => {
      refreshBtn.disabled = true;
      const original = refreshBtn.textContent;
      refreshBtn.textContent = "Refreshing…";
      try {
        const res = await apiFetch("/api/v1/setup/hardware-check/refresh", { method: "POST" });
        if (res) {
          snapshot = res;
          itemsContainer.replaceChildren(...((snapshot.items || []).map(itemRow)));
          summarySlot.replaceChildren(...summaryChips());
        }
        await onMutate();
      } catch (err) {
        console.error("hardware check refresh failed:", err);
      } finally {
        refreshBtn.disabled = false;
        refreshBtn.textContent = original;
      }
    },
  });

  const c = counts();
  const allOk = c.required === 0 || c.ok === c.required;

  return el("div", { className: "page-body" },
    card({
      title: "Hardware check",
      subtitle: allOk
        ? "All required components detected. Continue when ready."
        : "Some required components are missing. Plug them in and refresh, or continue without them.",
      severity: allOk ? "ok" : "warn",
      actions: [refreshBtn],
      body: el("div", {},
        el("div", { className: "card-pad" }, summarySlot),
        itemsContainer,
      ),
    }),
  );
}

function renderGenericStep(step, status) {
  return card({
    title: step.label || step.id,
    severity: severityForState(step.state),
    body: el("div", { className: "card-pad" },
      el("p", { style: { color: "var(--text-secondary)", fontSize: "13px" } },
        step.detail || "No details available."),
      step.href ? el("div", { className: "btn-row", style: { marginTop: "10px" } },
        btn(step.action_label || "Open", { href: step.href }),
      ) : null,
    ),
  });
}

// ---------------------------------------------------------------------------
// Page: mavlink
// ---------------------------------------------------------------------------

function renderMavlink() {
  subscribe((status) => {
    const m = status.mavlink || {};
    const sev = severityForMavlink(m);

    const content = [
      pageHeader({
        eyebrow: "MAVLink",
        title: "Flight controller link",
        subtitle: m.connected
          ? `Connected on ${m.port || "?"} at ${m.baud || "?"} baud.`
          : "No flight controller currently connected.",
      }),
      el("div", { className: "grid cols-3" },
        statTile({ label: "State", value: m.connected ? "Connected" : "Idle", dotSeverity: sev }),
        statTile({ label: "Port", value: pretty(m.port) }),
        statTile({ label: "Baud", value: pretty(m.baud) }),
      ),
      card({
        title: "WebSocket endpoints",
        subtitle: "Use these in Mission Control or any MAVLink client.",
        body: el("div", { className: "list" },
          m.websocket_url
            ? urlRow("Local WebSocket", m.websocket_url, "LAN, hotspot, USB tether")
            : el("div", { className: "empty" }, "No local WebSocket reported."),
          m.public_websocket_url
            ? urlRow("Tunnel WebSocket", m.public_websocket_url, "Cloudflare Tunnel")
            : null,
        ),
      }),
      card({
        title: "Troubleshooting",
        body: el("div", { className: "card-pad" },
          el("ul", { style: { paddingLeft: "18px", color: "var(--text-secondary)", fontSize: "13px", display: "flex", flexDirection: "column", gap: "6px", listStyle: "disc" } },
            el("li", {}, "Confirm the FC is powered and the USB or UART cable is connected."),
            el("li", {}, "Check the baud rate matches the FC firmware (typically 57600 or 115200)."),
            el("li", {}, "Open the System page to view the last MAVLink-related log lines."),
            el("li", {}, "If telemetry is intermittent, look for power dropouts on the FC."),
          ),
        ),
      }),
    ];

    renderShell("mavlink", content);
  });
}

// ---------------------------------------------------------------------------
// Page: video
// ---------------------------------------------------------------------------

function renderVideo() {
  subscribe((status) => {
    const v = status.video || {};
    const sev = severityForVideo(status);
    const content = [
      pageHeader({
        eyebrow: "Video",
        title: "Camera and video pipeline",
        subtitle: `Pipeline state: ${pretty(v.state)}.`,
      }),
      el("div", { className: "grid cols-3" },
        statTile({ label: "State", value: pretty(v.state), dotSeverity: sev }),
        statTile({ label: "Recording", value: v.recording ? "On" : "Off", dotSeverity: v.recording ? "ok" : "muted" }),
        statTile({ label: "Profile", value: pretty(status.profile) }),
      ),
      card({
        title: "WHEP endpoints",
        subtitle: "Open in a browser or feed Mission Control's video tile.",
        body: el("div", { className: "list" },
          v.whep_url
            ? urlRow("Local WHEP", v.whep_url, "LAN, hotspot, USB tether")
            : el("div", { className: "empty" }, "No local WHEP URL reported."),
          v.public_whep_url
            ? urlRow("Tunnel WHEP", v.public_whep_url, "Cloudflare Tunnel")
            : null,
        ),
      }),
    ];

    renderShell("video", content);
  });
}

// ---------------------------------------------------------------------------
// Page: network
// ---------------------------------------------------------------------------

function renderNetwork() {
  subscribe((status) => {
    const n = status.network || {};
    const setupUrls = (status.access_urls || []).filter((u) => u.kind === "setup");
    const sev = severityForNetwork(status);

    const content = [
      pageHeader({
        eyebrow: "Network",
        title: "Local access",
        subtitle: "Where this agent is reachable from.",
      }),
      el("div", { className: "grid cols-3" },
        statTile({ label: "State", value: sev === "ok" ? "Reachable" : "Limited", dotSeverity: sev }),
        statTile({ label: "Hostname", value: pretty(n.hostname) }),
        statTile({ label: "API port", value: pretty(n.api_port) }),
      ),
      card({
        title: "Local network",
        body: el("div", { className: "dl-rows" },
          dlRow("Hostname", n.hostname),
          dlRow("mDNS host", n.mdns_host),
          dlRow("Hotspot", n.hotspot_enabled ? `Enabled (${n.hotspot_ssid || "—"})` : "Disabled"),
          dlRow("Local IPs", (n.local_ips || []).join(", ")),
        ),
      }),
      card({
        title: "Setup URLs",
        subtitle: "Pick whichever network is reachable from your phone or laptop.",
        body: el("div", { className: "list" },
          ...(setupUrls.length
            ? setupUrls.map((u) => urlRow(u.label, u.url, u.source))
            : [el("div", { className: "empty" }, "No setup URLs reported.")]
          ),
        ),
      }),
    ];

    renderShell("network", content);
  });
}

// ---------------------------------------------------------------------------
// Page: remote (Cloudflare quick install)
// ---------------------------------------------------------------------------

function renderRemote() {
  let resultMessage = null;
  let resultSeverity = "";

  subscribe((status) => {
    const r = status.remote_access || {};
    const sev = severityForRemote(status);
    const textarea = el("textarea", {
      name: "token",
      placeholder: "Paste a Cloudflare tunnel token or the install command Cloudflare shows.",
      autocomplete: "off",
      spellcheck: false,
    });
    const result = el("div", { className: `form-result ${resultSeverity}`.trim() }, resultMessage || "");

    const form = el("form", {
      className: "form",
      onsubmit: async (e) => {
        e.preventDefault();
        const value = textarea.value.trim();
        if (!value) {
          resultMessage = "Paste a token or install command first.";
          resultSeverity = "err";
          result.textContent = resultMessage;
          result.className = `form-result err`;
          return;
        }
        resultMessage = "Installing…";
        resultSeverity = "";
        result.textContent = resultMessage;
        result.className = "form-result";
        try {
          const res = await apiFetch("/api/v1/setup/remote-access/cloudflare", {
            method: "POST",
            body: JSON.stringify({ token_or_script: value }),
          });
          textarea.value = "";
          resultMessage = (res && typeof res.message === "string") ? res.message : "Token installed.";
          resultSeverity = res?.ok === false ? "err" : "ok";
          result.textContent = resultMessage;
          result.className = `form-result ${resultSeverity}`;
          await loadStatus();
        } catch (err) {
          textarea.value = "";
          resultMessage = `Failed: ${err.message || err}`;
          resultSeverity = "err";
          result.textContent = resultMessage;
          result.className = "form-result err";
        }
      },
    },
      el("label", {},
        el("span", {}, "Cloudflare tunnel token or install command"),
        textarea,
      ),
      el("div", { className: "form-help" }, "The token is written to a root-owned secret file. It is never echoed back into this page or stored in your browser."),
      el("div", { className: "btn-row" },
        btn("Install token", { variant: "primary", type: "submit" }),
        btn("Clear", {
          onclick: () => {
            textarea.value = "";
            resultMessage = "Cleared.";
            resultSeverity = "";
            result.textContent = resultMessage;
            result.className = "form-result";
          },
        }),
      ),
      result,
    );

    const content = [
      pageHeader({
        eyebrow: "Remote access",
        title: "Optional cloud access",
        subtitle: "Cloudflare Tunnel exposes the agent to remote support without opening router ports.",
      }),
      el("div", { className: "grid cols-3" },
        statTile({ label: "Provider", value: pretty(r.provider), dotSeverity: r.provider === "none" ? "muted" : sev }),
        statTile({ label: "Status", value: pretty(r.status), dotSeverity: sev }),
        statTile({ label: "Public URLs", value: r.public_urls?.length ? `${r.public_urls.length} configured` : "None" }),
      ),
      r.error ? card({
        title: "Issue",
        severity: "err",
        body: el("div", { className: "card-pad" }, el("p", { style: { color: "var(--status-error)" } }, r.error)),
      }) : null,
      r.public_urls?.length ? card({
        title: "Public URLs",
        body: el("div", { className: "list" }, ...r.public_urls.map((u) => urlRow("Public endpoint", u, "via tunnel"))),
      }) : null,
      card({
        title: "Install Cloudflare token",
        subtitle: "Paste the token from Cloudflare Zero Trust, or the install command Cloudflare shows.",
        body: form,
      }),
    ].filter(Boolean);

    renderShell("remote", content);
  });
}

// ---------------------------------------------------------------------------
// Page: ground station
// ---------------------------------------------------------------------------

function renderGround() {
  subscribe((status) => {
    const isGround = status.profile === "ground_station";
    const content = [
      pageHeader({
        eyebrow: "Ground station",
        title: isGround ? "Ground station" : "Ground station (inactive)",
        subtitle: isGround
          ? "Profile-aware ground-station controls."
          : `This agent is running the ${pretty(status.profile)} profile.`,
      }),
      isGround
        ? card({
            title: "Profile is active",
            severity: "ok",
            body: el("div", { className: "card-pad" },
              el("p", { style: { color: "var(--text-secondary)", fontSize: "13px", marginBottom: "10px" } },
                "Pairing, WFB receiver, uplink, and mesh controls are exposed via the agent REST API and Mission Control's Hardware tab. Use those surfaces for live operations."),
              el("div", { className: "btn-row" },
                btn("API reference", { href: "/docs", external: true }),
                btn("Mission Control", { href: findUrl(status, (u) => u.kind === "mission_control")?.url || "#" }),
              ),
            ),
          })
        : card({
            title: "Inactive on this profile",
            body: el("div", { className: "card-pad" },
              el("p", { style: { color: "var(--text-secondary)", fontSize: "13px" } },
                "Ground-station configuration is only shown when the agent profile is set to ground_station."),
            ),
          }),
    ];
    renderShell("ground", content);
  });
}

// ---------------------------------------------------------------------------
// Page: system & logs
// ---------------------------------------------------------------------------

function renderSystem() {
  let systemMetrics = null;
  let logEntries = null;
  let extrasLoaded = false;

  subscribe(async (status) => {
    const services = status.services || [];

    if (!extrasLoaded) {
      extrasLoaded = true;
      const [metricsRes, logsRes] = await Promise.all([
        tryFetch("/api/system"),
        tryFetch("/api/logs?limit=40"),
      ]);
      if (metricsRes && typeof metricsRes === "object") systemMetrics = metricsRes;
      if (logsRes) {
        logEntries = Array.isArray(logsRes?.entries) ? logsRes.entries : Array.isArray(logsRes) ? logsRes : null;
      }
    }

    const cpu = systemMetrics?.cpu_percent ?? systemMetrics?.cpu;
    const mem = systemMetrics?.memory_percent ?? systemMetrics?.memory;
    const disk = systemMetrics?.disk_percent ?? systemMetrics?.disk;

    const content = [
      pageHeader({
        eyebrow: "System",
        title: "System & logs",
        subtitle: "Service health, system resources, and recent log lines.",
      }),
      el("div", { className: "grid cols-4" },
        statTile({ label: "Version", value: status.version || "—" }),
        statTile({ label: "Profile", value: pretty(status.profile) }),
        statTile({ label: "Hostname", value: pretty(status.network?.hostname) }),
        statTile({ label: "Services", value: `${services.filter((s) => severityForState(s.state) === "ok").length}/${services.length || "—"}` }),
      ),
      systemMetrics
        ? el("div", { className: "grid cols-3" },
            statTile({
              label: "CPU",
              value: cpu != null ? `${Math.round(cpu)}%` : "—",
              dotSeverity: cpu == null ? "muted" : cpu > 80 ? "err" : cpu > 50 ? "warn" : "ok",
            }),
            statTile({
              label: "Memory",
              value: mem != null ? `${Math.round(mem)}%` : "—",
              dotSeverity: mem == null ? "muted" : mem > 80 ? "err" : mem > 50 ? "warn" : "ok",
            }),
            statTile({
              label: "Disk",
              value: disk != null ? `${Math.round(disk)}%` : "—",
              dotSeverity: disk == null ? "muted" : disk > 90 ? "err" : disk > 70 ? "warn" : "ok",
            }),
          )
        : null,
      services.length
        ? card({
            title: "Services",
            subtitle: `${services.filter((s) => severityForState(s.state) === "ok").length} of ${services.length} running.`,
            body: el("div", { className: "list" },
              ...services.map((svc) => el("div", { className: "list-row" },
                el("div", { className: "label-block" },
                  el("span", { className: "primary-text" }, svc.name),
                ),
                el("div", { className: "actions" },
                  pill(severityForState(svc.state), pretty(svc.state)),
                ),
              )),
            ),
          })
        : null,
      logEntries && logEntries.length
        ? card({
            title: "Recent logs",
            subtitle: "Last 40 entries from the agent's in-memory ring buffer.",
            body: el("div", { className: "log" },
              ...logEntries.slice(-40).map((entry) => {
                const text = typeof entry === "string" ? entry : entry?.message || JSON.stringify(entry);
                const lvl = (entry?.level || "").toLowerCase();
                const cls = lvl === "warning" || lvl === "warn" ? "warn" : (lvl === "error" || lvl === "critical") ? "err" : "";
                return el("span", { className: `log-line ${cls}`.trim() }, text);
              }),
            ),
          })
        : card({
            title: "Recent logs",
            body: el("div", { className: "card-pad" },
              el("p", { style: { color: "var(--text-secondary)", fontSize: "13px", marginBottom: "10px" } },
                "Log streaming is not available in this session. View live logs on the host with journalctl, or use the API reference for the logs endpoint."),
              el("div", { className: "btn-row" },
                btn("API reference", { href: "/docs", external: true }),
              ),
            ),
          }),
    ].filter(Boolean);

    renderShell("system", content);
  });
}

// ---------------------------------------------------------------------------
// Page: advanced
// ---------------------------------------------------------------------------

function renderAdvanced() {
  subscribe((status) => {
    const content = [
      pageHeader({
        eyebrow: "Advanced",
        title: "Advanced",
        subtitle: "Low-frequency actions for operators and support.",
      }),
      card({
        title: "API reference",
        subtitle: "Inspect the full REST surface served by this agent.",
        body: el("div", { className: "card-pad" },
          el("div", { className: "btn-row" },
            btn("Open Swagger UI", { variant: "primary", href: "/docs", external: true }),
            btn("Open ReDoc", { href: "/redoc", external: true }),
          ),
        ),
      }),
      card({
        title: "Status payload",
        subtitle: "Live JSON used to render every page in this webapp.",
        body: el("pre", { className: "log" },
          status ? JSON.stringify(status, null, 2) : "Loading status…",
        ),
      }),
    ];
    renderShell("advanced", content);
  });
}

// ---------------------------------------------------------------------------
// Bootstrap
// ---------------------------------------------------------------------------

const PAGES = {
  dashboard: renderDashboard,
  setup: renderSetup,
  mavlink: renderMavlink,
  video: renderVideo,
  network: renderNetwork,
  remote: renderRemote,
  ground: renderGround,
  system: renderSystem,
  advanced: renderAdvanced,
};

async function gateBootstrap() {
  // Block the page until we know whether the operator has finished
  // onboarding. Anything other than the setup page redirects into the
  // wizard when setup_finalized is false. The setup page itself forces
  // wizard chrome regardless of ?mode= when not finalized so deep-links
  // cannot escape the gate.
  const page = (document.body && document.body.dataset && document.body.dataset.page) || "dashboard";
  let status = null;
  try {
    status = await apiFetch("/api/v1/setup/status");
    currentStatus = status;
    subscribers.forEach((fn) => {
      try { fn(status); } catch (e) { console.error(e); }
    });
  } catch (err) {
    console.error("gate: setup status load failed:", err);
  }

  const finalized = !!(status && status.setup_finalized);
  if (!finalized && page !== "setup") {
    // Pass the first incomplete step as a deep link so the wizard
    // lands the operator where their attention is needed.
    const steps = (status && status.steps) || [];
    const target = steps.find((s) => s.state === "needs_action");
    const stepParam = target ? `&step=${encodeURIComponent(target.id)}` : "";
    window.location.replace(`/setup.html?mode=wizard${stepParam}`);
    return;
  }
  if (!finalized && page === "setup") {
    // Force wizard chrome. Strip ?mode=revisit if the operator tried
    // to escape via URL editing.
    const params = new URLSearchParams(window.location.search);
    if (params.get("mode") !== "wizard") {
      params.set("mode", "wizard");
      const target = (status && status.steps || []).find((s) => s.state === "needs_action");
      if (target && !params.get("step")) {
        params.set("step", target.id);
      }
      window.history.replaceState(null, "", `/setup.html?${params.toString()}`);
    }
  }

  (PAGES[page] || renderDashboard)();
  startPolling();
}

if (document.readyState === "loading") {
  document.addEventListener("DOMContentLoaded", gateBootstrap, { once: true });
} else {
  gateBootstrap();
}
