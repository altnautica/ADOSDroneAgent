// L0 — the full-bleed video layer. Owns one playback session against the
// agent's :8080 proxy and paints it edge-to-edge behind the HUD.
//
// Transport cascade (the same order the dashboard's video panel uses):
//   WHEP first  — low latency (~100-300 ms) on the local / same-origin path.
//   HLS fallback — ~3-5 s latency but robust: WebRTC ICE will not traverse
//     Tailscale / remote hops (mediamtx answers with LAN candidates), so a
//     remote viewer that sticks to WHEP-only sees "No video source". On WHEP
//     failure we fall back to the relative HLS endpoint instead of giving up.
//
// On every-transport failure it retries on a fixed cadence and surfaces an
// honest connecting/no-feed state rather than a frozen black frame. "Live" means
// a frame has decoded (`loadeddata`), not that a handshake finished: a
// negotiated session over a black frame still reads "connecting". An HLS
// session that dies later is reported and retried. The Feed re-points
// it (a different `whepUrl`/`hlsUrl` for another camera, or a bumped
// `reconnectKey` for a manual refresh) by changing its props.

import { useEffect, useRef, useState } from "react";

import { useFeedStore } from "@/stores/feed-store";
import { startWhep, type WhepSession } from "@/lib/whep";
import { startHls, type HlsSession } from "@/lib/hls";

/** Fixed pause before a failed feed is retried (no backoff, no cap). */
const RETRY_MS = 3000;

type FeedState = "connecting" | "live" | "error";
type Transport = "whep" | "hls";

