import { useHeartbeat } from "@/hooks/use-heartbeat";
import { usePairingInfo } from "@/hooks/use-pairing";
import { useSnapshot } from "@/hooks/use-snapshot";
import { useStatus } from "@/hooks/use-status";
import { buildTiles, type Tile } from "@/lib/status-tiles";
import { cn } from "@/lib/utils";
import { severityClasses } from "@/shared/format";

function StatusTile({ tile, isLoading }: { tile: Tile; isLoading: boolean }) {
  const Icon = tile.icon;
  const sev = severityClasses(tile.severity);

  return (
    <div
      className="rounded-lg border border-border bg-card p-3 flex flex-col gap-1"
      title={tile.title}
    >
      <div className="flex items-center justify-between text-[11px] font-medium uppercase tracking-wider text-muted-foreground">
        <span className="inline-flex items-center gap-1.5">
          <Icon className={cn("h-3 w-3", sev.text)} />
          {tile.label}
        </span>
        <span className={cn("h-1.5 w-1.5 rounded-full", sev.dot)} />
      </div>
      <div className="font-mono text-lg font-medium leading-tight">
        {isLoading ? "…" : tile.value}
      </div>
      <div className="text-xs text-muted-foreground font-mono truncate">
        {isLoading ? "—" : tile.sub}
      </div>
    </div>
  );
}

export function StatusTiles() {
  const snap = useSnapshot();
  const status = useStatus();
  const heartbeat = useHeartbeat();
  const pairing = usePairingInfo();
  const isLoading = snap.isLoading && !snap.data;
  const ts = buildTiles({
    snapshot: snap.data,
    status: status.data,
    fcVariant: heartbeat.data?.fcVariant,
    fcReachable: heartbeat.data?.fcReachable,
    pairing: pairing.data,
  });

  return (
    <div className="grid grid-cols-2 sm:grid-cols-3 lg:grid-cols-6 gap-3">
      {ts.map((t) => (
        <StatusTile key={t.label} tile={t} isLoading={isLoading} />
      ))}
    </div>
  );
}
