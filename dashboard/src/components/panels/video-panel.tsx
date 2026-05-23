import { Image as ImageIcon, Video, VideoOff } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";

import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { useSnapshot } from "@/hooks/use-snapshot";
import { useStatus } from "@/hooks/use-status";
import { useWfb } from "@/hooks/use-wfb";
import { fmtBitrate, fmtNum } from "@/lib/format";
import { startHls, type HlsSession } from "@/lib/hls";
import { startWhep, type WhepSession } from "@/lib/whep";
import { cn } from "@/lib/utils";

// Two transports + a snapshot fallback. The order of attempts is
// profile-driven and overridable from the UI:
//
//   ground_station → HLS first (~3-5 s latency, no freeze)
//   drone           → WHEP first (~100-300 ms latency, local camera)
//
// HLS-first on ground was a deliberate trade-off: the WFB-rx → ffmpeg
// → MediaMTX path drops WHEP into a Chrome decoder-sync freeze after
// a few seconds in the field. HLS.js re-fetches segments on stutter
// and never gets stuck. WHEP stays selectable for low-latency demos.
type Transport = "whep" | "hls";
type PlayerState =
  | "idle"
  | "connecting"
  | "live"
  | "snapshot"
  | "final-error";

// Threshold for declaring "transport negotiated but no frame has
// decoded yet" — keeps the LIVE badge honest. The cascade can take
// a few seconds on cold-start (mpegts HLS muxer warm-up ~6-10 s;
// WHEP handshake ~3-5 s on Pi 4B), so we wait at least this long
// before flipping the badge from "live" to "live · no frames" to
// surface the truth instead of an optimistic green pill.
const NO_FRAMES_WARNING_MS = 6000;

