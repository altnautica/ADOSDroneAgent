// Mesh — the distributed-RX role and self-healing mesh membership. The role
// summary (current / configured / supported) and the mesh health (carrier up,
// peer count, selected gateway, partition) come from the GS-status composite the
// shell already polls, so they never drift from the OLED. The neighbour and
// gateway detail comes from `GET /api/v1/ground-station/mesh`, which returns the
// raw mesh-state object when this node is a relay/receiver and a not-in-mesh
// note when it is a plain `direct` receiver. Read-only — a null reading renders a
// dash, never a fabricated value.

import { useCallback } from "react";

import { Panel, PanelHeader } from "@/components/ui/panel";
import { Dot, EmptyNote, Row, SectionHeader, StaleBadge, Tile, TileGrid, type Tone } from "@/components/ui/data";
import { useResource } from "@/hooks/use-resource";
import { useTelemetryContext } from "@/hooks/telemetry-context";
import { apiFetch } from "@/lib/api";
import { DASH, fmtInt } from "@/lib/format";

interface MeshNeighbor {
  orig?: string;
  mac?: string;
  address?: string;
  name?: string;
  tq?: number;
  quality?: number;
  link_quality?: number;
  last_seen?: number;
  last_seen_msecs?: number;
  [k: string]: unknown;
}
interface MeshGateway {
  orig?: string;
  mac?: string;
  address?: string;
  name?: string;
  selected?: boolean;
  [k: string]: unknown;
}
interface MeshState {
  neighbors?: MeshNeighbor[];
  gateways?: MeshGateway[];
  selected_gateway?: string | null;
  carrier?: string | null;
  mesh_id?: string | null;
  channel?: number | null;
  bat_iface?: string | null;
  partition?: boolean;
  [k: string]: unknown;
}

function identity(n: { orig?: string; mac?: string; address?: string; name?: string }): string {
  return n.orig ?? n.mac ?? n.address ?? n.name ?? DASH;
}

/** batman-adv transmit quality is 0..255; render it as a percentage. */
function quality(n: MeshNeighbor): string {
  const q = n.tq ?? n.quality ?? n.link_quality;
  if (q == null || !Number.isFinite(q)) return DASH;
  return q > 100 ? `${Math.round((q / 255) * 100)}%` : `${Math.round(q)}%`;
}

export function MeshScreen() {
  const { status } = useTelemetryContext();
  const mesh = useResource<MeshState>(
    useCallback((s) => apiFetch<MeshState>("/api/v1/ground-station/mesh", { signal: s }), []),
    1500,
  );

  const role = status?.role;
  const health = status?.mesh;
  const current = role?.current ?? DASH;
  const meshCapable = role?.mesh_capable ?? false;
  const inMesh = current === "relay" || current === "receiver";
  const carrierUp = health?.up ?? false;
  const carrierTone: Tone = inMesh ? (carrierUp ? "ok" : "err") : "muted";

  const state = mesh.data;
  const neighbors = state?.neighbors ?? [];
  const gateways = state?.gateways ?? [];
  const selectedGw = state?.selected_gateway ?? health?.selected_gateway ?? null;

  return (
    <Panel>
      <PanelHeader
        title="Mesh"
        right={
          <div className="flex items-center gap-[0.5rem]">
            <Dot tone={carrierTone} />
            <span className="text-[0.8rem] text-surface-foreground">{current}</span>
            <StaleBadge stale={mesh.stale && inMesh} />
          </div>
        }
      />

      <div className="flex flex-col gap-[0.15rem]">
        <SectionHeader>Role</SectionHeader>
        <Row label="Current" value={current} tone={inMesh ? "ok" : "muted"} />
        <Row label="Configured" value={role?.configured ?? DASH} />
        <Row label="Mesh-capable" left={<Dot tone={meshCapable ? "ok" : "muted"} />} value={meshCapable ? "yes" : "no"} />
        {role?.supported?.length ? (
          <Row label="Supported roles" value={role.supported.join(" · ")} mono={false} />
        ) : null}

        {inMesh ? (
          <>
            <SectionHeader>Mesh health</SectionHeader>
            <TileGrid>
              <Tile label="Carrier" value={carrierUp ? "up" : "down"} tone={carrierTone} />
              <Tile label="Peers" value={fmtInt(health?.peer_count)} />
              <Tile label="Partition" value={health?.partition ? "yes" : "no"} tone={health?.partition ? "warn" : "ok"} />
            </TileGrid>
            <Row label="Mesh ID" value={health?.mesh_id ?? state?.mesh_id ?? DASH} />
            <Row label="Carrier iface" value={state?.bat_iface ?? state?.carrier ?? DASH} />
            {state?.channel != null ? <Row label="Channel" value={`ch${state.channel}`} /> : null}

            <SectionHeader>Gateway</SectionHeader>
            {gateways.length ? (
              gateways.map((g, i) => (
                <Row
                  key={identity(g) + i}
                  label={identity(g)}
                  left={<Dot tone={g.selected || identity(g) === selectedGw ? "ok" : "muted"} />}
                  value={g.selected || identity(g) === selectedGw ? "selected" : ""}
                  tone="ok"
                />
              ))
            ) : (
              <Row label="Selected gateway" value={selectedGw ?? "none elected"} tone={selectedGw ? "ok" : "muted"} />
            )}

            <SectionHeader>Neighbours ({neighbors.length})</SectionHeader>
            {mesh.status === 404 ? (
              <EmptyNote>Not participating in a mesh.</EmptyNote>
            ) : neighbors.length ? (
              neighbors.map((n, i) => (
                <Row key={identity(n) + i} label={identity(n)} value={quality(n)} hint="link quality" />
              ))
            ) : (
              <EmptyNote>No mesh neighbours discovered yet.</EmptyNote>
            )}
          </>
        ) : (
          <EmptyNote>
            This node is a plain <span className="text-surface-foreground">direct</span> receiver — not in a mesh. Set the
            role to relay or receiver in Settings to join one.
          </EmptyNote>
        )}
      </div>
    </Panel>
  );
}
