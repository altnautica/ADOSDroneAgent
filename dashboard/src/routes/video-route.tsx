import { Camera, RefreshCw } from "lucide-react";
import { useState } from "react";

import { PageShell } from "@/components/page-shell";
import { VideoPanel } from "@/components/panels/video-panel";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { useResource } from "@/hooks/use-resource";
import { fmtBitrate, fmtNum } from "@/lib/format";

interface VideoCamerasResponse {
  cameras: Array<{
    device_path: string;
    type: string;
    label: string;
    width: number;
    height: number;
  }>;
  assignments: Record<string, string>;
}

interface VideoConfigResponse {
  radio?: {
    channel?: number;
    band?: string;
    mcs_index?: number;
    tx_power_dbm?: number;
    preset?: string;
  };
  encoder?: {
    bitrate_kbps?: number;
    width?: number;
    height?: number;
    fps?: number;
    codec?: string;
  };
}

interface VideoLatencyResponse {
  latency_ms?: number | null;
  ewma_ms?: number | null;
  pipeline_latency_ms?: number | null;
  samples?: number;
  source?: string;
}

export function VideoRoute() {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const cameras = useResource<VideoCamerasResponse>(
    "video-cameras",
    "/api/video/cameras",
    15_000,
  );
  const config = useResource<VideoConfigResponse>(
    "video-config",
    "/api/video/config",
    15_000,
  );
  const latency = useResource<VideoLatencyResponse>(
    "video-latency",
    "/api/video/latency",
    3_000,
  );

  const snapshot = async () => {
    setBusy(true);
    setError(null);
    try {
      const res = await fetch("/api/video/snapshot.jpg", {
        cache: "no-store",
      });
      if (!res.ok) throw new Error(`snapshot http ${res.status}`);
      const blob = await res.blob();
      const url = URL.createObjectURL(blob);
      const a = document.createElement("a");
      a.href = url;
      a.download = `ados-snapshot-${Date.now()}.jpg`;
      a.click();
      URL.revokeObjectURL(url);
    } catch (e) {
      setError(e instanceof Error ? e.message : "snapshot failed");
    } finally {
      setBusy(false);
    }
  };

  const rescan = async () => {
    setBusy(true);
    setError(null);
    try {
      // Re-probe cameras via the config fetch path; the agent rescans
      // /dev/video* on every poll already, so we just refetch.
      await Promise.all([cameras.refetch(), config.refetch()]);
    } catch (e) {
      setError(e instanceof Error ? e.message : "rescan failed");
    } finally {
      setBusy(false);
    }
  };

  const enc = config.data?.encoder;
  const radio = config.data?.radio;
  const camList = cameras.data?.cameras ?? [];
  const latencyMs = latency.data?.latency_ms;
  const ewma = latency.data?.ewma_ms;
  const samples = latency.data?.samples ?? 0;

  return (
    <PageShell
      title="Video"
      blurb="Live stream, encoder configuration, camera assignment, and latency probe."
      rightAction={
        <div className="flex items-center gap-2">
          <Button variant="outline" size="sm" onClick={rescan} disabled={busy}>
            <RefreshCw className="h-3.5 w-3.5" /> Rescan
          </Button>
          <Button variant="outline" size="sm" onClick={snapshot} disabled={busy}>
            <Camera className="h-3.5 w-3.5" /> Snapshot
          </Button>
        </div>
      }
    >
      <VideoPanel />

      <div className="grid gap-4 md:grid-cols-2">
        <Card>
          <CardHeader>
            <CardTitle>Encoder</CardTitle>
          </CardHeader>
          <CardContent>
            <div className="grid grid-cols-2 gap-x-4 gap-y-1.5 text-sm">
              <div className="text-xs text-muted-foreground">codec</div>
              <div className="font-mono">{enc?.codec ?? "—"}</div>
              <div className="text-xs text-muted-foreground">resolution</div>
              <div className="font-mono">
                {enc?.width && enc?.height ? `${enc.width}×${enc.height}` : "—"}
              </div>
              <div className="text-xs text-muted-foreground">fps</div>
              <div className="font-mono">
                {enc?.fps != null ? fmtNum(enc.fps, 0) : "—"}
              </div>
              <div className="text-xs text-muted-foreground">bitrate</div>
              <div className="font-mono">{fmtBitrate(enc?.bitrate_kbps ?? null)}</div>
            </div>
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle>Radio</CardTitle>
          </CardHeader>
          <CardContent>
            <div className="grid grid-cols-2 gap-x-4 gap-y-1.5 text-sm">
              <div className="text-xs text-muted-foreground">channel</div>
              <div className="font-mono">{radio?.channel ?? "—"}</div>
              <div className="text-xs text-muted-foreground">band</div>
              <div className="font-mono">{radio?.band ?? "—"}</div>
              <div className="text-xs text-muted-foreground">MCS index</div>
              <div className="font-mono">{radio?.mcs_index ?? "—"}</div>
              <div className="text-xs text-muted-foreground">tx power</div>
              <div className="font-mono">
                {radio?.tx_power_dbm != null ? `${radio.tx_power_dbm} dBm` : "—"}
              </div>
              <div className="text-xs text-muted-foreground">preset</div>
              <div className="font-mono">{radio?.preset ?? "—"}</div>
            </div>
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle>Latency</CardTitle>
          </CardHeader>
          <CardContent>
            <div className="grid grid-cols-2 gap-x-4 gap-y-1.5 text-sm">
              <div className="text-xs text-muted-foreground">latest</div>
              <div className="font-mono">
                {latencyMs != null ? `${fmtNum(latencyMs, 0)} ms` : "—"}
              </div>
              <div className="text-xs text-muted-foreground">ewma</div>
              <div className="font-mono">
                {ewma != null ? `${fmtNum(ewma, 0)} ms` : "—"}
              </div>
              <div className="text-xs text-muted-foreground">samples</div>
              <div className="font-mono">{samples}</div>
              <div className="text-xs text-muted-foreground">source</div>
              <div className="font-mono">{latency.data?.source ?? "—"}</div>
            </div>
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle>Cameras detected</CardTitle>
          </CardHeader>
          <CardContent>
            {camList.length === 0 ? (
              <p className="text-sm text-muted-foreground">
                No cameras detected. Plug in a USB UVC camera or a MIPI-CSI
                module and Rescan.
              </p>
            ) : (
              <ul className="space-y-2 text-sm">
                {camList.map((cam) => (
                  <li
                    key={cam.device_path}
                    className="flex items-center justify-between gap-2 border-b border-border/40 pb-2 last:border-b-0 last:pb-0"
                  >
                    <div className="min-w-0">
                      <div className="font-medium truncate">{cam.label}</div>
                      <div className="text-xs text-muted-foreground font-mono">
                        {cam.device_path} · {cam.type}
                        {cam.width && cam.height
                          ? ` · ${cam.width}×${cam.height}`
                          : ""}
                      </div>
                    </div>
                    <Badge variant="outline">{cam.type}</Badge>
                  </li>
                ))}
              </ul>
            )}
          </CardContent>
        </Card>
      </div>

      {error && (
        <p className="text-sm text-destructive">{error}</p>
      )}
    </PageShell>
  );
}
