// Advanced section placeholder. Board override, logging, diagnostics,
// factory reset, and a re-run wizard link. Wizard ports + apply lands in a
// later iteration.

import { el } from "../../components.js";

export function renderAdvancedSection({ store }) {
  const status = store.get().status || {};
  return el("div", { className: "section-body" },
    el("p", { className: "section-hint text-faint", text: "advanced" }),
    el("dl", { className: "kv" },
      el("dt", { text: "board" }),
      el("dd", { className: "mono", text: status.board || "-" }),
      el("dt", { text: "agent" }),
      el("dd", { className: "mono", text: status.agent_version || status.version || "-" }),
    ),
    el("p", { className: "section-note text-faint", text: "wizard ports, board override, factory reset, diagnostics land in a later iteration." }),
  );
}
