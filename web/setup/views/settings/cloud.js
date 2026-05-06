// Cloud section placeholder. Cloud relay choice + pairing code edit lands
// in a later iteration.

import { el } from "../../components.js";

export function renderCloudSection({ store }) {
  const status = store.get().status || {};
  const pair = status.pairing_code || status.pair || "-";
  return el("div", { className: "section-body" },
    el("p", { className: "section-hint text-faint", text: "cloud relay" }),
    el("dl", { className: "kv" },
      el("dt", { text: "pairing" }),
      el("dd", { className: "mono", text: pair }),
      el("dt", { text: "relay" }),
      el("dd", { className: "mono", text: status.cloud_relay || "altnautica" }),
    ),
    el("p", { className: "section-note text-faint", text: "edit + apply lands in a later iteration." }),
  );
}
