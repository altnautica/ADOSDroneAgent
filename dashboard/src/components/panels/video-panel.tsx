import { Image as ImageIcon, Video, VideoOff } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";

import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { useSnapshot } from "@/hooks/use-snapshot";
import { useStatus } from "@/hooks/use-status";
import { useWfb } from "@/hooks/use-wfb";
import { fmtBitrate, fmtNum } from "@/lib/format";
import { capturedLabel, fetchSnapshot } from "@/lib/snapshot";
import { cn } from "@/lib/utils";
import { useProfile } from "@/shared/use-profile";
import { useVideoTransport } from "@/shared/use-video-transport";
import type { TransportKind } from "@/shared/video-transport";
import { useWakeLock } from "@/shared/wake-lock";

// Playback goes through the shared video transport: it reports `live` only
// once a frame is presented, re-dials a frozen feed once and steps to the next
// transport on a second freeze or a failure. The order is profile-driven and
// overridable from the UI:
//
//   ground_station → HLS first (~3-5 s latency, no decoder freeze)
//   drone           → WHEP first (~100-300 ms latency, local camera)
//
// HLS-first on ground is a deliberate trade-off: the WFB-rx → MediaMTX path
// can drop WHEP into a browser decoder-sync freeze after a few seconds in the
// field. When every transport fails, the panel shows a still snapshot from
// the agent while the transport keeps retrying.
type Transport = TransportKind;

/** True while the element is at least partly on screen. */
function useOnScreen(ref: React.RefObject<HTMLElement | null>): boolean {
  const [visible, setVisible] = useState(true);
  useEffect(() => {
    const el = ref.current;
    if (!el || typeof IntersectionObserver === "undefined") return;
    const io = new IntersectionObserver((entries) => {
      setVisible(entries.some((e) => e.isIntersecting));
    });
    io.observe(el);
    return () => io.disconnect();
  }, [ref]);
  return visible;
}

