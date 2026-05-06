// Advanced section. Log level dropdown, board override input, and a
// confirm-checkbox-gated factory reset. The reset is queued only;
// the agent does not actually wipe state in this iteration.

import { el } from "../../components.js";
import { createDirtyTracker } from "./_dirty.js";

const LOG_LEVELS = ["debug", "info", "warning", "error", "critical"];

function readAdvanced(status) {
  const agent = status.agent || {};
  return {
    log_level: agent.log_level || "info",
    board_override: status.board_override || "",
    factory_reset: false,
  };
}

export function renderAdvancedSection({ store, onChange }) {
  const status = store.get().status || {};
  const initial = readAdvanced(status);
  const tracker = createDirtyTracker(initial, () => {
    notify();
    repaint();
  });
  const notify = typeof onChange === "function" ? onChange : () => {};

  const root = el("div", { className: "section-body" });

  const repaint = () => {
    const logSelect = el("select", {
      className: "select",
      onchange: (ev) => tracker.set("log_level", ev.currentTarget.value),
    },
      ...LOG_LEVELS.map((lvl) => el("option", {
        value: lvl,
        selected: lvl === tracker.read("log_level"),
        text: lvl,
      })),
    );

    const boardInput = el("input", {
      type: "text",
      className: "input",
      value: tracker.read("board_override") || "",
      placeholder: "rock-5c-lite",
      oninput: (ev) => tracker.set("board_override", ev.currentTarget.value.trim()),
    });

    const resetToggle = el("label", { className: "toggle-row toggle-row--danger" },
      el("input", {
        type: "checkbox",
        checked: !!tracker.read("factory_reset"),
        onchange: (ev) => tracker.set("factory_reset", !!ev.currentTarget.checked),
      }),
      el("span", { className: "toggle-label", text: "queue factory reset on next reboot" }),
    );

    root.replaceChildren(
      el("p", { className: "section-hint text-faint", text: "advanced" }),
      el("label", { className: "field" },
        el("span", { className: "field-label", text: "log level" }),
        logSelect,
      ),
      el("label", { className: "field" },
        el("span", { className: "field-label", text: "board override" }),
        boardInput,
      ),
      resetToggle,
    );
  };

  repaint();

  return {
    node: root,
    tracker,
    payload: () => {
      const diff = tracker.payload();
      if (!Object.keys(diff).length) return null;
      const out = {};
      if ("log_level" in diff) out.log_level = diff.log_level;
      if ("board_override" in diff) out.board_override = diff.board_override;
      if ("factory_reset" in diff) out.factory_reset = !!diff.factory_reset;
      return out;
    },
    reset: () => {
      tracker.reset();
      repaint();
      notify();
    },
  };
}
