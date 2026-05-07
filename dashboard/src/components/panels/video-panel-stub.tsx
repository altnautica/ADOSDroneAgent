import { Video } from "lucide-react";

import {
  Card,
  CardContent,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { useSnapshot } from "@/hooks/use-snapshot";
import { fmtBitrate, fmtNum } from "@/lib/format";

// Phase 5 brings the WebRTC WHEP player + HLS / snapshot fallbacks +
// fullscreen + 60s bitrate sparkline. For v0.14.1 this panel just
// surfaces the live transport metadata so we know the pipeline is
// healthy without actually painting frames.
export function VideoPanelStub() {
  const snap = useSnapshot();
  const v = snap.data?.video;

  const codec = v?.codec ?? "";
  const w = v?.width ?? 0;
  const h = v?.height ?? 0;
  const fps = v?.fps ?? 0;
  const bitrate = v?.bitrate_kbps ?? 0;
  const state = v?.state ?? "unknown";
  const g2g = v?.glass_to_glass_ms;

  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          <Video className="h-3.5 w-3.5" />
          Video
        </CardTitle>
      </CardHeader>
      <CardContent className="space-y-3">
        <div className="aspect-video w-full rounded-md border border-dashed border-border bg-muted/30 flex items-center justify-center text-xs text-muted-foreground">
          live player lands in v0.14.4
        </div>

        <div className="grid grid-cols-2 gap-x-4 gap-y-1.5 text-sm">
          <div className="text-xs text-muted-foreground">state</div>
          <div className="font-mono">{state}</div>

          <div className="text-xs text-muted-foreground">codec</div>
          <div className="font-mono">{codec || "—"}</div>

          <div className="text-xs text-muted-foreground">res</div>
          <div className="font-mono">
            {w && h ? `${w}×${h}` : "—"}
          </div>

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
