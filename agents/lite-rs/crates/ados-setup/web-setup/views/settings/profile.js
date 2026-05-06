// Profile section. Drone / ground_station radio. Wiring to apply lands in
// a later iteration; for now the controls reflect current state read-only.

import { el } from "../../components.js";

function profileOf(status) {
  if (!status) return null;
  return status.profile || status.detected_profile || null;
}

export function renderProfileSection({ store }) {
  const current = profileOf(store.get().status) || "drone";

  const radio = (value, label) => el("label", { className: "radio-row" },
    el("input", { type: "radio", name: "profile", value, checked: value === current, disabled: true }),
    el("span", { className: "radio-label", text: label }),
  );

  return el("div", { className: "section-body" },
    el("p", { className: "section-hint text-faint", text: "active profile" }),
    el("div", { className: "radio-group" },
      radio("drone", "drone"),
      radio("ground_station", "ground station"),
    ),
    el("p", { className: "section-note text-faint", text: "switch + apply lands in a later iteration." }),
  );
}
