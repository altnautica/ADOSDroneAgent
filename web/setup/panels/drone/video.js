// Live video panel. WebRTC primary via WHEP, HLS fallback, MJPEG snapshot
// last-resort. Reads transport coordinates from store.state.status (the
// /api/v1/setup/status payload) and live metrics from
// store.state.dashboard.video (the /api/v1/dashboard/snapshot payload).
//
// The panel itself owns the player lifecycle: it picks the best transport
// at mount time, swaps to a fallback when the chosen one errors, and
// tears the player down on dispose so we never leak a peer connection.

import { el, panel, sparkline } from "../../components.js";
import { apiFetch } from "../../state.js";
import { toast } from "../../components/toast.js";
import { createRingBuffer, pick, fmtNum, fmtBitrate, severityFromState } from "../_util.js";

const HLS_DEFAULT = "/hls/index.m3u8";
const SNAPSHOT_URL = "/api/video/snapshot.jpg";

function pickTransport(status) {
  const whep = pick(status, "video.whep_url", null);
  if (whep) return { kind: "webrtc", url: whep };
  const hls = pick(status, "video.hls_url", HLS_DEFAULT);
  if (hls) return { kind: "hls", url: hls };
  return { kind: "snapshot", url: SNAPSHOT_URL };
}

async function startWebRTC(videoEl, whepUrl) {
  const pc = new RTCPeerConnection({ iceServers: [] });
  pc.addTransceiver("video", { direction: "recvonly" });
  pc.addTransceiver("audio", { direction: "recvonly" });
  pc.ontrack = (ev) => {
    if (videoEl.srcObject !== ev.streams[0]) {
      videoEl.srcObject = ev.streams[0];
    }
  };
  const offer = await pc.createOffer();
  await pc.setLocalDescription(offer);
  const res = await fetch(whepUrl, {
    method: "POST",
    headers: { "Content-Type": "application/sdp" },
    body: offer.sdp || "",
  });
  if (!res.ok) {
    pc.close();
    throw new Error(`whep ${res.status}`);
  }
  const answer = await res.text();
  await pc.setRemoteDescription({ type: "answer", sdp: answer });
  return () => {
    try { pc.close(); } catch { /* noop */ }
    videoEl.srcObject = null;
  };
}

function startHls(videoEl, hlsUrl) {
  // Native HLS on Safari and iOS. Other browsers need hls.js, which we
  // do not bundle. Falls back to snapshot path if the source errors.
  videoEl.src = hlsUrl;
  videoEl.load();
  return () => { try { videoEl.removeAttribute("src"); videoEl.load(); } catch { /* noop */ } };
}

function startSnapshot(imgEl) {
  let alive = true;
  const tick = () => {
    if (!alive) return;
    imgEl.src = `${SNAPSHOT_URL}?t=${Date.now()}`;
  };
  tick();
  const id = setInterval(tick, 1000);
  return () => {
    alive = false;
    clearInterval(id);
  };
}

function renderMetricsRow(snap, bitrateBuf) {
  const codec = pick(snap, "codec", "-");
  const w = pick(snap, "width", null);
  const h = pick(snap, "height", null);
  const fps = pick(snap, "fps", null);
  const kbps = pick(snap, "bitrate_kbps", null);
  const lat = pick(snap, "glass_to_glass_ms", null);

  const res = w && h ? `${w}x${h}` : "-";
  const fpsStr = fps != null ? `${fmtNum(fps, 0)} fps` : "-";

  const left = el("div", { className: "panel__row-label mono", text: `${codec} ${res} ${fpsStr}` });
  const right = el("div", { className: "panel__row-value mono", text: lat != null ? `g2g ${fmtNum(lat, 0)}ms` : "g2g -" });

  const meta = el("div", { className: "panel__row" }, left, right);

  const bitrateLine = el("div", { className: "panel__row" },
    el("span", { className: "panel__row-label", text: "bitrate" }),
    el("span", { className: "panel__row-value mono", text: fmtBitrate(kbps) }),
  );

  const sparkWrap = el("div", { className: "panel__row" },
    el("span", { className: "panel__row-label", text: "60s" }),
    el("span", { style: { width: "120px", height: "22px", display: "inline-block", color: "var(--info)" } },
      sparkline(bitrateBuf.points(), { width: 120, height: 22 }),
    ),
  );

  return [meta, bitrateLine, sparkWrap];
}

