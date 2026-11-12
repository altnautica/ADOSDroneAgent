// Mesh — the distributed-RX role and self-healing mesh membership. The role
// summary (current / configured / supported) and the mesh health (carrier up,
// peer count, selected gateway, partition) come from the GS-status composite the
// shell already polls, so they never drift from the OLED. The neighbour and
// gateway detail comes from `GET /api/v1/ground-station/mesh`: the mesh-state
// object while a snapshot is current, `{}` while the poll loop is not reporting
// (which is not "no peers"), and 404 on a plain `direct` receiver. Read-only — a
// null reading renders a dash, never a fabricated value.

import { useCallback } from "react";
import { useTranslation } from "react-i18next";

import { ConsoleScaffold } from "@/components/ui/console-scaffold";
import { Dot, EmptyNote, ErrorNote, Row, SectionHeader, StaleBadge, Tile, TileGrid } from "@/components/ui/data";
import { meshViewKind } from "@/lib/ground-station";
import { roleWordKey } from "@/lib/profile-words";
import type { Tone } from "@/lib/tone";
import { apiFetch } from "@/shared/api-fetch";
import { DASH, fmtInt, fmtTq } from "@/shared/format";
import "@/shared/i18n";
import { useResource } from "@/shared/use-resource";
import { useStatusStore } from "@/stores/status-store";

/** One `mesh-state` neighbour (crates/ados-groundlink mesh/state.rs). */
interface MeshNeighbor {
  mac?: string;
  iface?: string;
  tq?: number;
  last_seen_ms?: number;
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
  up?: boolean;
  role?: string;
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

export function MeshScreen() {
  const { t } = useTranslation();
  const status = useStatusStore((s) => s.status);
  const mesh = useResource<MeshState>(
    useCallback((s) => apiFetch<MeshState>("/api/v1/ground-station/mesh", { signal: s }), []),
    2000,
  );

  const role = status?.role;
  const health = status?.mesh;
  const current = role?.current ?? mesh.data?.role ?? null;
  // A role as a word; one outside the set reads unrecognised.
  const roleText = (r: string | null | undefined) => (r == null ? DASH : t(roleWordKey(r)));
  // Null until the status read names it: never a definite "no".
  const meshCapable = role?.mesh_capable ?? null;
  const state = mesh.data;
  const kind = meshViewKind({ data: state, httpStatus: mesh.status, error: mesh.error, role: current });

  const carrierUp = state?.up ?? health?.up ?? false;
  const carrierTone: Tone = kind === "ok" ? (carrierUp ? "ok" : "err") : "muted";
  const neighbors = state?.neighbors ?? [];
  const gateways = state?.gateways ?? [];
  const selectedGw = state?.selected_gateway ?? health?.selected_gateway ?? null;
  const partition = state?.partition ?? health?.partition ?? false;

  let body;
  if (kind === "not-in-mesh") {
    body = <EmptyNote>{t("cockpit.mesh.direct")}</EmptyNote>;
  } else if (kind === "error") {
    body = <ErrorNote onRetry={mesh.refresh}>{t("panels.mesh.readError")}</ErrorNote>;
  } else if (kind === "loading") {
    body = <EmptyNote>{t("panels.mesh.loading")}</EmptyNote>;
  } else if (kind === "no-snapshot") {
    body = <EmptyNote>{t("panels.mesh.noSnapshot")}</EmptyNote>;
  } else {
    body = (
      <>
        <SectionHeader>{t("cockpit.mesh.health")}</SectionHeader>
        <TileGrid>
          <Tile
            label={t("panels.mesh.carrier")}
            value={carrierUp ? t("panels.mesh.up") : t("panels.mesh.down")}
            tone={carrierTone}
          />
          <Tile label={t("panels.mesh.peers")} value={fmtInt(neighbors.length)} />
          <Tile
            label={t("panels.mesh.partition")}
            value={partition ? t("panels.mesh.partitioned") : t("panels.mesh.joined")}
            tone={partition ? "warn" : "ok"}
          />
        </TileGrid>
        <Row label={t("panels.mesh.meshId")} value={state?.mesh_id ?? health?.mesh_id ?? DASH} />
        <Row label={t("cockpit.mesh.carrierIface")} value={state?.bat_iface ?? state?.carrier ?? DASH} />
        {state?.channel != null ? <Row label={t("cockpit.mesh.channel")} value={`ch${state.channel}`} /> : null}

        <SectionHeader>{t("panels.mesh.gateway")}</SectionHeader>
        {gateways.length ? (
          gateways.map((g, i) => {
            const selected = g.selected === true || identity(g) === selectedGw;
            return (
              <Row
                key={identity(g) + i}
                label={identity(g)}
                left={<Dot tone={selected ? "ok" : "muted"} />}
                value={selected ? t("cockpit.mesh.selected") : ""}
                tone="ok"
              />
            );
          })
        ) : (
          <Row
            label={t("cockpit.mesh.selectedGateway")}
            value={selectedGw ?? t("panels.mesh.noneElected")}
            tone={selectedGw ? "ok" : "muted"}
          />
        )}

        <SectionHeader>{t("cockpit.mesh.neighbours", { count: neighbors.length })}</SectionHeader>
        {neighbors.length ? (
          neighbors.map((n, i) => (
            <Row key={identity(n) + i} label={identity(n)} value={fmtTq(n.tq)} hint={t("cockpit.mesh.linkQuality")} />
          ))
        ) : (
          <EmptyNote>{t("panels.mesh.noNeighbours")}</EmptyNote>
        )}
      </>
    );
  }

  return (
    <ConsoleScaffold stale={mesh.stale && kind === "ok"}
      title={t("panels.mesh.title")}
      actions={
        <span className="flex items-center gap-[0.5rem]">
          <Dot tone={carrierTone} />
          <span className="text-[0.8rem] text-surface-foreground">{roleText(current)}</span>
          <StaleBadge stale={mesh.stale && kind === "ok"} />
        </span>
      }
    >
      <div className="flex flex-col gap-[0.15rem]">
        <SectionHeader>{t("cockpit.mesh.role")}</SectionHeader>
        <Row label={t("cockpit.mesh.current")} value={roleText(current)} tone={kind === "ok" ? "ok" : "muted"} />
        <Row label={t("cockpit.mesh.configured")} value={roleText(role?.configured)} />
        <Row
          label={t("cockpit.mesh.capable")}
          left={<Dot tone={meshCapable ? "ok" : "muted"} />}
          value={meshCapable == null ? DASH : meshCapable ? t("cockpit.mesh.yes") : t("cockpit.mesh.no")}
        />
        {role?.supported?.length ? (
          <Row label={t("cockpit.mesh.supported")} value={role.supported.map(roleText).join(" · ")} mono={false} />
        ) : null}
        {body}
      </div>
    </ConsoleScaffold>
  );
}
