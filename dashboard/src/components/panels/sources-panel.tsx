import { Layers } from "lucide-react";

import {
  Card,
  CardContent,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { useSnapshot } from "@/hooks/use-snapshot";
import { fmtBitrate } from "@/lib/format";

export function SourcesPanel() {
  const snap = useSnapshot();
  const s = snap.data?.sources;

  const aggregated = s?.aggregated_kbps ?? null;
  const combined = s?.frames_combined ?? null;
  const dedup = s?.frames_dedup ?? null;
  const perSource = s?.per_source ?? [];

  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          <Layers className="h-3.5 w-3.5" />
          Sources
        </CardTitle>
      </CardHeader>
      <CardContent>
        <div className="grid grid-cols-2 gap-x-4 gap-y-1.5 text-sm">
          <div className="text-xs text-muted-foreground">aggregated</div>
          <div className="font-mono">{fmtBitrate(aggregated)}</div>

          <div className="text-xs text-muted-foreground">frames combined</div>
          <div className="font-mono">
            {combined != null ? combined.toLocaleString() : "—"}
          </div>

          <div className="text-xs text-muted-foreground">frames dedup</div>
          <div className="font-mono">
            {dedup != null ? dedup.toLocaleString() : "—"}
          </div>

          <div className="text-xs text-muted-foreground">per-source</div>
          <div className="font-mono">{perSource.length}</div>
        </div>

        {perSource.length === 0 && (
          <p className="text-xs text-muted-foreground pt-3 mt-3 border-t border-border/50">
            no relay nodes contributing yet. as relays come online and forward
            their drone link, they'll appear here with per-source FEC stats.
          </p>
        )}
      </CardContent>
    </Card>
  );
}
