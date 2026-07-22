// L0 — the full-bleed WHEP video layer. Owns one WHEP session against the
// agent's WHEP proxy (a recvonly WebRTC pull of the mediamtx feed) and paints it
// edge-to-edge behind the HUD. On failure it retries with backoff and surfaces
// an honest connecting/no-feed state rather than a frozen black frame. The Feed
// re-points it (a different `whepUrl` for another camera, or a bumped
// `reconnectKey` for a manual refresh) by changing its props.

import { useEffect, useRef, useState } from "react";

import { useFeedStore } from "@/stores/feed-store";
import { startWhep, type WhepSession } from "@/lib/whep";

const RETRY_MIN_MS = 1500;
const RETRY_MAX_MS = 8000;

type FeedState = "connecting" | "live" | "error";

export function VideoLayer({
  whepUrl = "/whep",
  reconnectKey,
}: {
  /** The WHEP endpoint for the active stream (the primary leg is `/whep`). */
  whepUrl?: string;
  /** Changing this tears down and re-establishes the session (a camera switch
   *  or a manual refresh). */
  reconnectKey?: string;
}) {
  const videoRef = useRef<HTMLVideoElement | null>(null);
  const [state, setState] = useState<FeedState>("connecting");
  const setVideoStatus = useFeedStore((s) => s.setVideoStatus);

  useEffect(() => {
    let cancelled = false;
    let session: WhepSession | null = null;
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

    const connect = async () => {
      const target = videoRef.current;
      if (cancelled || !target) return;
      setBoth("connecting");
      const result = await startWhep(whepUrl, target, controller.signal);
      if (cancelled) {
        void result.session?.close();
        return;
      }
      if (result.ok && result.session) {
        session = result.session;
        retryMs = RETRY_MIN_MS;
        setBoth("live");
        // A source-less feed connects but never plays; surface that honestly.
        session.pc.addEventListener("connectionstatechange", () => {
          const cs = session?.pc.connectionState;
          if (cs === "failed" || cs === "disconnected") {
            setBoth("error");
            scheduleRetry();
          }
        });
      } else {
        setBoth("error");
        scheduleRetry();
      }
    };

    const scheduleRetry = () => {
      if (cancelled || retryTimer) return;
      retryTimer = setTimeout(() => {
        retryTimer = null;
        void session?.close();
        session = null;
        void connect();
      }, retryMs);
      retryMs = Math.min(retryMs * 2, RETRY_MAX_MS);
    };

    void connect();

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
      void session?.close();
    };
  }, [whepUrl, reconnectKey, setVideoStatus]);

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
    </div>
  );
}
