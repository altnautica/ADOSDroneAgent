import { Video, VideoOff } from "lucide-react";
import { useEffect, useRef, useState } from "react";

import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { useSnapshot } from "@/hooks/use-snapshot";
import { useStatus } from "@/hooks/use-status";
import { fmtBitrate, fmtNum } from "@/lib/format";
import { startWhep, type WhepSession } from "@/lib/whep";

type PlayerState = "idle" | "connecting" | "live" | "error";

export function VideoPanel() {
  const status = useStatus();
  const snap = useSnapshot();

  const videoRef = useRef<HTMLVideoElement | null>(null);
  const sessionRef = useRef<WhepSession | null>(null);
  const [state, setState] = useState<PlayerState>("idle");
  const [error, setError] = useState<string | null>(null);
  const [retryToken, setRetryToken] = useState(0);

  const whepUrl = status.data?.video?.whep_url ?? "";
  const v = snap.data?.video;
  const codec = v?.codec ?? "";
  const w = v?.width ?? 0;
  const h = v?.height ?? 0;
  const fps = v?.fps ?? 0;
  const bitrate = v?.bitrate_kbps ?? 0;
  const pipelineState = v?.state ?? "unknown";
  const g2g = v?.glass_to_glass_ms;

  // Only attempt the WHEP handshake when the agent says the pipeline
  // is actively publishing. Otherwise mediamtx's "no source" magenta
  // test pattern leaks through and looks like a broken stream.
  const pipelineRunning = pipelineState === "running";
  const canStream = pipelineRunning && whepUrl.length > 0;

  useEffect(() => {
    if (!canStream || !videoRef.current) {
      // Tear down any prior session if the pipeline drops out.
      const prior = sessionRef.current;
      sessionRef.current = null;
      if (prior) prior.close().catch(() => undefined);
      setState("idle");
      setError(null);
      return;
    }

    const ac = new AbortController();
    let cancelled = false;
    setState("connecting");
    setError(null);

    startWhep(whepUrl, videoRef.current, ac.signal).then((res) => {
      if (cancelled) return;
      if (res.ok && res.session) {
        sessionRef.current = res.session;
        setState("live");
      } else {
        setState("error");
        setError(res.error || "WebRTC handshake failed.");
      }
    });

    return () => {
      cancelled = true;
      ac.abort();
      const session = sessionRef.current;
      sessionRef.current = null;
      if (session) {
        session.close().catch(() => undefined);
      }
    };
  }, [canStream, whepUrl, retryToken]);

  const showOverlay = state !== "live";

  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          {state === "live" ? (
            <Video className="h-3.5 w-3.5 text-emerald-500" />
          ) : (
            <Video className="h-3.5 w-3.5" />
          )}
          Video
          <span className="ml-auto text-[10px] uppercase tracking-wider text-muted-foreground/80">
            {state}
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
          {showOverlay && (
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
              {state === "connecting" && <div>connecting…</div>}
              {state === "error" && (
                <>
                  <VideoOff className="h-6 w-6 opacity-60" />
                  <div className="text-red-300/90">
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
