// The node's one-glance status row on the System screen: FC/MAV, hardware,
// video, network, cloud posture and pairing, built by the pure `buildTiles`
// rules. Polls its own reads, so the System screen mounts it with no props.

import { useCallback } from "react";

import { Tile, TileGrid } from "@/components/ui/data";
import { buildTiles } from "@/lib/status-tiles";
import type { Tone } from "@/lib/tone";
import { apiFetch } from "@/shared/api-fetch";
import type { SetupStatus } from "@/shared/agent-api.generated";
import type { AgentHeartbeat, DashboardSnapshot } from "@/shared/api/types";
import type { Severity } from "@/shared/format";
import "@/shared/i18n";
import { useResource } from "@/shared/use-resource";

const SEVERITY_TONE: Record<Severity, Tone | undefined> = {
  ok: "ok",
  warn: "warn",
  err: "err",
  info: undefined,
  idle: "muted",
};

interface PairingLite {
  paired?: boolean;
  pairing_code?: string | null;
}

export function StatusTilesCard() {
  const snapshot = useResource(
    useCallback((s: AbortSignal) => apiFetch<DashboardSnapshot>("/api/v1/dashboard/snapshot", { signal: s }), []),
    2000,
  );
  const setup = useResource(
    useCallback((s: AbortSignal) => apiFetch<SetupStatus>("/api/v1/setup/status", { signal: s }), []),
    20_000,
  );
  const heartbeat = useResource(
    useCallback((s: AbortSignal) => apiFetch<AgentHeartbeat>("/api/status", { signal: s }), []),
    5000,
  );
  const pairing = useResource(
    useCallback((s: AbortSignal) => apiFetch<PairingLite>("/api/pairing/info", { signal: s }), []),
    5000,
  );

  const loading = snapshot.data == null && !snapshot.ready;
  const tiles = buildTiles({
    snapshot: snapshot.data ?? undefined,
    status: setup.data ?? undefined,
    fcVariant: heartbeat.data?.fcVariant,
    fcReachable: heartbeat.data?.fcReachable,
    pairing:
      pairing.data && typeof pairing.data.paired === "boolean"
        ? { paired: pairing.data.paired, pairing_code: pairing.data.pairing_code ?? null }
        : undefined,
  });

  return (
    <TileGrid>
      {tiles.map((tile) => (
        <Tile
          key={tile.label}
          label={tile.label}
          value={loading ? "…" : tile.value}
          hint={<span title={tile.title}>{loading ? "—" : tile.sub}</span>}
          tone={SEVERITY_TONE[tile.severity]}
        />
      ))}
    </TileGrid>
  );
}
