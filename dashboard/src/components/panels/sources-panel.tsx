import { Layers } from "lucide-react";

import {
  Card,
  CardContent,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { useResource } from "@/hooks/use-resource";
import { fmtBitrate } from "@/lib/format";

// GET /api/v1/ground-station/wfb/receiver/combined — the receiver's combined
// FEC output. Every counter is null under `stale: true` (nothing is reporting).
interface ReceiverCombined {
  fragments_after_dedup: number | null;
  fec_repaired: number | null;
  output_kbps: number | null;
  up: boolean | null;
  stale: boolean;
}

// GET /api/v1/ground-station/wfb/receiver/relays — per-relay fragment counts.
// `relays` is null (not []) under `stale: true`.
interface ReceiverRelays {
  relays: { mac: string; last_seen_ms: number; fragments: number }[] | null;
  stale: boolean;
}

export function SourcesPanel() {
  const combined = useResource<ReceiverCombined>(
    "gs-receiver-combined",
    "/api/v1/ground-station/wfb/receiver/combined",
    2_000,
  );
  const relays = useResource<ReceiverRelays>(
    "gs-receiver-relays",
    "/api/v1/ground-station/wfb/receiver/relays",
    2_000,
  );

  const c = combined.data;
  const stale = c?.stale === true;
  const perSource = relays.data?.stale ? null : (relays.data?.relays ?? null);

  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          <Layers className="h-3.5 w-3.5" />
          Sources
          {stale && (
            <span className="ml-auto text-[10px] uppercase tracking-wider px-1.5 py-0.5 rounded border border-warn/40 text-warn">
              stale
            </span>
          )}
        </CardTitle>
      </CardHeader>
      <CardContent>
        {combined.isError ? (
          <p className="text-xs text-destructive">Could not read the receiver state.</p>
        ) : (
          <div className="grid grid-cols-2 gap-x-4 gap-y-1.5 text-sm">
            <div className="text-xs text-muted-foreground">combined output</div>
            <div className="font-mono">{fmtBitrate(c?.output_kbps)}</div>

            <div className="text-xs text-muted-foreground">fragments (dedup)</div>
            <div className="font-mono">{c?.fragments_after_dedup?.toLocaleString() ?? "—"}</div>

            <div className="text-xs text-muted-foreground">FEC repaired</div>
            <div className="font-mono">{c?.fec_repaired?.toLocaleString() ?? "—"}</div>

            <div className="text-xs text-muted-foreground">relays</div>
            <div className="font-mono">{perSource ? perSource.length : "—"}</div>
          </div>
        )}

        {perSource && perSource.length > 0 && (
          <ul className="pt-3 mt-3 border-t border-border/50 space-y-1 text-xs font-mono">
            {perSource.map((r) => (
              <li key={r.mac} className="flex justify-between gap-2 text-muted-foreground">
                <span className="truncate">{r.mac}</span>
                <span>{r.fragments.toLocaleString()} frags</span>
              </li>
            ))}
          </ul>
        )}
        {perSource && perSource.length === 0 && (
          <p className="text-xs text-muted-foreground pt-3 mt-3 border-t border-border/50">
            No relay is contributing right now.
          </p>
        )}
        {!perSource && !relays.isLoading && (
          <p className="text-xs text-muted-foreground pt-3 mt-3 border-t border-border/50">
            The receive loop is not reporting, so relay contributions are unknown.
          </p>
        )}
      </CardContent>
    </Card>
  );
}
