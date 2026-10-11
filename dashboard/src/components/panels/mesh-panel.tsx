import { Network } from "lucide-react";
import { useTranslation } from "react-i18next";

import {
  Card,
  CardContent,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { useResource } from "@/hooks/use-resource";
import { useNodeIdentity } from "@/hooks/use-node-identity";
import { ApiError } from "@/shared/api-fetch";
import { fmtPct } from "@/shared/format";

// GET /api/v1/ground-station/mesh — the relay/receiver poll loop's mesh-state
// snapshot (crates/ados-groundlink mesh/state.rs), served only while fresh; the
// empty object when no current snapshot exists, 404 on a `direct` node.
interface MeshNeighbor {
  mac: string;
  iface?: string;
  /** batman-adv transmit quality, 0..255. */
  tq: number;
  last_seen_ms?: number;
}

interface MeshState {
  role?: string;
  up?: boolean;
  mesh_id?: string;
  bat_iface?: string;
  carrier?: string;
  neighbors?: MeshNeighbor[];
  selected_gateway?: string | null;
  partition?: boolean;
}

export function MeshPanel() {
  const { t } = useTranslation();
  const identity = useNodeIdentity();
  const mesh = useResource<MeshState>("gs-mesh", "/api/v1/ground-station/mesh", 2_000);

  const m = mesh.data;
  // `{}` means the agent holds no current snapshot: the poll loop is not
  // reporting, which is not the same as "no peers".
  const current = m != null && m.up !== undefined;
  const notInMesh = mesh.error instanceof ApiError && mesh.error.status === 404;
  const role = m?.role || identity.role;
  const neighbors = m?.neighbors ?? [];

  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          <Network className="h-3.5 w-3.5" />
          {t("panels.mesh.title")}
          <span className="ml-auto text-[10px] uppercase tracking-wider px-1.5 py-0.5 rounded border border-border text-muted-foreground">
            {role}
          </span>
        </CardTitle>
      </CardHeader>
      <CardContent className="space-y-2">
        {notInMesh ? (
          <p className="text-xs text-muted-foreground">{t("panels.mesh.notInMesh")}</p>
        ) : mesh.isError ? (
          <p className="text-xs text-destructive">{t("panels.mesh.readError")}</p>
        ) : !current ? (
          <p className="text-xs text-muted-foreground">
            {mesh.isLoading ? t("panels.mesh.loading") : t("panels.mesh.noSnapshot")}
          </p>
        ) : (
          <>
            <div className="grid grid-cols-2 gap-x-4 gap-y-1.5 text-sm">
              <div className="text-xs text-muted-foreground">{t("panels.mesh.carrier")}</div>
              <div className={`font-mono text-xs ${m.up ? "text-ok" : "text-destructive"}`}>
                {m.up ? t("panels.mesh.up") : t("panels.mesh.down")}
              </div>

              <div className="text-xs text-muted-foreground">{t("panels.mesh.partition")}</div>
              <div className={`font-mono text-xs ${m.partition ? "text-warn" : "text-ok"}`}>
                {m.partition ? t("panels.mesh.partitioned") : t("panels.mesh.joined")}
              </div>

              <div className="text-xs text-muted-foreground">{t("panels.mesh.gateway")}</div>
              <div className="font-mono text-xs">{m.selected_gateway ?? t("panels.mesh.noneElected")}</div>

              <div className="text-xs text-muted-foreground">{t("panels.mesh.meshId")}</div>
              <div className="font-mono text-xs">{m.mesh_id || "—"}</div>

              <div className="text-xs text-muted-foreground">{t("panels.mesh.peers")}</div>
              <div className="font-mono">{neighbors.length}</div>
            </div>

            {neighbors.length > 0 ? (
              <ul className="pt-2 border-t border-border/50 space-y-1 text-xs font-mono max-h-32 overflow-y-auto">
                {neighbors.map((n) => (
                  <li key={n.mac} className="flex justify-between gap-2 text-muted-foreground">
                    <span className="truncate">{n.mac}</span>
                    <span>{fmtPct((n.tq / 255) * 100)}</span>
                  </li>
                ))}
              </ul>
            ) : (
              <p className="pt-2 border-t border-border/50 text-xs text-muted-foreground">
                {t("panels.mesh.noNeighbours")}
              </p>
            )}
          </>
        )}
      </CardContent>
    </Card>
  );
}
