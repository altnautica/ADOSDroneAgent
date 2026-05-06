// OLED + buttons panel. Current screen, brightness, contrast, last
// button event with age, mapping for buttons 1-4. Reads
// store.state.dashboard.oled and store.state.dashboard.buttons. Active
// on every ground role. Empty state when no OLED detected.

import { el, panel } from "../../components.js";
import { pick, safeObj, fmtNum, fmtDur } from "../_util.js";

function row(label, value, severity) {
  const v = el("span", { className: "panel__row-value mono", text: value != null ? String(value) : "-" });
  if (severity) v.classList.add(`text--${severity}`);
  return el("div", { className: "panel__row" },
    el("span", { className: "panel__row-label", text: label }),
    v,
  );
}

function buttonRow(idx, label) {
  return el("div", { className: "panel__row" },
    el("span", { className: "panel__row-label mono", text: `btn ${idx}` }),
    el("span", { className: "panel__row-value mono", text: label || "-" }),
  );
}

function ageString(ms) {
  if (ms == null) return "-";
  const seconds = Number(ms) / 1000;
  return `${fmtDur(seconds)} ago`;
}

export function renderOledButtonsPanel(store, opts = {}) {
  const body = el("div", { className: "panel-stack" });
  const node = panel({
    title: "oled + buttons",
    span: 6,
    expandable: true,
    severity: "idle",
    body,
  });

  const rerender = () => {
    const state = store.get();
    const oled = pick(state, "dashboard.oled", null);
    const buttons = pick(state, "dashboard.buttons", null);

    const screen = pick(oled, "screen", null);
    const brightness = pick(oled, "brightness", null);
    const contrast = pick(oled, "contrast", null);

    const mapping = safeObj(pick(buttons, "mapping", null));
    const lastEvent = pick(buttons, "last_event", null);
    const lastBtn = pick(lastEvent, "button", null);
    const lastAction = pick(lastEvent, "action", null);
    const lastAge = pick(lastEvent, "age_ms", null);

    const hasOled = oled || screen != null;
    const hasButtons = Object.keys(mapping).length > 0 || lastEvent;

    const overall = (hasOled || hasButtons) ? "ok" : "idle";
    const dot = node.querySelector(".status-dot");
    if (dot) dot.className = `status-dot status-dot--${overall}`;

    if (!hasOled && !hasButtons) {
      body.replaceChildren(el("p", { className: "panel-empty text-faint", text: "No OLED detected" }));
      return;
    }

    const oledRows = [
      row("screen", screen ? String(screen) : "-"),
      row("brightness", brightness != null ? `${fmtNum(brightness, 0)}` : "-"),
      row("contrast", contrast != null ? `${fmtNum(contrast, 0)}` : "-"),
    ];

    const lastEventRow = el("div", { className: "panel__row" },
      el("span", { className: "panel__row-label", text: "last event" }),
      el("span", { className: "panel__row-value mono", text:
        lastEvent ? `btn ${lastBtn != null ? lastBtn : "?"} ${lastAction || ""} · ${ageString(lastAge)}`.trim() : "-"
      }),
    );

    const mapHead = el("div", { className: "panel__row mono", style: { color: "var(--text-dim)", fontSize: "11px" } },
      el("span", { className: "panel__row-label", text: "button" }),
      el("span", { className: "panel__row-value", text: "mapping" }),
    );

    const mapRows = [];
    for (const idx of [1, 2, 3, 4]) {
      const label = mapping[idx] || mapping[String(idx)] || null;
      mapRows.push(buttonRow(idx, label));
    }

    body.replaceChildren(...oledRows, lastEventRow, mapHead, ...mapRows);
  };

  const unsub = store.subscribe(rerender);
  rerender();

  if (opts.palette && opts.palette.registry) {
    opts.palette.registry.register("oled.tail_logs", {
      label: "oled: tail logs",
      verb: "tail",
      action: () => {
        if (opts.router) opts.router.navigate("/logs/ados-oled");
      },
    });
  }

  return {
    node,
    dispose: () => {
      try { unsub && unsub(); } catch { /* noop */ }
      if (opts.palette && opts.palette.registry) {
        try { opts.palette.registry.unregister("oled.tail_logs"); } catch { /* noop */ }
      }
    },
  };
}
