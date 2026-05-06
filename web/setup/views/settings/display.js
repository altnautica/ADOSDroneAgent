// Display section. Dropdown of supported displays from
// /api/v1/setup/display/options. The dropdown is populated lazily
// on first render so the settings sheet is responsive even on a
// slow boot.

import { el } from "../../components.js";
import { apiFetch } from "../../state.js";
import { createDirtyTracker } from "./_dirty.js";

function readDisplay(status) {
  const display = status.display || status.local_display || {};
  return {
    display_id: display.id || display.display_id || "",
  };
}

export function renderDisplaySection({ store, onChange }) {
  const status = store.get().status || {};
  const initial = readDisplay(status);
  const tracker = createDirtyTracker(initial, () => {
    notify();
    repaint();
  });
  const notify = typeof onChange === "function" ? onChange : () => {};

  const root = el("div", { className: "section-body" });
  let options = [{ id: "", label: "loading…" }];

  const repaint = () => {
    const select = el("select", {
      className: "select",
      onchange: (ev) => tracker.set("display_id", ev.currentTarget.value),
    },
      ...options.map((opt) => el("option", {
        value: opt.id || "",
        selected: (opt.id || "") === (tracker.read("display_id") || ""),
        text: opt.label || opt.id || "(unnamed)",
      })),
    );

    root.replaceChildren(
      el("p", { className: "section-hint text-faint", text: "local display" }),
      el("label", { className: "field" },
        el("span", { className: "field-label", text: "device" }),
        select,
      ),
    );
  };

  repaint();

  // Pull the supported list once. A failure leaves the placeholder
  // option in place so the user can still see what's wrong.
  apiFetch("/api/v1/setup/display/options")
    .then((data) => {
      const list = Array.isArray(data?.supported) ? data.supported : [];
      options = list.map((d) => ({ id: d.id, label: d.label || d.id }));
      if (data?.current?.display_id) {
        const id = data.current.display_id;
        if (initial.display_id !== id) {
          initial.display_id = id;
          tracker.reset();
        }
      }
      repaint();
    })
    .catch(() => {
      options = [{ id: "", label: "could not load options" }];
      repaint();
    });

  return {
    node: root,
    tracker,
    payload: () => {
      const diff = tracker.payload();
      if (!Object.keys(diff).length) return null;
      return { display_id: tracker.read("display_id") || "" };
    },
    reset: () => {
      tracker.reset();
      repaint();
      notify();
    },
  };
}
