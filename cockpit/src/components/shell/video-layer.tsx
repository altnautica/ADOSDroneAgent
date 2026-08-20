// L0 — the full-bleed video layer. Owns one playback session against the
// agent's :8080 proxy and paints it edge-to-edge behind the HUD.
//
// Transport cascade (ported from the dashboard's VideoPanel):
//   WHEP first  — low latency (~100-300 ms) on the local / same-origin path.
//   HLS fallback — ~3-5 s latency but Robut: WebRTC ICE will not traverse
//     Tailscale / remote hops (mediamtx answers with LAN candidates), so a
//     remote viewer that sticks to WHEP-only sees "No video source". On WHEP
//     failure we fall back to the relative HLS endpoint instead of giving up.
//
// On every-transport failure it retries with backoff and surfaces an honest
// connecting/no-feed state rather than a frozen black frame. The Feed re-points
// it (a different `whepUrl`/`hlsUrl` for another camera, or a bumped
// `reconnectKey` for a manual refresh) by changing its props.

import { useEffect, useRef, useState } from "react";

import { useFeedStore } from "@/stores/feed-store";
import { startWhep, type WhepSession } from "@/lib/whep";
import { startHls, type HlsSession } from "@/lib/hls";

const RETRY_MIN_MS = 1500;
const RETRY_MAX_MS = 8000;

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
    let retryMs = RETRY_MIN_MS;
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

    const el = videoRef.current;
    if (el) {
      el.addEventListener("loadedmetadata", publishLive);
      el.addEventListener("resize", publishLive);
    }

    const teardown = () => {
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
          retryMs = RETRY_MIN_MS;
          setTransport("whep");
          setBoth("live");
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
        const result = await startHls(hlsUrl, target);
        if (cancelled) return;
        if (result.ok && result.session) {
          hls = result.session;
          retryMs = RETRY_MIN_MS;
          setTransport("hls");
          setBoth("live");
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
      const result = await startHls(hlsUrl, target);
      if (cancelled) return;
      if (result.ok && result.session) {
        hls = result.session;
        retryMs = RETRY_MIN_MS;
        setTransport("hls");
        setBoth("live");
        return;
      }
      setBoth("error");
      scheduleRetry();
    };

    const scheduleRetry = () => {
      if (cancelled || retryTimer) return;
      retryTimer = setTimeout(() => {
        retryTimer = null;
        teardown();
        void attemptCascade();
      }, retryMs);
      retryMs = Math.min(retryMs * 2, RETRY_MAX_MS);
    };

    void attemptCascade();

    return () => {
      cancelled = true;
      controller.abort();
      if (retryTimer) clearTimeout(retryTimer);
      if (el) {
        el.removeEventListener("loadedmetadata", publishLive);
        el.removeEventListener("resize", publishLive);
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
