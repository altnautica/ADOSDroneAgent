// Services panel. Per-service active state, cpu %, RSS MB, "tail logs"
// button. Reads status.services (the existing setup snapshot already
// returns a list). A "failed only" filter checkbox folds the table down
// to just unhealthy services for at-a-glance triage.

import { el, panel } from "../../components.js";
import { pick, fmtNum, fmtBytes, safeArr, severityFromState } from "../_util.js";

function toBytes(svc) {
  // Accept either rss_bytes or rss_mb to be defensive against schema drift.
  const b = pick(svc, "rss_bytes", null);
  if (b != null) return Number(b);
  const mb = pick(svc, "rss_mb", null);
  if (mb != null) return Number(mb) * 1024 * 1024;
  return null;
}

function svcSeverity(svc) {
  const explicit = pick(svc, "severity", null);
  if (explicit) return severityFromState(explicit);
  return severityFromState(pick(svc, "active_state", pick(svc, "state", null)));
}

function svcRow(svc, onTail) {
  const sev = svcSeverity(svc);
  const name = String(svc.name || svc.unit || "?");
  const state = pick(svc, "active_state", pick(svc, "state", "-"));
  const cpu = pick(svc, "cpu_pct", null);
  const rss = toBytes(svc);

  return el("div", { className: "service-row panel__row" },
    el("span", { className: "service-row-name" },
      el("span", { className: `dot dot--${sev}`, "aria-hidden": "true" }),
      el("span", { className: "mono", text: ` ${name}` }),
    ),
    el("span", { className: "service-row-state mono text-faint", text: String(state) }),
    el("span", { className: "service-row-cpu mono", text: cpu != null ? `cpu ${fmtNum(cpu, 1)}%` : "cpu -" }),
    el("span", { className: "service-row-rss mono", text: rss != null ? fmtBytes(rss) : "-" }),
    el("button", {
      type: "button",
      className: "btn btn--sm btn--ghost",
      text: "tail logs",
      onclick: () => onTail(name),
    }),
  );
}

export function renderServicesPanel(store, opts = {}) {
  let failedOnly = false;

  const body = el("div", { className: "panel-stack" });
  const node = panel({
    title: "services",
    span: 12,
    expandable: true,
    severity: "idle",
    body,
  });

  const onTail = (name) => {
    if (opts.router) opts.router.navigate(`/logs/${encodeURIComponent(name)}`);
  };

  const filterLabel = el("label", { className: "service-filter" },
    el("input", {
      type: "checkbox",
      onchange: (ev) => { failedOnly = !!ev.target.checked; rerender(); },
    }),
    el("span", { className: "mono text-faint", text: " failed only" }),
  );

  const rerender = () => {
    const state = store.get();
    const all = safeArr(pick(state, "status.services", null));

    let worst = "idle";
    for (const s of all) {
      const sev = svcSeverity(s);
      if (sev === "err") worst = "err";
      else if (sev === "warn" && worst !== "err") worst = "warn";
      else if (sev === "ok" && worst === "idle") worst = "ok";
    }
    const dot = node.querySelector(".status-dot");
    if (dot) dot.className = `status-dot status-dot--${worst}`;

    const list = failedOnly
      ? all.filter((s) => { const sev = svcSeverity(s); return sev === "err" || sev === "warn"; })
      : all;

    const head = el("div", { className: "service-controls" },
      filterLabel,
      el("span", { className: "text-faint mono", text: ` ${list.length}/${all.length}` }),
    );

    if (!list.length) {
      body.replaceChildren(head, el("p", { className: "panel-empty text-faint", text: failedOnly ? "no failed services" : "no services" }));
      return;
    }

    body.replaceChildren(head, ...list.map((s) => svcRow(s, onTail)));
  };

  const unsub = store.subscribe(rerender);
  rerender();

  if (opts.palette && opts.palette.registry) {
    opts.palette.registry.register("services.tail_supervisor", {
      label: "services: tail supervisor",
      verb: "tail",
      action: () => onTail("ados-supervisor"),
    });
    opts.palette.registry.register("services.toggle_failed", {
      label: "services: toggle failed-only filter",
      verb: "filter",
      action: () => {
        failedOnly = !failedOnly;
        const cb = filterLabel.querySelector("input[type=checkbox]");
        if (cb) cb.checked = failedOnly;
        rerender();
      },
    });
  }

  return {
    node,
    dispose: () => {
      try { unsub && unsub(); } catch { /* noop */ }
      if (opts.palette && opts.palette.registry) {
        try { opts.palette.registry.unregister("services.tail_supervisor"); } catch { /* noop */ }
        try { opts.palette.registry.unregister("services.toggle_failed"); } catch { /* noop */ }
      }
    },
  };
}
