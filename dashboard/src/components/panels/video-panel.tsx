import { Image as ImageIcon, Video, VideoOff } from "lucide-react";
import { useEffect, useRef, useState } from "react";

import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { useSnapshot } from "@/hooks/use-snapshot";
import { useStatus } from "@/hooks/use-status";
import { fmtBitrate, fmtNum } from "@/lib/format";
import { startHls, type HlsSession } from "@/lib/hls";
import { startWhep, type WhepSession } from "@/lib/whep";

// State machine: try WHEP first; on failure (3s timeout or error), try
// HLS; if HLS also fails, fall back to a still snapshot. Final-error
// only when nothing works AND the pipeline says it's running — when
// the pipeline is idle the panel shows the friendly empty state instead.
type PlayerState =
  | "idle"
  | "whep-connecting"
  | "whep-live"
  | "whep-failed"
  | "hls-connecting"
  | "hls-live"
  | "snapshot"
  | "final-error";

const WHEP_TIMEOUT_MS = 5000;

export function VideoPanel() {
  const status = useStatus();
  const snap = useSnapshot();

  const videoRef = useRef<HTMLVideoElement | null>(null);
  const whepRef = useRef<WhepSession | null>(null);
  const hlsRef = useRef<HlsSession | null>(null);
  const [state, setState] = useState<PlayerState>("idle");
  const [error, setError] = useState<string | null>(null);
  const [retryToken, setRetryToken] = useState(0);

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

  const pipelineRunning = pipelineState === "running";
  const canStream = pipelineRunning && whepUrl.length > 0;

  function tearDown() {
    const w = whepRef.current;
    whepRef.current = null;
    if (w) w.close().catch(() => undefined);
    const h = hlsRef.current;
    hlsRef.current = null;
    if (h) h.close();
  }

  useEffect(() => {
    if (!canStream || !videoRef.current) {
      tearDown();
      setState("idle");
      setError(null);
      return;
    }

    const ac = new AbortController();
    let cancelled = false;
    setState("whep-connecting");
    setError(null);

    const tryFallback = (whepError: string) => {
      if (cancelled) return;
      if (!hlsUrl) {
        setState("final-error");
        setError(whepError);
        return;
      }
      setState("hls-connecting");
      startHls(hlsUrl, videoRef.current!).then((hlsRes) => {
        if (cancelled) return;
        if (hlsRes.ok && hlsRes.session) {
          hlsRef.current = hlsRes.session;
          setState("hls-live");
        } else {
          // Final fallback: render a still snapshot via the JPEG endpoint.
          setState("snapshot");
          setError(`WebRTC: ${whepError}; HLS: ${hlsRes.error}`);
        }
      });
    };

    // Race the WHEP handshake against a hard timeout. WHEP can hang
    // indefinitely on networks that drop UDP — bail at WHEP_TIMEOUT_MS
    // and fall through to HLS.
    const timeoutId = setTimeout(() => {
      if (cancelled) return;
      ac.abort();
      tryFallback("WHEP handshake timeout");
    }, WHEP_TIMEOUT_MS);

    startWhep(whepUrl, videoRef.current, ac.signal).then((res) => {
      clearTimeout(timeoutId);
      if (cancelled) return;
      if (res.ok && res.session) {
        whepRef.current = res.session;
        setState("whep-live");
      } else {
        tryFallback(res.error || "WebRTC handshake failed");
      }
    });

    return () => {
      cancelled = true;
      clearTimeout(timeoutId);
      ac.abort();
      tearDown();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [canStream, whepUrl, hlsUrl, retryToken]);

  const isLive = state === "whep-live" || state === "hls-live";
  const showOverlay = !isLive && state !== "snapshot";
  const showSnapshot = state === "snapshot";
  const stateBadge = stateLabel(state);

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
            className={`ml-auto text-[10px] uppercase tracking-wider px-1.5 py-0.5 rounded border ${
              state === "hls-live"
                ? "border-warn/40 text-warn"
                : isLive
                  ? "border-ok/40 text-ok"
                  : "border-muted-foreground/40 text-muted-foreground/80"
            }`}
            title={
              state === "hls-live"
                ? "HLS fallback. ~3-5s latency vs WHEP's 100-300ms."
                : undefined
            }
          >
            {stateBadge}
          </span>
        </CardTitle>
      </CardHeader>
      <CardContent className="space-y-3">
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
                    {pipelineState === "error"
                      ? "Pipeline error."
                      : pipelineState === "starting"
                        ? "Pipeline starting…"
                        : !whepUrl
                          ? "No camera detected."
                          : "Pipeline idle."}
                  </div>
                  <div className="text-[10px] max-w-xs text-center">
                    Plug in a camera and the agent will publish a WHEP stream
                    automatically.
                  </div>
                </>
              )}
              {state === "whep-connecting" && (
                <div>connecting (WebRTC)…</div>
              )}
              {state === "hls-connecting" && (
                <div>connecting (HLS fallback)…</div>
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
                    {error || "Stream unavailable."}
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

          <div className="text-xs text-muted-foreground">codec</div>
          <div className="font-mono">{codec || "—"}</div>

          <div className="text-xs text-muted-foreground">res</div>
          <div className="font-mono">{w && h ? `${w}×${h}` : "—"}</div>

          <div className="text-xs text-muted-foreground">fps</div>
          <div className="font-mono">{fps > 0 ? fmtNum(fps, 0) : "—"}</div>

          <div className="text-xs text-muted-foreground">bitrate</div>
          <div className="font-mono">{fmtBitrate(bitrate)}</div>

          <div className="text-xs text-muted-foreground">g2g</div>
          <div className="font-mono">
            {g2g != null ? `${fmtNum(g2g, 0)} ms` : "—"}
          </div>
        </div>
      </CardContent>
    </Card>
  );
}

function stateLabel(s: PlayerState): string {
  switch (s) {
    case "whep-live":
      return "live · webrtc";
    case "hls-live":
      return "live · hls";
    case "snapshot":
      return "snapshot";
    case "whep-connecting":
    case "hls-connecting":
      return "connecting";
    case "final-error":
      return "error";
    case "whep-failed":
      return "fallback";
    case "idle":
    default:
      return "idle";
  }
}
