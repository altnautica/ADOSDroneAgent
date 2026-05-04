// Universal setup webapp.
// Renders one shared shell per HTML page and dispatches to a page renderer
// based on document.body.dataset.page. All API data is rendered with
// textContent or DOM creation; user-supplied strings are never assigned to
// innerHTML.

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
  { id: "system", href: "/system.html", label: "System & logs" },
  { id: "advanced", href: "/advanced.html", label: "Advanced" },
];

let currentStatus = null;
let pollTimer = null;
const subscribers = new Set();

function el(tag, props = {}, children = []) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(props || {})) {
    if (value == null || value === false) continue;
    if (key === "className") node.className = value;
    else if (key === "text") node.textContent = value;
    else if (key === "dataset") Object.assign(node.dataset, value);
    else if (key === "onclick") node.addEventListener("click", value);
    else if (key === "onsubmit") node.addEventListener("submit", value);
    else if (key === "oninput") node.addEventListener("input", value);
    else if (key.startsWith("aria-") || key === "role") node.setAttribute(key, value);
    else node[key] = value;
  }
  const list = Array.isArray(children) ? children : [children];
  for (const child of list) {
    if (child == null || child === false) continue;
    if (typeof child === "string" || typeof child === "number") {
      node.appendChild(document.createTextNode(String(child)));
    } else {
      node.appendChild(child);
    }
  }
  return node;
}

function badge(state, label) {
  const cls =
    state === "complete" || state === "running" || state === "ok"
      ? "ok"
      : state === "needs_action" || state === "stopped" || state === "warn" || state === "configured"
        ? "warn"
        : state === "blocked" || state === "error" || state === "danger"
          ? "danger"
          : "";
  return el("span", { className: `badge ${cls}`.trim() }, label || prettyState(state));
}

function prettyState(state) {
  if (!state) return "unknown";
  if (state === "needs_action") return "needs setup";
  if (state === "complete") return "ready";
  return String(state).replace(/_/g, " ");
}

function metric(label, value) {
  return el("div", { className: "metric" }, [
    el("span", {}, label),
    el("strong", {}, value == null || value === "" ? "—" : String(value)),
  ]);
}

function row(dt, dd) {
  return el("div", { className: "row" }, [
    el("dt", {}, dt),
    el("dd", {}, dd == null || dd === "" ? "—" : String(dd)),
  ]);
}

function panel({ title, subtitle, actions, children }) {
  const head = title || subtitle || actions
    ? el("header", { className: "panel-head" }, [
        el("div", {}, [
          title ? el("h2", {}, title) : null,
          subtitle ? el("p", {}, subtitle) : null,
        ]),
        actions ? el("div", { className: "inline-actions" }, actions) : null,
      ])
    : null;
  return el("section", { className: "panel" }, [head, ...(Array.isArray(children) ? children : [children])]);
}

function pageHeader(kicker, title, description, actions) {
  return el("header", { className: "page-header" }, [
    el("div", { className: "page-title" }, [
      el("div", { className: "page-kicker" }, kicker),
      el("h1", {}, title),
      description ? el("p", { className: "muted" }, description) : null,
    ]),
    actions ? el("div", { className: "header-actions" }, actions) : null,
  ]);
}

function copyButton(value) {
  const button = el("button", { className: "button", type: "button" }, "Copy");
  button.addEventListener("click", async () => {
    try {
      await navigator.clipboard.writeText(value);
      button.textContent = "Copied";
      setTimeout(() => {
        button.textContent = "Copy";
      }, 1200);
    } catch {
      button.textContent = "Copy failed";
      setTimeout(() => {
        button.textContent = "Copy";
      }, 1500);
    }
  });
  return button;
}

function openButton(value, label = "Open") {
  return el("a", {
    className: "button primary",
    href: value,
    target: "_blank",
    rel: "noopener noreferrer",
  }, label);
}

