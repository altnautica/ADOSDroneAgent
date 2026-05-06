// Camera pipeline panel. Device path, codec, resolution, fps, bitrate,
// encoder API (RKMPP / V4L2 / libcamera), pipeline state, dropped-frame
// count, encoder cpu %. Reads store.state.dashboard.camera. Carries a
// "restart camera pipeline" verb that POSTs to the agent's service
// restart endpoint, plus a 60s sparkline of encoder cpu.

import { el, panel, sparkline } from "../../components.js";
import { apiFetch } from "../../state.js";
import { toast } from "../../components/toast.js";
import { pick, fmtNum, fmtBitrate, createRingBuffer, severityFromState } from "../_util.js";

function pipelineSeverity(state, dropped) {
  const sev = severityFromState(state);
  if (sev === "err") return "err";
  if (dropped != null && dropped > 0) return "warn";
  return sev;
}

function row(label, value, severity) {
  const v = el("span", { className: "panel__row-value mono", text: value != null ? String(value) : "-" });
  if (severity) v.classList.add(`text--${severity}`);
  return el("div", { className: "panel__row" },
    el("span", { className: "panel__row-label", text: label }),
    v,
  );
}

export function renderCameraPanel(store, opts = {}) {
  const cpuBuf = createRingBuffer(60);
  const body = el("div", { className: "panel-stack" });
  const node = panel({
    title: "camera pipeline",
    span: 6,
    expandable: true,
    severity: "idle",
    body,
  });

  const restartBtn = el("button", {
    type: "button",
    className: "btn btn--sm",
    text: "restart pipeline",
    onclick: async () => {
      try {
        await apiFetch("/api/services/ados-video/restart", { method: "POST" });
        toast({ message: "video pipeline restart requested", severity: "info" });
      } catch (err) {
        toast({ message: `restart failed: ${err.message}`, severity: "err" });
      }
    },
  });

  const rerender = () => {
    const state = store.get();
    const cam = pick(state, "dashboard.camera", null);

    const device = pick(cam, "device", "-");
    const codec = pick(cam, "codec", "-");
    const w = pick(cam, "width", null);
    const h = pick(cam, "height", null);
    const fps = pick(cam, "fps", null);
    const kbps = pick(cam, "bitrate_kbps", null);
    const api = pick(cam, "encoder_api", "-");
    const pState = pick(cam, "state", null);
    const dropped = pick(cam, "dropped_frames", null);
    const cpuPct = pick(cam, "encoder_cpu_pct", null);

    if (cpuPct != null) cpuBuf.push(cpuPct);

    const sev = pipelineSeverity(pState, dropped);
    const dot = node.querySelector(".status-dot");
    if (dot) dot.className = `status-dot status-dot--${sev}`;

    const res = (w && h) ? `${w}x${h}` : "-";
    const fpsStr = fps != null ? `${fmtNum(fps, 0)} fps` : "-";

    const rows = [
      row("device", device),
      row("codec", `${codec} · ${api}`),
      row("res / fps", `${res} ${fpsStr}`),
      row("bitrate", fmtBitrate(kbps)),
      row("state", pState ? String(pState) : "-", severityFromState(pState)),
      row("dropped", dropped != null ? String(dropped) : "-", dropped != null && dropped > 0 ? "warn" : null),
      row("encoder cpu", cpuPct != null ? `${fmtNum(cpuPct, 0)}%` : "-"),
    ];

    const sparkRow = el("div", { className: "panel__row" },
      el("span", { className: "panel__row-label", text: "cpu 60s" }),
      el("span", { style: { width: "120px", height: "20px", display: "inline-block", color: "var(--info)" } },
        sparkline(cpuBuf.points(), { width: 120, height: 20 }),
      ),
    );

    const actions = el("div", { className: "panel-actions-row" }, restartBtn);

    body.replaceChildren(...rows, sparkRow, actions);
  };

  const unsub = store.subscribe(rerender);
  rerender();

  if (opts.palette && opts.palette.registry) {
    opts.palette.registry.register("camera.restart", {
      label: "camera: restart pipeline",
      verb: "restart",
      action: () => restartBtn.click(),
    });
    opts.palette.registry.register("camera.tail_logs", {
      label: "camera: tail logs",
      verb: "tail",
      action: () => {
        if (opts.router) opts.router.navigate("/logs/ados-video");
      },
    });
  }

  return {
    node,
    dispose: () => {
      try { unsub && unsub(); } catch { /* noop */ }
      if (opts.palette && opts.palette.registry) {
        try { opts.palette.registry.unregister("camera.restart"); } catch { /* noop */ }
        try { opts.palette.registry.unregister("camera.tail_logs"); } catch { /* noop */ }
      }
    },
  };
}
