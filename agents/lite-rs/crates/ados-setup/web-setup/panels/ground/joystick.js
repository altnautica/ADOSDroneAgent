// Joystick HID panel. Device, vendor, product, per-axis bars (-1 to +1),
// per-button pressed/idle chips. Reads store.state.dashboard.joystick.
// Active on every ground role. Empty state when no joystick attached.

import { el, panel } from "../../components.js";
import { pick, safeArr, fmtNum } from "../_util.js";

function clamp(v, lo, hi) {
  return Math.max(lo, Math.min(hi, v));
}

function axisBar(axis) {
  const idx = pick(axis, "idx", "?");
  const name = pick(axis, "name", `axis ${idx}`);
  const raw = pick(axis, "value", 0);
  const value = Number(raw) || 0;
  const norm = clamp(value, -1, 1);
  // Map -1..+1 onto a 0..100 fill from the center.
  const half = 50 * Math.abs(norm);
  const fillStart = norm < 0 ? 50 - half : 50;
  const fillWidth = half;

  const track = el("div", {
    style: {
      position: "relative",
      width: "120px",
      height: "10px",
      background: "var(--surface-alt, #222)",
      borderRadius: "2px",
      overflow: "hidden",
    },
  });
  const center = el("div", {
    style: {
      position: "absolute",
      left: "50%",
      top: "0",
      width: "1px",
      height: "100%",
      background: "var(--border, #444)",
    },
  });
  const fill = el("div", {
    style: {
      position: "absolute",
      left: `${fillStart}%`,
      top: "0",
      width: `${fillWidth}%`,
      height: "100%",
      background: "var(--info)",
    },
  });
  track.append(center, fill);

  return el("div", { className: "panel__row" },
    el("span", { className: "panel__row-label mono", text: String(name) }),
    el("span", { className: "panel__row-value mono", style: { display: "flex", gap: "8px", alignItems: "center" } },
      track,
      el("span", { text: fmtNum(value, 2) }),
    ),
  );
}

function buttonChip(btn) {
  const idx = pick(btn, "idx", "?");
  const pressed = !!pick(btn, "pressed", false);
  const sev = pressed ? "ok" : "idle";
  return el("span", {
    className: `pill pill--${sev} pill--sm mono`,
    title: pressed ? "pressed" : "idle",
    text: `${idx}`,
  });
}

function row(label, value) {
  return el("div", { className: "panel__row" },
    el("span", { className: "panel__row-label", text: label }),
    el("span", { className: "panel__row-value mono", text: value != null ? String(value) : "-" }),
  );
}

export function renderJoystickPanel(store, opts = {}) {
  const body = el("div", { className: "panel-stack" });
  const node = panel({
    title: "joystick",
    span: 6,
    expandable: true,
    severity: "idle",
    body,
  });

  const rerender = () => {
    const state = store.get();
    const js = pick(state, "dashboard.joystick", null);

    const device = pick(js, "device", null);
    const vendor = pick(js, "vendor", null);
    const product = pick(js, "product", null);
    const axes = safeArr(pick(js, "axes", null));
    const buttons = safeArr(pick(js, "buttons", null));

    const hasJs = device || axes.length > 0 || buttons.length > 0;
    const overall = hasJs ? "ok" : "idle";
    const dot = node.querySelector(".status-dot");
    if (dot) dot.className = `status-dot status-dot--${overall}`;

    if (!hasJs) {
      body.replaceChildren(el("p", { className: "panel-empty text-faint", text: "No joystick attached" }));
      return;
    }

    const idLine = [vendor, product].filter(Boolean).join(" · ") || "-";

    const headRows = [
      row("device", device ? String(device) : "-"),
      row("identity", idLine),
    ];

    const axisHead = axes.length
      ? el("div", { className: "panel__row mono", style: { color: "var(--text-dim)", fontSize: "11px" } },
        el("span", { className: "panel__row-label", text: "axis" }),
        el("span", { className: "panel__row-value", text: "value" }),
      )
      : null;

    const axisRows = axes.slice(0, 8).map(axisBar);

    const btnRow = buttons.length
      ? el("div", { className: "panel__row" },
        el("span", { className: "panel__row-label", text: "buttons" }),
        el("span", { className: "panel-chip-row", style: { display: "flex", gap: "4px", flexWrap: "wrap" } },
          ...buttons.slice(0, 16).map(buttonChip),
        ),
      )
      : null;

    body.replaceChildren(...headRows, axisHead, ...axisRows, btnRow);
  };

  const unsub = store.subscribe(rerender);
  rerender();

  if (opts.palette && opts.palette.registry) {
    opts.palette.registry.register("joystick.tail_logs", {
      label: "joystick: tail logs",
      verb: "tail",
      action: () => {
        if (opts.router) opts.router.navigate("/logs/ados-joystick");
      },
    });
  }

  return {
    node,
    dispose: () => {
      try { unsub && unsub(); } catch { /* noop */ }
      if (opts.palette && opts.palette.registry) {
        try { opts.palette.registry.unregister("joystick.tail_logs"); } catch { /* noop */ }
      }
    },
  };
}