export function renderVideoPanel(store, opts = {}) {
  const bitrateBuf = createRingBuffer(60);

  const videoEl = el("video", {
    className: "video-tile",
    autoplay: true,
    muted: true,
    playsinline: true,
    controls: false,
    style: { width: "100%", height: "auto", maxHeight: "320px", background: "#000", borderRadius: "4px" },
  });
  const imgEl = el("img", {
    className: "video-tile",
    alt: "snapshot",
    style: { width: "100%", height: "auto", maxHeight: "320px", background: "#000", borderRadius: "4px", display: "none" },
  });
  const errEl = el("p", { className: "panel-empty text-faint", text: "" });
  const stage = el("div", { className: "video-stage" }, videoEl, imgEl, errEl);

  const fullBtn = el("button", {
    type: "button",
    className: "btn btn--sm",
    text: "[⛶ fullscreen]",
    onclick: () => {
      const target = videoEl.style.display !== "none" ? videoEl : imgEl;
      if (target && typeof target.requestFullscreen === "function") {
        target.requestFullscreen().catch((err) => {
          toast({ message: `fullscreen blocked: ${err.message || err}`, severity: "warn" });
        });
      }
    },
  });

  const metricsHost = el("div", { className: "video-metrics" });

  const body = el("div", { className: "panel-stack" },
    stage,
    metricsHost,
    el("div", { className: "panel-actions-row" }, fullBtn),
  );

  let stopPlayer = null;
  let currentTransport = null;

  const useTransport = async (t) => {
    if (stopPlayer) { try { stopPlayer(); } catch { /* noop */ } stopPlayer = null; }
    if (!t) {
      videoEl.style.display = "none";
      imgEl.style.display = "none";
      errEl.textContent = "no video transport";
      return;
    }
    errEl.textContent = "";
    if (t.kind === "webrtc") {
      videoEl.style.display = "block";
      imgEl.style.display = "none";
      try {
        stopPlayer = await startWebRTC(videoEl, t.url);
      } catch (err) {
        errEl.textContent = `webrtc failed (${err.message || err}), falling back`;
        await useTransport({ kind: "hls", url: pick(store.get().status, "video.hls_url", HLS_DEFAULT) });
      }
      return;
    }
    if (t.kind === "hls") {
      videoEl.style.display = "block";
      imgEl.style.display = "none";
      videoEl.onerror = () => {
        errEl.textContent = "hls failed, falling back to snapshot";
        useTransport({ kind: "snapshot", url: SNAPSHOT_URL });
      };
      stopPlayer = startHls(videoEl, t.url);
      return;
    }
    videoEl.style.display = "none";
    imgEl.style.display = "block";
    stopPlayer = startSnapshot(imgEl);
  };

  const node = panel({
    title: "live video",
    span: 8,
    expandable: true,
    severity: "idle",
    body,
  });

  const rerender = () => {
    const state = store.get();
    const snap = pick(state, "dashboard.video", null);
    const kbps = pick(snap, "bitrate_kbps", null);
    if (kbps != null) bitrateBuf.push(kbps);

    const sev = severityFromState(pick(snap, "state", null)) || "idle";
    const dot = node.querySelector(".status-dot");
    if (dot) {
      dot.className = `status-dot status-dot--${sev}`;
    }

    metricsHost.replaceChildren(...renderMetricsRow(snap, bitrateBuf));

    // Pick transport from status the first time we have one.
    const t = pickTransport(state.status);
    if (!currentTransport || currentTransport.kind !== t.kind || currentTransport.url !== t.url) {
      currentTransport = t;
      useTransport(t);
    }
  };

  const unsub = store.subscribe(rerender);
  rerender();

  if (opts.palette && opts.palette.registry) {
    opts.palette.registry.register("video.fullscreen", {
      label: "video: fullscreen",
      verb: "fullscreen",
      action: () => fullBtn.click(),
    });
    opts.palette.registry.register("video.snapshot", {
      label: "video: snapshot",
      verb: "snapshot",
      action: async () => {
        try {
          await apiFetch("/api/video/snapshot", { method: "POST" });
          toast({ message: "snapshot captured", severity: "ok" });
        } catch (err) {
          toast({ message: `snapshot failed: ${err.message}`, severity: "err" });
        }
      },
    });
  }

  return {
    node,
    dispose: () => {
      try { unsub && unsub(); } catch { /* noop */ }
      try { stopPlayer && stopPlayer(); } catch { /* noop */ }
      if (opts.palette && opts.palette.registry) {
        try { opts.palette.registry.unregister("video.fullscreen"); } catch { /* noop */ }
        try { opts.palette.registry.unregister("video.snapshot"); } catch { /* noop */ }
      }
    },
  };
}
