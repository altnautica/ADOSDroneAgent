// L0 — the full-bleed WHEP video layer. Owns one WHEP session against the
// agent's `POST /whep` proxy (a recvonly WebRTC pull of the mediamtx feed) and
// paints it edge-to-edge behind the HUD. On failure it retries with backoff and
// surfaces an honest connecting/no-feed state rather than a frozen black frame.

import { useEffect, useRef, useState } from "react";

import { startWhep, type WhepSession } from "@/lib/whep";

const WHEP_URL = "/whep";
const RETRY_MIN_MS = 1500;
const RETRY_MAX_MS = 8000;

type FeedState = "connecting" | "live" | "error";

export function VideoLayer() {
  const videoRef = useRef<HTMLVideoElement | null>(null);
  const [state, setState] = useState<FeedState>("connecting");

  useEffect(() => {
    let cancelled = false;
    let session: WhepSession | null = null;
    let retryMs = RETRY_MIN_MS;
    let retryTimer: ReturnType<typeof setTimeout> | null = null;
    const controller = new AbortController();

    const connect = async () => {
      const el = videoRef.current;
      if (cancelled || !el) return;
      setState("connecting");
      const result = await startWhep(WHEP_URL, el, controller.signal);
      if (cancelled) {
        void result.session?.close();
        return;
      }
      if (result.ok && result.session) {
        session = result.session;
        retryMs = RETRY_MIN_MS;
        setState("live");
        // A source-less feed connects but never plays; surface that honestly.
        session.pc.addEventListener("connectionstatechange", () => {
          const cs = session?.pc.connectionState;
          if (cs === "failed" || cs === "disconnected") {
            setState("error");
            scheduleRetry();
          }
        });
      } else {
        setState("error");
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
      void session?.close();
    };
  }, []);

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
            {state === "connecting" ? "Connecting to feed…" : "No video feed"}
          </span>
        </div>
      ) : null}
    </div>
  );
}
