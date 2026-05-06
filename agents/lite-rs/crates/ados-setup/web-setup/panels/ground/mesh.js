// Mesh panel. batman-adv role badge, peer table, gateway node, partition
// state. Reads store.state.dashboard.mesh. Active for relay and receiver
// roles. Direct role gets an empty-state card so the panel still renders
// when a role is mid-flip.

import { el, panel } from "../../components.js";
import { pick, safeArr, fmtDur } from "../_util.js";

const ROLE_VARIANT = {
  direct: "idle",
  relay: "info",
  receiver: "ok",
};

function partitionSeverity(state) {
  if (!state) return "idle";
  const s = String(state).toLowerCase();
  if (s.includes("partition") || s === "isolated" || s === "split") return "err";
  if (s === "degraded" || s === "merging") return "warn";
  if (s === "joined" || s === "stable" || s === "healthy") return "ok";
  return "idle";
}

function linkQualitySeverity(lq) {
  if (lq == null) return "idle";
  if (lq < 100) return "err";
  if (lq < 180) return "warn";
  return "ok";
}

function ageString(ms) {
  if (ms == null) return "-";
  const seconds = Number(ms) / 1000;
  return `${fmtDur(seconds)} ago`;
}

function peerRow(peer) {
  const mac = String(pick(peer, "mac", "?"));
  const lq = pick(peer, "link_quality", null);
  const last = pick(peer, "last_seen_ms", null);
  const sev = linkQualitySeverity(lq);

  return el("div", { className: "panel__row" },
    el("span", { className: "panel__row-label mono" },
      el("span", { className: `dot dot--${sev}`, "aria-hidden": "true" }),
      el("span", { text: ` ${mac}` }),
    ),
    el("span", { className: "panel__row-value mono", text: `lq ${lq != null ? lq : "-"} · ${ageString(last)}` }),
  );
}

function row(label, value, severity) {
  const v = el("span", { className: "panel__row-value mono", text: value != null ? String(value) : "-" });
  if (severity) v.classList.add(`text--${severity}`);
  return el("div", { className: "panel__row" },
    el("span", { className: "panel__row-label", text: label }),
    v,
  );
}

export function renderMeshPanel(store, opts = {}) {
  const body = el("div", { className: "panel-stack" });
  const node = panel({
    title: "mesh",
    span: 6,
    expandable: true,
    severity: "idle",
    body,
  });

  const rerender = () => {
    const state = store.get();
    const mesh = pick(state, "dashboard.mesh", null);
    const role = String(pick(mesh, "role", pick(state, "status.ground_role", "direct")) || "direct").toLowerCase();
    const peers = safeArr(pick(mesh, "batman_peers", null));
    const gateway = pick(mesh, "gateway_node", null);
    const partition = pick(mesh, "partition_state", null);
    const meshAddr = pick(mesh, "mesh_addr", null);

    const partSev = partitionSeverity(partition);
    const overall = partSev === "err" ? "err" : (partSev === "warn" ? "warn" : (peers.length > 0 ? "ok" : "idle"));

    const dot = node.querySelector(".status-dot");
    if (dot) dot.className = `status-dot status-dot--${overall}`;

    const roleVariant = ROLE_VARIANT[role] || "idle";
    const roleChip = el("span", { className: `pill pill--${roleVariant} pill--solid`, text: role.toUpperCase() });
    const partChip = partition ? el("span", { className: `pill pill--${partSev}`, text: String(partition) }) : null;
    const head = el("div", { className: "panel-chip-row" }, roleChip, partChip);

    if (role === "direct") {
      body.replaceChildren(head, el("p", { className: "panel-empty text-faint", text: "Not in a mesh" }));
      return;
    }

    const summary = [
      row("mesh addr", meshAddr ? String(meshAddr) : "-"),
      row("gateway", gateway ? String(gateway) : "-"),
      row("peers", peers.length > 0 ? `${peers.length}` : "0", peers.length > 0 ? "ok" : "idle"),
    ];

    const peerHead = el("div", { className: "panel__row mono", style: { color: "var(--text-dim)", fontSize: "11px" } },
      el("span", { className: "panel__row-label", text: "peer · link quality" }),
      el("span", { className: "panel__row-value", text: "last seen" }),
    );

    const peerRows = peers.length
      ? peers.slice(0, 8).map(peerRow)
      : [el("p", { className: "panel-empty text-faint", text: "no peers" })];

    body.replaceChildren(head, ...summary, peerHead, ...peerRows);
  };

  const unsub = store.subscribe(rerender);
  rerender();

  if (opts.palette && opts.palette.registry) {
    opts.palette.registry.register("mesh.tail_logs", {
      label: "mesh: tail logs",
      verb: "tail",
      action: () => {
        if (opts.router) opts.router.navigate("/logs/ados-batman");
      },
    });
  }

  return {
    node,
    dispose: () => {
      try { unsub && unsub(); } catch { /* noop */ }
      if (opts.palette && opts.palette.registry) {
        try { opts.palette.registry.unregister("mesh.tail_logs"); } catch { /* noop */ }
      }
    },
  };
}