export function VideoPanel() {
  const status = useStatus();
  const snap = useSnapshot();
  const wfb = useWfb();
  const profile = useProfile();

  const panelRef = useRef<HTMLDivElement | null>(null);
  const videoRef = useRef<HTMLVideoElement | null>(null);
  const [retryToken, setRetryToken] = useState(0);
  // Object URL of a still frame grabbed from the live stream, fetched with the
  // credential when every live transport failed, and when the agent took it.
  const [snapshotUrl, setSnapshotUrl] = useState<string | null>(null);
  const [snapshotAt, setSnapshotAt] = useState<Date | null>(null);
  const [snapshotError, setSnapshotError] = useState(false);

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

  const isGround = profile === "ground_station";

  // Profile-driven default; the operator can override it with the Transport
  // chip for the life of the page.
  const defaultTransport: Transport = isGround ? "hls" : "whep";
  const [override, setOverride] = useState<Transport | null>(null);
  const preferredTransport = override ?? defaultTransport;

  const wfbPacketsReceived = wfb.data?.packets_received ?? 0;
  const wfbState = wfb.data?.state ?? "unknown";
  const wfbChannel = wfb.data?.actual_channel ?? null;
  const wfbStreaming = wfbPacketsReceived > 0;
  const waitingForWfb = isGround && !wfbStreaming;
  const wfbWaitDetail =
    `drone TX ${wfbState}` +
    (wfbChannel ? ` on ch ${wfbChannel}` : "");

  const pipelineRunning = pipelineState === "running";
  const haveAnyUrl = whepUrl.length > 0 || hlsUrl.length > 0;

  // The order of attempts, dropping any leg whose URL is empty.
  const order = useMemo(() => {
    const legs: Transport[] =
      preferredTransport === "hls" ? ["hls", "whep"] : ["whep", "hls"];
    return legs.filter((t) => (t === "hls" ? hlsUrl : whepUrl).length > 0);
  }, [preferredTransport, whepUrl, hlsUrl]);

  const onScreen = useOnScreen(panelRef);
  const canStream = pipelineRunning && order.length > 0 && profile !== null;
  const playing = canStream && onScreen;

  const feed = useVideoTransport(videoRef, {
    order,
    whepUrl,
    hlsUrl,
    reconnectKey: String(retryToken),
    enabled: playing,
  });
  // The screen stays awake only while a feed is being shown.
  useWakeLock(playing);

  const state: "idle" | "connecting" | "live" | "frozen" | "failed" = playing
    ? feed.state
    : "idle";
  const showSnapshot = state === "failed";

  // The still-frame fallback. Fetched (not an <img src>) so it carries the
  // dashboard's credential.
  useEffect(() => {
    if (!showSnapshot) return;
    const ac = new AbortController();
    let url: string | null = null;
    setSnapshotError(false);
    fetchSnapshot(ac.signal).then(
      (shot) => {
        url = URL.createObjectURL(shot.blob);
        setSnapshotUrl(url);
        setSnapshotAt(shot.capturedAt);
      },
      () => {
        if (!ac.signal.aborted) setSnapshotError(true);
      },
    );
    return () => {
      ac.abort();
      if (url) URL.revokeObjectURL(url);
      setSnapshotUrl(null);
      setSnapshotAt(null);
    };
  }, [showSnapshot, retryToken]);

  const isLive = state === "live";
  const transportLabel = feed.transport === "hls" ? "hls" : "webrtc";
  const badgeText =
    state === "live"
      ? `live · ${transportLabel}`
      : state === "frozen"
        ? `${transportLabel} · frozen`
        : state === "connecting"
          ? "connecting"
          : state === "failed"
            ? snapshotUrl
              ? "snapshot"
              : "error"
            : "idle";
  const badgeClass =
    state === "live"
      ? feed.transport === "hls"
        ? "border-warn/40 text-warn"
        : "border-ok/40 text-ok"
      : state === "frozen" || state === "failed"
        ? "border-destructive/40 text-destructive"
        : "border-muted-foreground/40 text-muted-foreground";
  const badgeTitle =
    state === "live" && feed.highLatency
      ? "WebRTC failed or froze, so playback fell back to HLS (several seconds of latency). WebRTC is retried in the background."
      : state === "live" && feed.transport === "hls"
        ? "HLS playback. Several seconds of latency vs WebRTC's 100-300 ms; no decoder freeze."
        : state === "frozen"
          ? "No new frame decoded recently; reconnecting."
          : undefined;

  return (
    <Card ref={panelRef}>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          <Video className={cn("h-3.5 w-3.5", isLive && "text-ok")} aria-hidden />
          Video
          <span
            className={cn(
              "ml-auto text-xs uppercase tracking-wider px-1.5 py-0.5 rounded border",
              badgeClass,
            )}
            title={badgeTitle}
          >
            {badgeText}
            {state === "live" && feed.highLatency ? " · high latency" : ""}
          </span>
        </CardTitle>
      </CardHeader>
      <CardContent className="space-y-3">
        {whepUrl && hlsUrl && (
          <TransportChip
            value={preferredTransport}
            onChange={setOverride}
            isGround={isGround}
          />
        )}

        <div className="relative aspect-video w-full rounded-md border border-border bg-letterbox overflow-hidden">
          <video
            ref={videoRef}
            className="absolute inset-0 h-full w-full object-contain"
            muted
            autoPlay
            playsInline
          />
          {showSnapshot && snapshotUrl && (
            <img
              src={snapshotUrl}
              alt="Last snapshot"
              className="absolute inset-0 h-full w-full object-contain"
            />
          )}
          {state !== "live" && state !== "frozen" && (
            <div className="absolute inset-0 flex flex-col items-center justify-center gap-2 bg-scrim text-xs text-muted-foreground">
              {state === "idle" && (
                <>
                  <VideoOff className="h-6 w-6 opacity-60" aria-hidden />
                  <div>
                    {waitingForWfb
                      ? "Waiting for WFB stream from drone."
                      : pipelineState === "no_camera"
                        ? "No camera detected."
                        : !haveAnyUrl
                          ? "No stream published."
                          : canStream && !onScreen
                            ? "Paused while off screen."
                            : "Pipeline idle."}
                  </div>
                  <div className="text-xs max-w-xs text-center">
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
                      <div className="text-xs mt-1 opacity-80">
                        {wfbWaitDetail}
                      </div>
                    </>
                  ) : (
                    <>connecting…</>
                  )}
                </div>
              )}
              {state === "failed" && snapshotUrl && (
                <div className="absolute bottom-2 left-2 right-2 flex items-center justify-between gap-2 bg-background/80 rounded px-2 py-1">
                  <div className="flex items-center gap-1 text-xs">
                    <ImageIcon className="h-3 w-3" aria-hidden />
                    Snapshot only — live feed unavailable
                    {snapshotAt && (
                      <span className="font-mono opacity-80">
                        · {capturedLabel(snapshotAt)}
                      </span>
                    )}
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
              {state === "failed" && !snapshotUrl && (
                <>
                  <VideoOff className="h-6 w-6 opacity-60" aria-hidden />
                  <div className="text-destructive max-w-md text-center">
                    {waitingForWfb
                      ? `No video. ${wfbWaitDetail}; WFB-rx received 0 packets.`
                      : snapshotError
                        ? `${feed.error ?? "Stream unavailable."} Snapshot also unavailable.`
                        : feed.error || "Stream unavailable."}
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
      className="inline-flex items-center gap-1 text-xs uppercase tracking-wider"
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