export function VideoPanel() {
  const status = useStatus();
  const snap = useSnapshot();
  const wfb = useWfb();

  const videoRef = useRef<HTMLVideoElement | null>(null);
  const whepRef = useRef<WhepSession | null>(null);
  const hlsRef = useRef<HlsSession | null>(null);
  const [state, setState] = useState<PlayerState>("idle");
  const [activeTransport, setActiveTransport] = useState<Transport | null>(
    null,
  );
  const [error, setError] = useState<string | null>(null);
  const [retryToken, setRetryToken] = useState(0);
  // Truth signal for the LIVE badge — flips to true on videoEl's
  // first `loadeddata` event, resets on every cascade restart.
  // Without this the green pill could show "live · webrtc" while
  // the video element renders black: startWhep returns ok on
  // PeerConnection setRemoteDescription, but Chrome's decoder
  // can still fail to bootstrap if no IDR with SPS/PPS arrives.
  const [framesArrived, setFramesArrived] = useState(false);
  const [noFramesWarning, setNoFramesWarning] = useState(false);

  const whepUrl = status.data?.video?.whep_url ?? "";
  const hlsUrl = status.data?.video?.hls_url ?? "";
  const v = snap.data?.video;
  const codec = v?.codec ?? "";
  const w = v?.width ?? 0;
  const h = v?.height ?? 0;
  const fps = v?.fps ?? 0;
  const bitrate = v?.bitrate_kbps ?? 0;
  const pipelineState = v?.state ?? "unknown";
  const g2g = v?.glass_to_glass_ms;

  const profile = status.data?.profile;
  const isGround = profile === "ground_station";

  // Profile-driven default. Operator can override via the Transport
  // chip; the override persists in component state until page
  // reload (no localStorage — most operators want the sensible
  // default on every reload).
  const defaultTransport: Transport = isGround ? "hls" : "whep";
  const [preferredTransport, setPreferredTransport] = useState<Transport>(
    defaultTransport,
  );
  // Keep the preference aligned with the profile when status loads
  // after first render.
  useEffect(() => {
    setPreferredTransport(defaultTransport);
  }, [defaultTransport]);

  const wfbPacketsReceived = wfb.data?.packets_received ?? 0;
  const wfbState = wfb.data?.state ?? "unknown";
  const wfbChannel = wfb.data?.channel ?? null;
  const wfbStreaming = wfbPacketsReceived > 0;
  const waitingForWfb = isGround && !wfbStreaming;
  const wfbWaitDetail =
    `drone TX ${wfbState}` +
    (wfbChannel ? ` on ch ${wfbChannel}` : "");

  const pipelineRunning = pipelineState === "running";
  const haveAnyUrl = whepUrl.length > 0 || hlsUrl.length > 0;
  const canStream = pipelineRunning && haveAnyUrl;

  // Compute the cascade order based on preferred transport, dropping
  // any leg whose URL is empty. Snapshot is always the final fallback.
  const cascade = useMemo(() => {
    const order: Transport[] = [];
    if (preferredTransport === "hls") {
      if (hlsUrl) order.push("hls");
      if (whepUrl) order.push("whep");
    } else {
      if (whepUrl) order.push("whep");
      if (hlsUrl) order.push("hls");
    }
    return order;
  }, [preferredTransport, whepUrl, hlsUrl]);

  function tearDown() {
    const w = whepRef.current;
    whepRef.current = null;
    if (w) w.close().catch(() => undefined);
    const h = hlsRef.current;
    hlsRef.current = null;
    if (h) h.close();
  }

  useEffect(() => {
    if (!canStream || !videoRef.current || cascade.length === 0) {
      tearDown();
      setState("idle");
      setActiveTransport(null);
      setError(null);
      setFramesArrived(false);
      setNoFramesWarning(false);
      return;
    }

    let cancelled = false;
    const ac = new AbortController();
    const errors: string[] = [];

    const runCascade = async () => {
      setState("connecting");
      setActiveTransport(cascade[0]);
      setError(null);
      setFramesArrived(false);
      setNoFramesWarning(false);

      for (const transport of cascade) {
        if (cancelled) return;
        setActiveTransport(transport);
        if (transport === "whep") {
          // No artificial timeout wrapper. mediamtx's WHEP server
          // has its own 15 s webrtcHandshakeTimeout. A racy short
          // timeout (1.2 s in earlier revisions) leaked
          // PeerConnections when startWhep resolved AFTER the
          // timeout fired: whepRef.current was never set, close()
          // was never called, and the live PC kept feeding the
          // videoEl.srcObject while the cascade moved on to HLS.
          // ac.signal still aborts on unmount via the cleanup
          // return below.
          const result = await startWhep(whepUrl, videoRef.current!, ac.signal);
          if (cancelled) return;
          if (result.ok && result.session) {
            whepRef.current = result.session;
            setState("live");
            return;
          }
          errors.push(`WebRTC: ${result.error ?? "handshake failed"}`);
        } else {
          const result = await startHls(hlsUrl, videoRef.current!);
          if (cancelled) return;
          if (result.ok && result.session) {
            hlsRef.current = result.session;
            setState("live");
            return;
          }
          errors.push(`HLS: ${result.error ?? "playback failed"}`);
        }
      }

      // Every transport in the cascade failed. Fall through to
      // the still-snapshot fallback so the operator at least sees
      // the last good frame.
      if (cancelled) return;
      setError(errors.join("; "));
      setState("snapshot");
      setActiveTransport(null);
    };

    void runCascade();

    return () => {
      cancelled = true;
      ac.abort();
      tearDown();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [canStream, whepUrl, hlsUrl, retryToken, preferredTransport]);

  // Truth signal: bind to the video element's decode lifecycle so
  // the LIVE badge cannot lie. loadeddata fires when the first
  // decoded frame is ready; metadata can be present without
  // pixels, hence loadeddata not loadedmetadata. Reset by the
  // cascade effect on every restart.
  useEffect(() => {
    const el = videoRef.current;
    if (!el) return;
    const onLoadedData = () => setFramesArrived(true);
    el.addEventListener("loadeddata", onLoadedData);
    return () => {
      el.removeEventListener("loadeddata", onLoadedData);
    };
  }, []);

  // 6 s after state flips to "live", warn if no frame has yet
  // decoded. Operator sees "live · no frames" in a warning color
  // and knows the transport completed handshake but the decoder
  // never got a usable IDR. Better than a green pill over a
  // black frame.
  useEffect(() => {
    if (state !== "live" || framesArrived) {
      setNoFramesWarning(false);
      return;
    }
    const t = setTimeout(() => setNoFramesWarning(true), NO_FRAMES_WARNING_MS);
    return () => clearTimeout(t);
  }, [state, framesArrived]);

  const isLive = state === "live";
  // Only call it "really live" when frames are actually decoding.
  // Transports can return ok without delivering pixels (PC
  // negotiated but no IDR, HLS playlist served but segments 404).
  const isReallyLive = isLive && framesArrived;
  const isLivePending = isLive && !framesArrived;
  // Overlay covers the video element only when the transport
  // itself isn't yet up. Once state="live", let the video element
  // show what it has (potentially black if no frames decoded) and
  // surface the truth via the badge — the overlay div had no
  // matching inner branch for the live-pending case anyway.
  const showOverlay = !isLive && state !== "snapshot";
  const showSnapshot = state === "snapshot";

  const badgeText = isReallyLive
    ? activeTransport === "hls"
      ? "live · hls"
      : "live · webrtc"
    : isLivePending && noFramesWarning
      ? activeTransport === "hls"
        ? "hls · no frames"
        : "webrtc · no frames"
      : isLivePending
        ? "negotiating"
        : state === "connecting"
          ? "connecting"
          : state === "snapshot"
            ? "snapshot"
            : state === "final-error"
              ? "error"
              : "idle";
  const badgeClass = isReallyLive
    ? activeTransport === "hls"
      ? "border-warn/40 text-warn"
      : "border-ok/40 text-ok"
    : isLivePending && noFramesWarning
      ? "border-destructive/40 text-destructive"
      : "border-muted-foreground/40 text-muted-foreground/80";
  const badgeTitle = isReallyLive && activeTransport === "hls"
    ? "HLS playback. ~6-10 s latency vs WHEP's 100-300 ms; no Chrome-decoder freeze."
    : isLivePending && noFramesWarning
      ? "Transport handshake completed but no decoded frame yet — likely a codec-parameter or segment-serving issue upstream."
      : undefined;

  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          {isLive ? (
            <Video className="h-3.5 w-3.5 text-ok" />
          ) : (
            <Video className="h-3.5 w-3.5" />
          )}
          Video
          <span
            className={cn(
              "ml-auto text-[10px] uppercase tracking-wider px-1.5 py-0.5 rounded border",
              badgeClass,
            )}
            title={badgeTitle}
          >
            {badgeText}
          </span>
        </CardTitle>
      </CardHeader>
      <CardContent className="space-y-3">
        {whepUrl && hlsUrl && (
          <TransportChip
            value={preferredTransport}
            onChange={setPreferredTransport}
            isGround={isGround}
          />
        )}

        <div className="relative aspect-video w-full rounded-md border border-border bg-black overflow-hidden">
          <video
            ref={videoRef}
            className="absolute inset-0 h-full w-full object-contain"
            muted
            autoPlay
            playsInline
          />
          {showSnapshot && (
            <img
              src={`/api/video/snapshot.jpg?t=${retryToken}`}
              alt="Last snapshot"
              className="absolute inset-0 h-full w-full object-contain"
              onError={() => {
                setState("final-error");
                setError(error || "Snapshot also unavailable.");
              }}
            />
          )}
          {(showOverlay || state === "snapshot") && (
            <div className="absolute inset-0 flex flex-col items-center justify-center gap-2 bg-black/60 text-xs text-muted-foreground">
              {state === "idle" && (
                <>
                  <VideoOff className="h-6 w-6 opacity-60" />
                  <div>
                    {waitingForWfb
                      ? "Waiting for WFB stream from drone."
                      : pipelineState === "error"
                        ? "Pipeline error."
                        : pipelineState === "starting"
                          ? "Pipeline starting…"
                          : !haveAnyUrl
                            ? "No camera detected."
                            : "Pipeline idle."}
                  </div>
                  <div className="text-[10px] max-w-xs text-center">
                    {waitingForWfb
                      ? wfbWaitDetail
                      : "Plug in a camera and the agent will publish a video stream automatically."}
                  </div>
                </>
              )}
              {state === "connecting" && (
                <div className="text-center">
                  {waitingForWfb ? (
                    <>
                      Waiting for WFB stream from drone.
                      <div className="text-[10px] mt-1 opacity-80">
                        {wfbWaitDetail}
                      </div>
                    </>
                  ) : (
                    <>
                      connecting (
                      {activeTransport === "hls" ? "HLS" : "WebRTC"}
                      )…
                    </>
                  )}
                </div>
              )}
              {state === "snapshot" && (
                <div className="absolute bottom-2 left-2 right-2 flex items-center justify-between gap-2 bg-background/80 rounded px-2 py-1">
                  <div className="flex items-center gap-1 text-[10px]">
                    <ImageIcon className="h-3 w-3" />
                    Snapshot only — live feed unavailable
                  </div>
                  <Button
                    size="sm"
                    variant="outline"
                    onClick={() => setRetryToken((n) => n + 1)}
                  >
                    Retry
                  </Button>
                </div>
              )}
              {state === "final-error" && (
                <>
                  <VideoOff className="h-6 w-6 opacity-60" />
                  <div className="text-destructive/90 max-w-md text-center">
                    {waitingForWfb
                      ? `No video. ${wfbWaitDetail}; WFB-rx received 0 packets.`
                      : error || "Stream unavailable."}
                  </div>
                  <Button
                    size="sm"
                    variant="outline"
                    onClick={() => setRetryToken((n) => n + 1)}
                  >
                    Retry
                  </Button>
                </>
              )}
            </div>
          )}
        </div>

        <div className="grid grid-cols-2 gap-x-4 gap-y-1.5 text-sm">
          <div className="text-xs text-muted-foreground">pipeline</div>
          <div className="font-mono">{pipelineState}</div>

          {codec && (
            <>
              <div className="text-xs text-muted-foreground">codec</div>
              <div className="font-mono">{codec}</div>
            </>
          )}

          {w > 0 && h > 0 && (
            <>
              <div className="text-xs text-muted-foreground">res</div>
              <div className="font-mono">{`${w}×${h}`}</div>
            </>
          )}

          {fps > 0 && (
            <>
              <div className="text-xs text-muted-foreground">fps</div>
              <div className="font-mono">{fmtNum(fps, 0)}</div>
            </>
          )}

          {bitrate > 0 && (
            <>
              <div className="text-xs text-muted-foreground">bitrate</div>
              <div className="font-mono">{fmtBitrate(bitrate)}</div>
            </>
          )}

          {g2g != null && (
            <>
              <div className="text-xs text-muted-foreground">g2g</div>
              <div className="font-mono">{`${fmtNum(g2g, 0)} ms`}</div>
            </>
          )}
        </div>
      </CardContent>
    </Card>
  );
}

function TransportChip({
  value,
  onChange,
  isGround,
}: {
  value: Transport;
  onChange: (next: Transport) => void;
  isGround: boolean;
}) {
  const hint = isGround
    ? "HLS is preferred on ground (no Chrome decoder freeze). WebRTC has lower latency."
    : "WebRTC is preferred for low latency. HLS has ~3-5 s lag.";
  return (
    <div
      className="inline-flex items-center gap-1 text-[10px] uppercase tracking-wider"
      title={hint}
    >
      <span className="text-muted-foreground">transport</span>
      <div className="inline-flex rounded border border-border overflow-hidden">
        <button
          type="button"
          onClick={() => onChange("hls")}
          className={cn(
            "px-2 py-0.5 transition-colors",
            value === "hls"
              ? "bg-accent text-accent-foreground"
              : "bg-transparent text-muted-foreground hover:bg-accent/40",
          )}
        >
          HLS
        </button>
        <button
          type="button"
          onClick={() => onChange("whep")}
          className={cn(
            "px-2 py-0.5 transition-colors border-l border-border",
            value === "whep"
              ? "bg-accent text-accent-foreground"
              : "bg-transparent text-muted-foreground hover:bg-accent/40",
          )}
        >
          WebRTC
        </button>
      </div>
    </div>
  );
}
