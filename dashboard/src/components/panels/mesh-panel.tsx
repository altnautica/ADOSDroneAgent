import { Network } from "lucide-react";

import {
  Card,
  CardContent,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { useResource } from "@/hooks/use-resource";
import { useStatus } from "@/hooks/use-status";
import { ApiError } from "@/lib/api";
import { fmtPercent } from "@/lib/format";

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
  const status = useStatus();
  const mesh = useResource<MeshState>("gs-mesh", "/api/v1/ground-station/mesh", 2_000);

  const m = mesh.data;
  // `{}` means the agent holds no current snapshot: the poll loop is not
  // reporting, which is not the same as "no peers".
  const current = m != null && m.up !== undefined;
  const notInMesh = mesh.error instanceof ApiError && mesh.error.status === 404;
  const role = m?.role || status.data?.ground_role || "—";
  const neighbors = m?.neighbors ?? [];

  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          <Network className="h-3.5 w-3.5" />
          Mesh
          <span className="ml-auto text-[10px] uppercase tracking-wider px-1.5 py-0.5 rounded border border-border text-muted-foreground">
            {role}
          </span>
        </CardTitle>
      </CardHeader>
      <CardContent className="space-y-2">
        {notInMesh ? (
          <p className="text-xs text-muted-foreground">This node is not in a mesh.</p>
        ) : mesh.isError ? (
          <p className="text-xs text-destructive">Could not read the mesh state.</p>
        ) : !current ? (
          <p className="text-xs text-muted-foreground">
            {mesh.isLoading ? "loading…" : "No current mesh snapshot. The mesh service is not reporting."}
          </p>
        ) : (
          <>
            <div className="grid grid-cols-2 gap-x-4 gap-y-1.5 text-sm">
              <div className="text-xs text-muted-foreground">carrier</div>
              <div className={`font-mono text-xs ${m.up ? "text-ok" : "text-destructive"}`}>
                {m.up ? "up" : "down"}
              </div>

              <div className="text-xs text-muted-foreground">partition</div>
              <div className={`font-mono text-xs ${m.partition ? "text-warn" : "text-ok"}`}>
                {m.partition ? "partitioned" : "joined"}
              </div>

              <div className="text-xs text-muted-foreground">gateway</div>
              <div className="font-mono text-xs">{m.selected_gateway ?? "none elected"}</div>

              <div className="text-xs text-muted-foreground">mesh id</div>
              <div className="font-mono text-xs">{m.mesh_id || "—"}</div>

              <div className="text-xs text-muted-foreground">peers</div>
              <div className="font-mono">{neighbors.length}</div>
            </div>

            {neighbors.length > 0 ? (
              <ul className="pt-2 border-t border-border/50 space-y-1 text-xs font-mono max-h-32 overflow-y-auto">
                {neighbors.map((n) => (
                  <li key={n.mac} className="flex justify-between gap-2 text-muted-foreground">
                    <span className="truncate">{n.mac}</span>
                    <span>{fmtPercent((n.tq / 255) * 100)}</span>
                  </li>
                ))}
              </ul>
            ) : (
              <p className="pt-2 border-t border-border/50 text-xs text-muted-foreground">
                No batman-adv neighbours discovered yet.
              </p>
            )}
          </>
        )}
      </CardContent>
    </Card>
  );
}
