// Profile section. Drone / ground_station radio + ground role select.
// Bound to the per-section dirty tracker; the index module collects the
// payload and posts it through /api/v1/setup/apply.

import { el } from "../../components.js";
import { createDirtyTracker } from "./_dirty.js";

const ROLES = ["direct", "relay", "receiver"];

function profileOf(status) {
  if (!status) return "drone";
  return status.profile || status.detected_profile || "drone";
}

function roleOf(status) {
  if (!status) return "direct";
  return status.ground_role || "direct";
}

export function renderProfileSection({ store, onChange }) {
  const status = store.get().status || {};
  const initial = {
    profile: profileOf(status),
    ground_role: roleOf(status),
  };

  const tracker = createDirtyTracker(initial, () => {
    notify();
    repaint();
  });

  const notify = typeof onChange === "function" ? onChange : () => {};

  const root = el("div", { className: "section-body" });

  const repaint = () => {
    const profile = tracker.read("profile");
    const role = tracker.read("ground_role");

    const radio = (value, label) => el("label", { className: "radio-row" },
      el("input", {
        type: "radio",
        name: "profile",
        value,
        checked: value === profile,
        onchange: (ev) => {
          if (ev.currentTarget.checked) tracker.set("profile", value);
        },
      }),
      el("span", { className: "radio-label", text: label }),
    );

    const roleSelect = el("select", {
      className: "select",
      disabled: profile !== "ground_station",
      onchange: (ev) => tracker.set("ground_role", ev.currentTarget.value),
    },
      ...ROLES.map((r) => el("option", {
        value: r,
        selected: r === role,
        text: r,
      })),
    );

    root.replaceChildren(
      el("p", { className: "section-hint text-faint", text: "active profile" }),
      el("div", { className: "radio-group" },
        radio("drone", "drone"),
        radio("ground_station", "ground station"),
      ),
      el("label", { className: "field" },
        el("span", { className: "field-label", text: "ground role" }),
        roleSelect,
      ),
    );
  };

  repaint();

  return {
    node: root,
    tracker,
    payload: () => {
      const diff = tracker.payload();
      if (!Object.keys(diff).length) return null;
      const profile = tracker.read("profile");
      const out = { profile };
      if (profile === "ground_station") {
        out.ground_role = tracker.read("ground_role");
      }
      return out;
    },
    reset: () => {
      tracker.reset();
      repaint();
      notify();
    },
  };
}
