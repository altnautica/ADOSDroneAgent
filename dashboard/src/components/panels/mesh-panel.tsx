import { Network } from "lucide-react";

import {
  Card,
  CardContent,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { useSnapshot } from "@/hooks/use-snapshot";
import { useStatus } from "@/hooks/use-status";

export function MeshPanel() {
  const snap = useSnapshot();
  const status = useStatus();

  const m = snap.data?.mesh;
  const role =
    m?.role ||
    status.data?.ground_role ||
    "—";

  const peers = m?.batman_peers ?? [];
  const gateway = m?.gateway_node ?? null;
  const partition = m?.partition_state ?? null;
  const meshAddr = m?.mesh_addr ?? null;

  const partitionTone =
    partition == null
      ? "muted"
      : partition === "healthy" || partition === "ok"
        ? "ok"
        : partition === "isolated" || partition === "split"
          ? "err"
          : "warn";

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
        <div className="grid grid-cols-2 gap-x-4 gap-y-1.5 text-sm">
          <div className="text-xs text-muted-foreground">partition</div>
          <div
            className={`font-mono text-xs ${
              partitionTone === "ok"
                ? "text-ok"
                : partitionTone === "err"
                  ? "text-destructive"
                  : partitionTone === "warn"
                    ? "text-warn"
                    : ""
            }`}
          >
            {partition ?? "—"}
          </div>

          <div className="text-xs text-muted-foreground">gateway</div>
          <div className="font-mono text-xs">{gateway ?? "—"}</div>

          <div className="text-xs text-muted-foreground">mesh addr</div>
          <div className="font-mono text-xs">{meshAddr ?? "—"}</div>

          <div className="text-xs text-muted-foreground">peers</div>
          <div className="font-mono">{peers.length}</div>
        </div>

        {peers.length > 0 ? (
          <ul className="pt-2 border-t border-border/50 space-y-1 text-xs font-mono max-h-32 overflow-y-auto">
            {peers.map((peer, i) => {
              const text =
                typeof peer === "string"
                  ? peer
                  : peer != null
                    ? JSON.stringify(peer)
                    : `peer ${i}`;
              return (
                <li
                  key={i}
                  className="truncate text-muted-foreground"
                  title={text}
                >
                  {text}
                </li>
              );
            })}
          </ul>
        ) : (
          <p className="pt-2 border-t border-border/50 text-xs text-muted-foreground">
            no batman-adv peers reported. mesh requires a second USB dongle on
            the relay or receiver node.
          </p>
        )}
      </CardContent>
    </Card>
  );
}
