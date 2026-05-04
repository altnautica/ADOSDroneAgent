// ADOS universal setup webapp.
// Single ES module SPA dispatcher. Renders one shared shell per HTML page
// based on document.body.dataset.page, then delegates to a per-page
// renderer. All API data is rendered with textContent / DOM creation; no
// API string is ever passed to innerHTML.

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
  const params = new URLSearchParams(window.location.search);
  const explicit = params.get("mode");
  if (explicit === "wizard" || explicit === "revisit") return explicit;
  return status?.setup_complete ? "revisit" : "wizard";
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

function renderSetup() {
  subscribe((status) => {
    const mode = setupModeFor(status);
    if (mode === "wizard") {
      renderWizard(status, setupStepFor(status, mode));
    } else {
      renderRevisit(status);
    }
  });
}

function renderRevisit(status) {
  const steps = status.steps || [];
  const next = steps.find((s) => s.state === "needs_action");

  const content = [
    pageHeader({
      eyebrow: "Setup",
      title: "Setup checklist",
      subtitle: status.setup_complete
        ? "All required steps are complete. Re-run any step from the list below."
        : "Walk through the remaining steps to bring this device online.",
      actions: [
        btn("Re-run setup", {
          href: "/setup.html?mode=wizard",
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

function renderWizard(status, currentStepId) {
  const steps = status.steps || [];
  const currentIdx = Math.max(0, steps.findIndex((s) => s.id === currentStepId));
  const currentStep = steps[currentIdx] || steps[0];
  if (!currentStep) {
    renderShell("setup", [pageHeader({ eyebrow: "Setup", title: "No steps reported" })]);
    return;
  }
  const total = steps.length;
  const isFirst = currentIdx === 0;
  const isLast = currentIdx === total - 1;
  const isSkippable = currentStep.state === "optional" || currentStep.state === "not_applicable" ||
    ["mavlink", "video", "remote_access", "ground_receiver", "pair"].includes(currentStep.id);

  const stepperDots = steps.map((s, idx) => {
    const cls = idx === currentIdx ? "current" : (idx < currentIdx || s.state === "complete") ? "done" : "todo";
    return el("span", { className: `wizard-pip ${cls}`, "aria-label": `Step ${idx + 1}` });
  });

  const goTo = (id) => {
    const params = new URLSearchParams(window.location.search);
    params.set("mode", "wizard");
    if (id) params.set("step", id); else params.delete("step");
    window.location.assign(`/setup.html?${params.toString()}`);
  };
  const exitWizard = () => {
    window.location.assign("/setup.html?mode=revisit");
  };

  const stepBody = renderWizardStepBody(currentStep, status, () => loadStatus());

  const content = [
    el("header", { className: "wizard-header" },
      el("div", { className: "wizard-stepper" },
        el("span", { className: "wizard-step-count" }, `Step ${currentIdx + 1} of ${total}`),
        el("div", { className: "wizard-pips" }, ...stepperDots),
      ),
      el("div", { className: "wizard-header-actions" },
        isSkippable && !isLast
          ? btn("Skip for now", { variant: "ghost", onclick: () => {
              const nextStep = steps[currentIdx + 1];
              if (nextStep) goTo(nextStep.id);
            } })
          : null,
        btn("Exit", { variant: "ghost", onclick: exitWizard }),
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
      btn(isLast ? "Finish" : "Continue", {
        variant: "primary",
        onclick: () => {
          if (isLast) {
            exitWizard();
          } else {
            goTo(steps[currentIdx + 1].id);
          }
        },
      }),
    ),
  ];

  renderShell("setup", content);
}

function renderWizardStepBody(step, status, onMutate) {
  switch (step.id) {
    case "welcome":
      return renderWelcomeStep(status);
    case "cloud_choice":
      return renderCloudChoiceStep(status, onMutate);
    case "network":
      return renderNetworkStep(status);
    case "mavlink":
      return renderMavlinkStepInline(status);
    case "video":
      return renderVideoStepInline(status);
    case "ground_receiver":
      return renderGroundStepInline(status);
    case "remote_access":
      return renderRemoteStepInline(status);
    case "pair":
      return renderPairStep(status);
    case "finish":
      return renderFinishStep(status);
    default:
      return renderGenericStep(step, status);
  }
}

function renderWelcomeStep(status) {
  const isGround = status.profile === "ground_station";
  return card({
    title: "Local-first setup",
    body: el("div", { className: "card-pad" },
      el("p", { style: { color: "var(--text-secondary)", fontSize: "13px", marginBottom: "12px" } },
        `This wizard configures ${status.device_name || "this device"} for ${isGround ? "a ground station" : "a drone"}. Setup is local-first: MAVLink and video work over LAN, hotspot, or USB tether before any cloud step is required. You can exit at any time and pick up later from the Setup page.`),
      el("div", { className: "dl-rows" },
        dlRow("Device name", status.device_name),
        dlRow("Profile", pretty(status.profile)),
        dlRow("Version", status.version),
        dlRow("Device ID", status.device_id),
      ),
    ),
  });
}

function renderCloudChoiceStep(status, onMutate) {
  const current = status.cloud_choice?.mode || "cloud";
  let selected = current;
  let resultMessage = "";
  let resultClass = "";

  const buildForm = () => {
    const sh = el("div", { className: "wizard-form", style: selected === "self_hosted" ? {} : { display: "none" } });
    const urlInput = el("input", { type: "url", name: "url", placeholder: "https://convex.example.com", autocomplete: "off", value: status.cloud_choice?.backend_url || "" });
    const brokerInput = el("input", { type: "text", name: "mqtt_broker", placeholder: "mqtt.example.com", autocomplete: "off" });
    const portInput = el("input", { type: "number", name: "mqtt_port", min: "1", max: "65535", value: "8883" });
    const apiKeyInput = el("input", { type: "password", name: "api_key", placeholder: "Optional. Stored 0600 on device.", autocomplete: "off" });
    sh.append(
      el("label", {}, el("span", {}, "Convex URL"), urlInput),
      el("label", {}, el("span", {}, "MQTT broker"), brokerInput),
      el("label", {}, el("span", {}, "MQTT port"), portInput),
      el("label", {}, el("span", {}, "API key (optional)"), apiKeyInput),
    );

    const result = el("div", { className: `form-result ${resultClass}`.trim() }, resultMessage);

    const submitBtn = btn("Save cloud posture", {
      variant: "primary",
      onclick: async () => {
        const body = { mode: selected };
        if (selected === "self_hosted") {
          body.self_hosted = {
            url: urlInput.value.trim(),
            mqtt_broker: brokerInput.value.trim(),
            mqtt_port: parseInt(portInput.value || "8883", 10),
            api_key: apiKeyInput.value || "",
          };
        }
        result.textContent = "Saving…";
        result.className = "form-result";
        try {
          const res = await apiFetch("/api/v1/setup/cloud-choice", {
            method: "POST",
            body: JSON.stringify(body),
          });
          apiKeyInput.value = "";
          resultMessage = res?.message || "Saved.";
          resultClass = res?.ok === false ? "err" : "ok";
          result.textContent = resultMessage;
          result.className = `form-result ${resultClass}`;
          await onMutate();
        } catch (err) {
          apiKeyInput.value = "";
          resultMessage = `Failed: ${err.message || err}`;
          resultClass = "err";
          result.textContent = resultMessage;
          result.className = "form-result err";
        }
      },
    });

    return el("div", {},
      sh,
      el("div", { className: "btn-row", style: { padding: "16px" } }, submitBtn),
      result,
    );
  };

  const formContainer = el("div", {});
  formContainer.append(buildForm());

  const renderRadio = (mode, title, blurb) => {
    const isSelected = selected === mode;
    const card = el("label", { className: `cloud-card ${isSelected ? "selected" : ""}`.trim() },
      el("input", {
        type: "radio",
        name: "cloud_mode",
        value: mode,
        checked: isSelected,
        onchange: () => {
          selected = mode;
          formContainer.replaceChildren(buildForm());
          renderCardClasses();
        },
      }),
      el("div", { className: "cloud-card-body" },
        el("strong", {}, title),
        el("p", {}, blurb),
      ),
    );
    return card;
  };

  const cards = el("div", { className: "cloud-cards" });
  const renderCardClasses = () => {
    Array.from(cards.children).forEach((node) => {
      const radio = node.querySelector("input[type=radio]");
      node.classList.toggle("selected", radio?.checked);
    });
  };
  cards.append(
    renderRadio("cloud", "Altnautica cloud (default)",
      "Sign in with your Altnautica account on Mission Control. Your devices show up there from anywhere."),
    renderRadio("self_hosted", "Self-hosted backend",
      "Point this device at your own Convex deployment and MQTT broker. Useful for OEMs and operators behind a firewall."),
    renderRadio("local", "Local only",
      "No cloud relay. Mission Control connects directly over LAN, hotspot, or USB tether. You can still enable Cloudflare Tunnel later."),
  );

  return el("div", { className: "page-body" },
    card({
      title: "Choose a cloud posture",
      subtitle: status.cloud_choice?.mode
        ? `Currently set to ${pretty(status.cloud_choice.mode)}.`
        : "How should this device talk to Mission Control?",
      body: el("div", { className: "card-pad" }, cards),
    }),
    selected === "self_hosted" ? card({
      title: "Self-hosted endpoints",
      subtitle: "API key is written to a root-owned secret file and never echoed back.",
      body: formContainer,
    }) : null,
    selected !== "self_hosted" ? card({
      title: "Confirm",
      body: formContainer,
    }) : null,
  );
}

function renderNetworkStep(status) {
  const n = status.network || {};
  const sev = severityForNetwork(status);
  return card({
    title: "Local network",
    severity: sev,
    body: el("div", {},
      el("div", { className: "card-pad" },
        el("p", { style: { color: "var(--text-secondary)", fontSize: "13px" } },
          sev === "ok"
            ? "The agent is reachable on the network. Continue to choose a cloud posture."
            : "No usable network detected yet. Bring up a hotspot, plug in a USB tether, or join a Wi-Fi network."),
      ),
      el("div", { className: "dl-rows" },
        dlRow("Hostname", n.hostname),
        dlRow("mDNS", n.mdns_host),
        dlRow("Hotspot", n.hotspot_enabled ? `Enabled (${n.hotspot_ssid || "—"})` : "Disabled"),
        dlRow("Local IPs", (n.local_ips || []).join(", ")),
      ),
    ),
  });
}

function renderMavlinkStepInline(status) {
  const m = status.mavlink || {};
  return card({
    title: "Flight controller",
    severity: severityForMavlink(m),
    body: el("div", {},
      el("div", { className: "card-pad" },
        el("p", { style: { color: "var(--text-secondary)", fontSize: "13px", marginBottom: "12px" } },
          m.connected
            ? `Connected on ${m.port || "?"} at ${m.baud || "?"} baud.`
            : "No flight controller is currently connected. Power the FC, plug in the USB or UART cable, and refresh."),
        el("div", { className: "btn-row" }, btn("Open MAVLink", { href: "/mavlink.html" })),
      ),
      el("div", { className: "dl-rows" },
        dlRow("Port", pretty(m.port)),
        dlRow("Baud", pretty(m.baud)),
        dlRow("WebSocket", m.websocket_url),
      ),
    ),
  });
}

function renderVideoStepInline(status) {
  const v = status.video || {};
  return card({
    title: "Video pipeline",
    severity: severityForVideo(status),
    body: el("div", {},
      el("div", { className: "card-pad" },
        el("p", { style: { color: "var(--text-secondary)", fontSize: "13px", marginBottom: "12px" } },
          v.state === "running"
            ? "Pipeline is running. WHEP video is available for Mission Control."
            : "No camera or receiver detected. Skip if you do not need video on this device."),
        el("div", { className: "btn-row" }, btn("Open Video", { href: "/video.html" })),
      ),
      el("div", { className: "dl-rows" },
        dlRow("State", pretty(v.state)),
        dlRow("WHEP URL", v.whep_url),
        dlRow("Recording", v.recording ? "On" : "Off"),
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
  return card({
    title: "Remote access (optional)",
    severity: severityForRemote(status),
    body: el("div", { className: "card-pad" },
      el("p", { style: { color: "var(--text-secondary)", fontSize: "13px", marginBottom: "12px" } },
        "Cloudflare Tunnel exposes this agent to remote support without opening router ports. Skip if you do not need external access."),
      el("div", { className: "btn-row" }, btn("Open Remote access", { href: "/remote.html", variant: "primary" })),
    ),
  });
}

function renderPairStep(status) {
  const cc = status.cloud_choice || {};
  if (cc.mode === "local") {
    return card({
      title: "Pairing not required",
      body: el("div", { className: "card-pad" },
        el("p", { style: { color: "var(--text-secondary)", fontSize: "13px" } },
          "Local-only mode is active. Mission Control connects directly over the LAN; no pairing code is needed."),
      ),
    });
  }
  return card({
    title: "Pair with Mission Control",
    body: el("div", { className: "card-pad" },
      el("p", { style: { color: "var(--text-secondary)", fontSize: "13px", marginBottom: "12px" } },
        cc.paired
          ? "This device is already paired with a Mission Control account."
          : "Open Mission Control, go to Settings → Devices → Add device, copy the pairing code, and enter it on this device."),
      el("p", { style: { color: "var(--text-tertiary)", fontSize: "12px" } },
        "Pairing entry is exposed through the agent CLI (`ados status` shows the pairing flow) and through Mission Control's Hardware tab. The wizard surfaces this step so the order of operations is clear; you can pair from any of those surfaces."),
    ),
  });
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

function bootstrap() {
  const page = (document.body && document.body.dataset && document.body.dataset.page) || "dashboard";
  (PAGES[page] || renderDashboard)();
  startPolling();
}

if (document.readyState === "loading") {
  document.addEventListener("DOMContentLoaded", bootstrap, { once: true });
} else {
  bootstrap();
}