async function apiFetch(path, init = {}) {
  const headers = new Headers(init.headers || {});
  const token = sessionStorage.getItem(SETUP_TOKEN_KEY);
  if (token) headers.set("X-ADOS-Setup-Token", token);
  if (init.body && !headers.has("Content-Type")) headers.set("Content-Type", "application/json");
  const res = await fetch(path, { ...init, headers });
  const ct = res.headers.get("content-type") || "";
  if (!res.ok) {
    let detail = `${res.status} ${res.statusText}`;
    if (ct.includes("application/json")) {
      try {
        const body = await res.json();
        if (body && typeof body.detail === "string") detail = body.detail;
        else detail = JSON.stringify(body);
      } catch {
        // ignore
      }
    } else {
      const text = await res.text().catch(() => "");
      if (text) detail = `${detail} ${text}`.trim();
    }
    throw new Error(detail);
  }
  if (ct.includes("application/json")) return res.json();
  return res.text();
}

async function loadStatus() {
  try {
    const data = await apiFetch("/api/v1/setup/status");
    currentStatus = data;
    subscribers.forEach((fn) => fn(data));
  } catch (err) {
    console.error("Failed to load setup status:", err);
  }
}

function subscribe(fn) {
  subscribers.add(fn);
  if (currentStatus) fn(currentStatus);
  return () => subscribers.delete(fn);
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

function findUrl(status, predicate) {
  if (!status?.access_urls) return null;
  return status.access_urls.find(predicate) || null;
}

function setupSummaryUrls(status) {
  const out = {
    primarySetup: findUrl(status, (u) => u.kind === "setup" && u.primary)?.url,
    hotspot: findUrl(status, (u) => u.source === "hotspot")?.url,
    usb: findUrl(status, (u) => u.source === "usb")?.url,
    mdns: findUrl(status, (u) => u.source === "mdns")?.url,
    api: findUrl(status, (u) => u.kind === "api")?.url,
    missionControl: findUrl(status, (u) => u.kind === "mission_control")?.url,
    tunnelSetup: findUrl(status, (u) => u.kind === "setup" && u.source === "cloud")?.url,
  };
  out.lan = (status?.access_urls || []).filter((u) => u.kind === "setup" && u.source === "local" && !u.primary);
  return out;
}

function renderShell(page, content) {
  const root = document.getElementById("app");
  if (!root) return;
  root.replaceChildren(
    el("div", { className: "app-shell" }, [
      renderSidebar(page),
      renderMobileBar(),
      el("div", {
        className: "sidebar-backdrop",
        onclick: () => document.body.classList.remove("menu-open"),
      }),
      el("main", { className: "content" }, content),
    ])
  );
}

function renderSidebar(page) {
  const completion =
    currentStatus && typeof currentStatus.completion_percent === "number"
      ? `${currentStatus.completion_percent}%`
      : "—";
  return el("aside", { className: "sidebar", "aria-label": "Setup navigation" }, [
    el("div", { className: "brand" }, [
      el("div", { className: "brand-mark", "aria-hidden": "true" }, [
        el("img", { src: "/brand.svg", alt: "" }),
      ]),
      el("div", { className: "brand-title" }, [
        el("strong", {}, "ADOS Setup"),
        el("span", {}, currentStatus?.device_name || "Drone Agent"),
      ]),
    ]),
    el(
      "nav",
      { className: "nav" },
      NAV.map((item) =>
        el(
          "a",
          {
            className: "nav-link" + (item.id === page ? " active" : ""),
            href: item.href,
          },
          [
            el("span", {}, item.label),
            item.id === page ? el("small", {}, completion) : null,
          ]
        )
      )
    ),
    el("div", { className: "sidebar-footer" }, [
      el(
        "div",
        { className: "sidebar-meta" },
        currentStatus ? `Agent v${currentStatus.version || "?"}` : "Drone Agent"
      ),
      currentStatus
        ? el("div", { className: "sidebar-meta" }, `Profile: ${prettyState(currentStatus.profile)}`)
        : null,
    ]),
  ]);
}

function renderMobileBar() {
  return el("div", { className: "mobile-bar" }, [
    el(
      "button",
      {
        className: "menu-button",
        type: "button",
        "aria-label": "Toggle navigation",
        onclick: () => document.body.classList.toggle("menu-open"),
      },
      "Menu"
    ),
    el("strong", {}, "ADOS Setup"),
    el(
      "small",
      { className: "muted" },
      currentStatus ? `${currentStatus.completion_percent || 0}%` : "—"
    ),
  ]);
}

// ---------------------------- page renderers ---------------------------------

function priorityBanner(status) {
  const completion = status?.completion_percent ?? 0;
  const nextAction =
    status?.next_action || (status?.setup_complete ? "Setup is complete" : "Loading…");
  return el("section", { className: "panel priority panel-pad" }, [
    el("div", {}, [
      el("div", { className: "page-kicker" }, status?.setup_complete ? "Status" : "Next action"),
      el("h2", {}, nextAction),
      status?.steps?.length
        ? (() => {
            const next = status.steps.find((s) => s.state === "needs_action");
            if (!next) return null;
            return el("div", { className: "inline-actions" }, [
              next.href
                ? el(
                    "a",
                    { className: "button primary", href: next.href },
                    next.action_label || "Continue setup"
                  )
                : null,
            ]);
          })()
        : null,
    ]),
    el("div", { className: "progress" }, [
      el("strong", {}, `${completion}%`),
      el("span", { className: "progress-track" }, [
        el("span", { className: "progress-fill", style: `width:${completion}%` }),
      ]),
    ]),
  ]);
}

function renderDashboard() {
  const update = (status) => {
    if (!status) {
      renderShell("dashboard", [el("p", { className: "empty" }, "Loading status…")]);
      return;
    }
    const urls = setupSummaryUrls(status);
    const services = status.services || [];
    const telemetry = status.telemetry || {};

    renderShell("dashboard", [
      pageHeader("Dashboard", "Drone Agent", `Device ${status.device_name || ""}`, [
        urls.primarySetup ? openButton(urls.primarySetup, "Open setup link") : null,
      ]),
      el("div", { className: "page" }, [
        priorityBanner(status),

        el("div", { className: "grid cols-3" }, [
          metric("Setup", `${status.completion_percent || 0}%`),
          metric("MAVLink", status.mavlink?.connected ? "Connected" : "Not connected"),
          metric("Video", prettyState(status.video?.state)),
          metric("Remote access", prettyState(status.remote_access?.status)),
          metric("Profile", prettyState(status.profile)),
          metric("Services", services.length ? `${services.length} active` : "—"),
        ]),

        panel({
          title: "Access links",
          subtitle: "Open the setup webapp from any reachable network.",
          children: el(
            "div",
            { className: "link-list" },
            (status.access_urls || [])
              .filter((u) => u.kind === "setup" || u.kind === "mission_control" || u.kind === "api")
              .map((u) =>
                el(
                  "a",
                  {
                    className: "link-row",
                    href: u.url,
                    target: "_blank",
                    rel: "noopener noreferrer",
                  },
                  [
                    el("strong", {}, u.label),
                    el("code", {}, u.url),
                    el("small", { className: "muted" }, u.source),
                  ]
                )
              )
          ),
        }),

        el("div", { className: "grid cols-2" }, [
          panel({
            title: "MAVLink",
            subtitle: status.mavlink?.connected
              ? `Connected on ${status.mavlink.port || "?"} @ ${status.mavlink.baud || "?"} baud`
              : "Flight controller not connected",
            actions: [
              el("a", { className: "button", href: "/mavlink.html" }, "Open MAVLink"),
            ],
            children: el("dl", { className: "row-list" }, [
              row("Port", status.mavlink?.port || "—"),
              row("Baud", status.mavlink?.baud || "—"),
              row("WebSocket", status.mavlink?.websocket_url || "—"),
              status.mavlink?.public_websocket_url
                ? row("Tunnel WebSocket", status.mavlink.public_websocket_url)
                : null,
            ]),
          }),

          panel({
            title: "Video",
            subtitle: `Pipeline: ${prettyState(status.video?.state)}`,
            actions: [el("a", { className: "button", href: "/video.html" }, "Open Video")],
            children: el("dl", { className: "row-list" }, [
              row("WHEP URL", status.video?.whep_url || "—"),
              row("Recording", status.video?.recording ? "Yes" : "No"),
              status.video?.public_whep_url
                ? row("Tunnel WHEP", status.video.public_whep_url)
                : null,
            ]),
          }),
        ]),

        el("div", { className: "grid cols-2" }, [
          panel({
            title: "Network",
            actions: [el("a", { className: "button", href: "/network.html" }, "Open Network")],
            children: el("dl", { className: "row-list" }, [
              row("Hostname", status.network?.hostname || "—"),
              row("mDNS", status.network?.mdns_host || "—"),
              row(
                "Hotspot",
                status.network?.hotspot_enabled
                  ? `Enabled (${status.network?.hotspot_ssid || "—"})`
                  : "Disabled"
              ),
              row("Local IPs", (status.network?.local_ips || []).join(", ") || "—"),
            ]),
          }),

          panel({
            title: "Remote access",
            actions: [el("a", { className: "button", href: "/remote.html" }, "Configure")],
            children: el("dl", { className: "row-list" }, [
              row("Provider", prettyState(status.remote_access?.provider)),
              row("Status", prettyState(status.remote_access?.status)),
              status.remote_access?.public_urls?.length
                ? row("Public URLs", status.remote_access.public_urls.join(", "))
                : null,
              status.remote_access?.error
                ? row("Error", status.remote_access.error)
                : null,
            ]),
          }),
        ]),

        services.length
          ? panel({
              title: "Services",
              children: el(
                "div",
                { className: "service-list" },
                services.map((svc) =>
                  el("div", { className: "service-row" }, [
                    el("strong", {}, svc.name || "—"),
                    el("span", { className: "muted" }, prettyState(svc.state)),
                    badge(svc.state, prettyState(svc.state)),
                  ])
                )
              ),
            })
          : null,

        Object.keys(telemetry).length
          ? panel({
              title: "Telemetry",
              children: el(
                "div",
                { className: "grid cols-3" },
                Object.entries(telemetry).slice(0, 6).map(([key, value]) =>
                  metric(prettyState(key), formatTelemetryValue(value))
                )
              ),
            })
          : null,
      ]),
    ]);
  };

  subscribe(update);
  if (currentStatus) update(currentStatus);
  else renderShell("dashboard", [el("p", { className: "empty" }, "Loading status…")]);
}

function formatTelemetryValue(value) {
  if (value == null) return "—";
  if (typeof value === "number") return Number.isFinite(value) ? value.toFixed(2) : "—";
  if (typeof value === "boolean") return value ? "Yes" : "No";
  if (typeof value === "object") return JSON.stringify(value);
  return String(value);
}

function renderSetup() {
  subscribe((status) => {
    const steps = status?.steps || [];
    renderShell("setup", [
      pageHeader(
        "Setup",
        "Setup checklist",
        status?.next_action || "Walk through the remaining steps to bring the agent online."
      ),
      el("div", { className: "page" }, [
        priorityBanner(status),
        panel({
          title: "Steps",
          children: el(
            "div",
            { className: "step-list" },
            steps.length
              ? steps.map((step) =>
                  el(
                    "a",
                    {
                      className: "step-row",
                      href: step.href || "#",
                    },
                    [
                      el("div", {}, [
                        el("strong", {}, step.label || step.id),
                        step.detail ? el("p", {}, step.detail) : null,
                      ]),
                      badge(step.state, prettyState(step.state)),
                    ]
                  )
                )
              : [el("p", { className: "empty" }, "No setup steps reported.")]
          ),
        }),
      ]),
    ]);
  });
}

function renderMavlink() {
  subscribe((status) => {
    const mav = status?.mavlink || {};
    renderShell("mavlink", [
      pageHeader("MAVLink", "Flight controller link", mav.connected
        ? `Connected on ${mav.port || "?"} @ ${mav.baud || "?"} baud`
        : "Flight controller not connected"),
      el("div", { className: "page" }, [
        el("div", { className: "grid cols-3" }, [
          metric("State", mav.connected ? "Connected" : "Not connected"),
          metric("Port", mav.port || "—"),
          metric("Baud", mav.baud || "—"),
        ]),
        panel({
          title: "WebSocket endpoints",
          subtitle: "Use these URLs from Mission Control or any MAVLink client.",
          children: el("div", { className: "link-list" }, [
            mav.websocket_url
              ? el("div", { className: "link-row" }, [
                  el("strong", {}, "Local WebSocket"),
                  el("code", {}, mav.websocket_url),
                  el("div", { className: "inline-actions" }, [
                    copyButton(mav.websocket_url),
                    openButton(mav.websocket_url, "Open"),
                  ]),
                ])
              : el("p", { className: "empty" }, "No local WebSocket reported."),
            mav.public_websocket_url
              ? el("div", { className: "link-row" }, [
                  el("strong", {}, "Tunnel WebSocket"),
                  el("code", {}, mav.public_websocket_url),
                  el("div", { className: "inline-actions" }, [
                    copyButton(mav.public_websocket_url),
                    openButton(mav.public_websocket_url, "Open"),
                  ]),
                ])
              : null,
          ]),
        }),
        panel({
          title: "Troubleshooting",
          children: el("div", { className: "panel-pad" }, [
            el("ul", {}, [
              el("li", {}, "Check the FC is powered and the USB or UART cable is connected."),
              el("li", {}, "Confirm baud rate matches the FC firmware (typically 57600 or 115200)."),
              el("li", {}, "Open System & logs to view the last MAVLink log lines."),
            ]),
          ]),
        }),
      ]),
    ]);
  });
}

function renderVideo() {
  subscribe((status) => {
    const video = status?.video || {};
    renderShell("video", [
      pageHeader("Video", "Camera and video pipeline", `Pipeline state: ${prettyState(video.state)}`),
      el("div", { className: "page" }, [
        el("div", { className: "grid cols-3" }, [
          metric("State", prettyState(video.state)),
          metric("Recording", video.recording ? "On" : "Off"),
          metric("Profile", prettyState(status?.profile)),
        ]),
        panel({
          title: "WHEP endpoints",
          subtitle: "Open these in a browser or feed them to Mission Control's video tile.",
          children: el("div", { className: "link-list" }, [
            video.whep_url
              ? el("div", { className: "link-row" }, [
                  el("strong", {}, "Local WHEP"),
                  el("code", {}, video.whep_url),
                  el("div", { className: "inline-actions" }, [
                    copyButton(video.whep_url),
                    openButton(video.whep_url, "Open"),
                  ]),
                ])
              : el("p", { className: "empty" }, "No local WHEP URL reported."),
            video.public_whep_url
              ? el("div", { className: "link-row" }, [
                  el("strong", {}, "Tunnel WHEP"),
                  el("code", {}, video.public_whep_url),
                  el("div", { className: "inline-actions" }, [
                    copyButton(video.public_whep_url),
                    openButton(video.public_whep_url, "Open"),
                  ]),
                ])
              : null,
          ]),
        }),
      ]),
    ]);
  });
}

function renderNetwork() {
  subscribe((status) => {
    const net = status?.network || {};
    const setupUrls = (status?.access_urls || []).filter((u) => u.kind === "setup");
    renderShell("network", [
      pageHeader("Network", "Local access", "Where this agent is reachable from."),
      el("div", { className: "page" }, [
        panel({
          title: "Local network",
          children: el("dl", { className: "row-list" }, [
            row("Hostname", net.hostname || "—"),
            row("mDNS host", net.mdns_host || "—"),
            row(
              "Hotspot",
              net.hotspot_enabled ? `Enabled (${net.hotspot_ssid || "—"})` : "Disabled"
            ),
            row("Local IPs", (net.local_ips || []).join(", ") || "—"),
            row("API port", net.api_port ?? "—"),
          ]),
        }),
        panel({
          title: "Setup URLs",
          subtitle: "Pick whichever network is reachable from your phone or laptop.",
          children: el(
            "div",
            { className: "link-list" },
            setupUrls.length
              ? setupUrls.map((u) =>
                  el("div", { className: "link-row" }, [
                    el("strong", {}, u.label),
                    el("code", {}, u.url),
                    el("small", { className: "muted" }, u.source),
                    el("div", { className: "inline-actions" }, [
                      copyButton(u.url),
                      openButton(u.url, "Open"),
                    ]),
                  ])
                )
              : [el("p", { className: "empty" }, "No setup URLs reported.")]
          ),
        }),
      ]),
    ]);
  });
}

function renderRemote() {
  let lastResult = null;

  const view = (status) => {
    const remote = status?.remote_access || {};
    const formResult = el("p", { className: "form-result" }, lastResult || "");
    const textarea = el("textarea", {
      name: "token",
      placeholder: "Paste a Cloudflare tunnel token or the install command shown by Cloudflare.",
      autocomplete: "off",
      spellcheck: false,
    });

    const form = el(
      "form",
      {
        className: "form",
        onsubmit: async (event) => {
          event.preventDefault();
          const value = textarea.value.trim();
          if (!value) {
            lastResult = "Paste a token or install command first.";
            formResult.textContent = lastResult;
            return;
          }
          lastResult = "Installing…";
          formResult.textContent = lastResult;
          try {
            const result = await apiFetch("/api/v1/setup/remote-access/cloudflare", {
              method: "POST",
              body: JSON.stringify({ token_or_script: value }),
            });
            // Clear the input immediately. Never echo the token back.
            textarea.value = "";
            const message =
              result && typeof result.message === "string" ? result.message : "Token installed.";
            lastResult = message;
            formResult.textContent = lastResult;
            // Refresh status now so the UI reflects the new provider.
            await loadStatus();
          } catch (err) {
            // Still clear the input; never persist the token in the DOM.
            textarea.value = "";
            lastResult = `Failed: ${err.message || err}`;
            formResult.textContent = lastResult;
          }
        },
      },
      [
        el("label", {}, [
          "Cloudflare tunnel token or install command",
          textarea,
        ]),
        el("p", { className: "form-help" }, "The token is written to a root-owned secret file and never echoed back."),
        el("div", { className: "inline-actions" }, [
          el("button", { className: "button primary", type: "submit" }, "Install token"),
          el(
            "button",
            {
              className: "button",
              type: "button",
              onclick: () => {
                textarea.value = "";
                lastResult = "Cleared.";
                formResult.textContent = lastResult;
              },
            },
            "Clear"
          ),
        ]),
        formResult,
      ]
    );

    renderShell("remote", [
      pageHeader(
        "Remote access",
        "Optional cloud access",
        "Cloudflare Tunnel exposes this agent to remote support without opening router ports."
      ),
      el("div", { className: "page" }, [
        el("div", { className: "grid cols-3" }, [
          metric("Provider", prettyState(remote.provider)),
          metric("Status", prettyState(remote.status)),
          metric(
            "Public URLs",
            remote.public_urls?.length ? `${remote.public_urls.length} configured` : "None"
          ),
        ]),
        remote.error
          ? panel({
              title: "Issue",
              children: el("div", { className: "panel-pad" }, [
                badge("error", prettyState(remote.status)),
                el("p", { className: "muted" }, remote.error),
              ]),
            })
          : null,
        remote.public_urls?.length
          ? panel({
              title: "Public URLs",
              children: el(
                "div",
                { className: "link-list" },
                remote.public_urls.map((url) =>
                  el("div", { className: "link-row" }, [
                    el("code", {}, url),
                    el("div", { className: "inline-actions" }, [
                      copyButton(url),
                      openButton(url, "Open"),
                    ]),
                  ])
                )
              ),
            })
          : null,
        panel({
          title: "Install Cloudflare token",
          subtitle:
            "Paste the token from Cloudflare's Zero Trust dashboard or the install command Cloudflare shows you.",
          children: form,
        }),
      ]),
    ]);
  };

  subscribe(view);
}

function renderGround() {
  subscribe((status) => {
    const isGround = status?.profile === "ground_station";
    renderShell("ground", [
      pageHeader(
        "Ground station",
        isGround ? "Ground station" : "Ground station (inactive)",
        isGround
          ? "Profile-aware ground-station controls."
          : `This agent is running the ${prettyState(status?.profile)} profile.`
      ),
      el("div", { className: "page" }, [
        isGround
          ? panel({
              title: "Profile is active",
              children: el("div", { className: "panel-pad" }, [
                el("p", { className: "muted" }, "Pairing, WFB receiver, uplink and mesh controls are exposed via the agent REST API and Mission Control's Hardware tab."),
                el("div", { className: "inline-actions" }, [
                  el(
                    "a",
                    {
                      className: "button",
                      href: "/docs",
                      target: "_blank",
                      rel: "noopener noreferrer",
                    },
                    "Open API reference"
                  ),
                ]),
              ]),
            })
          : panel({
              title: "Inactive on this profile",
              children: el("div", { className: "panel-pad" }, [
                el(
                  "p",
                  { className: "muted" },
                  "Ground-station configuration is only shown when the agent profile is set to ground_station."
                ),
              ]),
            }),
      ]),
    ]);
  });
}

function renderSystem() {
  subscribe((status) => {
    const services = status?.services || [];
    renderShell("system", [
      pageHeader("System", "System & logs", "Service health and recent log lines."),
      el("div", { className: "page" }, [
        el("div", { className: "grid cols-3" }, [
          metric("Agent version", status?.version || "—"),
          metric("Profile", prettyState(status?.profile)),
          metric("Hostname", status?.network?.hostname || "—"),
        ]),
        panel({
          title: "Services",
          children: services.length
            ? el(
                "div",
                { className: "service-list" },
                services.map((svc) =>
                  el("div", { className: "service-row" }, [
                    el("strong", {}, svc.name || "—"),
                    el("span", { className: "muted" }, prettyState(svc.state)),
                    badge(svc.state, prettyState(svc.state)),
                  ])
                )
              )
            : el("p", { className: "empty" }, "No service data reported."),
        }),
        panel({
          title: "Recent logs",
          subtitle: "Tail the last log entries for the agent.",
          children: el("div", { className: "panel-pad" }, [
            el(
              "p",
              { className: "muted" },
              "Use journalctl -u ados-supervisor -n 200 on the host, or open the API reference for log endpoints."
            ),
            el("div", { className: "inline-actions" }, [
              el(
                "a",
                {
                  className: "button",
                  href: "/docs#/Logs",
                  target: "_blank",
                  rel: "noopener noreferrer",
                },
                "Open API reference"
              ),
            ]),
          ]),
        }),
      ]),
    ]);
  });
}

function renderAdvanced() {
  subscribe((status) => {
    renderShell("advanced", [
      pageHeader("Advanced", "Advanced", "Low-frequency actions for operators and support."),
      el("div", { className: "page" }, [
        panel({
          title: "API reference",
          subtitle: "Inspect the full REST surface served by this agent.",
          children: el("div", { className: "panel-pad" }, [
            el("div", { className: "inline-actions" }, [
              el(
                "a",
                {
                  className: "button primary",
                  href: "/docs",
                  target: "_blank",
                  rel: "noopener noreferrer",
                },
                "Open Swagger UI"
              ),
              el(
                "a",
                {
                  className: "button",
                  href: "/redoc",
                  target: "_blank",
                  rel: "noopener noreferrer",
                },
                "Open ReDoc"
              ),
            ]),
          ]),
        }),
        panel({
          title: "Status payload",
          subtitle: "Live JSON used to build every page in this webapp.",
          children: el(
            "pre",
            {},
            status ? JSON.stringify(status, null, 2) : "Loading status…"
          ),
        }),
      ]),
    ]);
  });
}

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
  const page = document.body?.dataset?.page || "dashboard";
  const renderer = PAGES[page] || renderDashboard;
  renderer();
  startPolling();
}

if (document.readyState === "loading") {
  document.addEventListener("DOMContentLoaded", bootstrap, { once: true });
} else {
  bootstrap();
}
