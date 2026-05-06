// Logs view. Streams the agent's log endpoint via WebSocket using
// streamConsole(). On a service-less route, lists known services to pick
// from. Mobile / tablet: full-screen route. Desktop: a 400px right-docked
// drawer alongside the dashboard.

import { el, streamConsole, chip } from "../components.js";

const KNOWN_SERVICES = [
  { id: "ados-supervisor", label: "supervisor" },
  { id: "ados-mavlink", label: "mavlink" },
  { id: "ados-cloud-relay", label: "cloud-relay" },
  { id: "ados-video", label: "video" },
  { id: "ados-api", label: "api" },
];

function logsUrl(service) {
  if (service) return `/api/v1/logs/${encodeURIComponent(service)}`;
  return "/api/logs?limit=50";
}

function picker(router) {
  const list = el("ul", { className: "logs-picker" });
  for (const s of KNOWN_SERVICES) {
    list.appendChild(el("li", null,
      el("button", {
        type: "button",
        className: "logs-picker-row",
        onclick: () => router.navigate(`/logs/${s.id}`),
      },
        el("span", { className: "mono", text: s.id }),
        el("span", { className: "text-faint", text: s.label }),
      ),
    ));
  }
  return list;
}

export function renderLogs(targetEl, { router, params }) {
  const service = params && params.service ? params.service : null;

  const head = el("header", { className: "view-head" },
    el("h1", { className: "view-title", text: service ? `logs · ${service}` : "logs" }),
    service
      ? el("button", { type: "button", className: "btn btn--ghost", text: "back", onclick: () => router.navigate("/logs") })
      : el("p", { className: "view-sub text-faint", text: "pick a service" }),
  );

  const body = el("div", { className: "view-body" });

  let console = null;
  if (service) {
    console = streamConsole({ wsUrl: logsUrl(service), height: 480 });
    body.appendChild(console);
  } else {
    body.appendChild(picker(router));
  }

  const root = el("section", { className: "view view-logs", "data-view": "logs" }, head, body);
  targetEl.replaceChildren(root);

  return {
    dispose: () => {
      if (console && typeof console.dispose === "function") console.dispose();
    },
  };
}