export function VideoLayer({
  whepUrl = "/whep",
  hlsUrl = "/hls/main/index.m3u8",
  reconnectKey,
}: {
  /** The WHEP endpoint for the active stream (the primary leg is `/whep`). */
  whepUrl?: string;
  /** Relative HLS endpoint for the SAME leg (`/hls/main/index.m3u8` primary,
   *  `/hls/<id>/index.m3u8` per-leg). Used only when WHEP cannot establish
   *  (typically over Tailscale / remote, where ICE will not traverse). */
  hlsUrl?: string;
  /** Changing this tears down and re-establishes the session (a camera switch
   *  or a manual refresh). */
  reconnectKey?: string;
}) {
  const videoRef = useRef<HTMLVideoElement | null>(null);
  const [state, setState] = useState<FeedState>("connecting");
  const [transport, setTransport] = useState<Transport>("whep");
  const setVideoStatus = useFeedStore((s) => s.setVideoStatus);

  useEffect(() => {
    let cancelled = false;
    let whep: WhepSession | null = null;
    let hls: HlsSession | null = null;
    let retryTimer: ReturnType<typeof setTimeout> | null = null;
    const controller = new AbortController();

    // Publish the decoded resolution off the real <video> element so the strip's
    // video zone shows the actual stream size (never a config-derived guess). A
    // width of 0 means metadata is not in yet — publish live with a null size and
    // let the resize/loadedmetadata listeners refine it.
    const publishLive = () => {
      const v = videoRef.current;
      const w = v && v.videoWidth > 0 ? v.videoWidth : null;
      const h = v && v.videoHeight > 0 ? v.videoHeight : null;
      setVideoStatus("live", w, h);
    };

    const setBoth = (s: FeedState) => {
      setState(s);
      if (s === "live") publishLive();
      else setVideoStatus(s, null, null);
    };

    // A transport that negotiated is only a candidate; the feed is live once
    // the element decodes a frame.
    let negotiated = false;
    const onFrame = () => {
      if (negotiated && !cancelled) setBoth("live");
    };
    const onResize = () => {
      if (negotiated && !cancelled) publishLive();
    };

    const el = videoRef.current;
    if (el) {
      el.addEventListener("loadeddata", onFrame);
      el.addEventListener("loadedmetadata", onResize);
      el.addEventListener("resize", onResize);
    }

    const teardown = () => {
      negotiated = false;
      const w = whep;
      whep = null;
      if (w) w.close().catch(() => undefined);
      const h = hls;
      hls = null;
      if (h) h.close();
    };

    // Try WHEP, then HLS. Only when both fail do we schedule a full retry.
    const attemptCascade = async () => {
      const target = videoRef.current;
      if (cancelled || !target) return;
      setBoth("connecting");

      if (whepUrl) {
        const result = await startWhep(whepUrl, target, controller.signal);
        if (cancelled) {
          void result.session?.close();
          return;
        }
        if (result.ok && result.session) {
          whep = result.session;
          negotiated = true;
          // A frame may already have decoded before the handshake resolved.
          if (target.readyState >= HTMLMediaElement.HAVE_CURRENT_DATA) onFrame();
          setTransport("whep");
          result.session.pc.addEventListener("connectionstatechange", () => {
            const cs = result.session?.pc.connectionState;
            // A session that was live but drops (radio fade / RTP stall over
            // WHEP) should try HLS rather than sit on a dead connection.
            if (cs === "failed" || cs === "disconnected") {
              void hlsFallback();
            }
          });
          return;
        }
      }

      // WHEP gave up (typically ICE not traversing a remote hop). Fall back to HLS.
      if (hlsUrl) {
        const result = await startHls(hlsUrl, target, onHlsLost);
        if (cancelled) return;
        if (result.ok && result.session) {
          hls = result.session;
          negotiated = true;
          // A frame may already have decoded before the handshake resolved.
          if (target.readyState >= HTMLMediaElement.HAVE_CURRENT_DATA) onFrame();
          setTransport("hls");
          return;
        }
      }

      setBoth("error");
      scheduleRetry();
    };

    // Mid-stream WHEP dropout → fall back to HLS for the same leg (no full
    // retry needed). Only if HLS also fails do we schedule a retry.
    const hlsFallback = async () => {
      const target = videoRef.current;
      if (cancelled || !target || !hlsUrl) {
        setBoth("error");
        scheduleRetry();
        return;
      }
      setBoth("connecting");
      const w = whep;
      whep = null;
      if (w) w.close().catch(() => undefined);
      const result = await startHls(hlsUrl, target, onHlsLost);
      if (cancelled) return;
      if (result.ok && result.session) {
        hls = result.session;
        negotiated = true;
        if (target.readyState >= HTMLMediaElement.HAVE_CURRENT_DATA) onFrame();
        setTransport("hls");
        return;
      }
      setBoth("error");
      scheduleRetry();
    };

    // An established HLS session died beyond hls.js's own recovery.
    function onHlsLost() {
      if (cancelled) return;
      hls = null;
      negotiated = false;
      setBoth("error");
      scheduleRetry();
    }

    const scheduleRetry = () => {
      if (cancelled || retryTimer) return;
      retryTimer = setTimeout(() => {
        retryTimer = null;
        teardown();
        void attemptCascade();
      }, RETRY_MS);
    };

    void attemptCascade();

    return () => {
      cancelled = true;
      controller.abort();
      if (retryTimer) clearTimeout(retryTimer);
      if (el) {
        el.removeEventListener("loadeddata", onFrame);
        el.removeEventListener("loadedmetadata", onResize);
        el.removeEventListener("resize", onResize);
      }
      // Reset the shared state so a stale "live" never lingers after the feed
      // unmounts (leaving the Feed screen).
      setVideoStatus("connecting", null, null);
      teardown();
    };
  }, [whepUrl, hlsUrl, reconnectKey, setVideoStatus]);

  return (
    <div className="absolute inset-0 bg-black">
      <video
        ref={videoRef}
        className="h-full w-full object-contain"
        autoPlay
        muted
        playsInline
      />
      {state !== "live" ? (
        <div className="absolute inset-0 flex items-center justify-center">
          <span className="rounded-md bg-background/70 px-[0.9rem] py-[0.5rem] text-[0.9rem] text-muted-foreground">
            {state === "connecting" ? "Connecting to feed…" : "No video source"}
          </span>
        </div>
      ) : null}
      {/*
        Keep the transport visible to the operator without extra chrome: a tiny
        corner tag only when we had to fall back to HLS (WHEP is the default and
        needs no explanation).
      */}
      {state === "live" && transport === "hls" ? (
        <div className="absolute bottom-2 left-2 rounded bg-background/60 px-1.5 py-0.5 text-[0.6rem] uppercase tracking-wider text-muted-foreground">
          HLS
        </div>
      ) : null}
    </div>
  );
}
