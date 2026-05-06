// Display section placeholder. SPI LCD overlay install lands in a later
// iteration; for now it surfaces detected state from the status snapshot.

import { el } from "../../components.js";

export function renderDisplaySection({ store }) {
  const status = store.get().status || {};
  const display = status.display || status.local_display || null;
  return el("div", { className: "section-body" },
    el("p", { className: "section-hint text-faint", text: "local display" }),
    el("dl", { className: "kv" },
      el("dt", { text: "device" }),
      el("dd", { className: "mono", text: (display && display.kind) || "none detected" }),
      el("dt", { text: "resolution" }),
      el("dd", { className: "mono", text: (display && display.resolution) || "-" }),
    ),
    el("p", { className: "section-note text-faint", text: "overlay install + apply lands in a later iteration." }),
  );
}
