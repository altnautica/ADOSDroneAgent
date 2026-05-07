import { Network as NetworkIcon } from "lucide-react";

import {
  Card,
  CardContent,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { useSnapshot } from "@/hooks/use-snapshot";
import { useStatus } from "@/hooks/use-status";
import { fmtRssi } from "@/lib/format";

export function NetworkPanel() {
  const snap = useSnapshot();
  const status = useStatus();

  const ips = snap.data?.network?.ip ?? status.data?.network?.ip_addresses ?? {};
  const uplink = snap.data?.network?.uplink ?? status.data?.network?.uplink_kind;
  const rssi =
    typeof snap.data?.network?.rssi_dbm === "number"
      ? snap.data?.network?.rssi_dbm
      : (status.data?.network?.rssi_dbm ?? null);
  const interfaces = Object.entries(ips);

  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          <NetworkIcon className="h-3.5 w-3.5" />
          Network
        </CardTitle>
      </CardHeader>
      <CardContent className="space-y-2">
        <div className="flex items-baseline justify-between text-sm">
          <span className="text-xs text-muted-foreground">uplink</span>
          <span className="font-mono">{uplink ?? "—"}</span>
        </div>
        <div className="flex items-baseline justify-between text-sm">
          <span className="text-xs text-muted-foreground">rssi</span>
          <span className="font-mono">{fmtRssi(rssi)}</span>
        </div>

        {interfaces.length > 0 ? (
          <ul className="space-y-1 pt-2 border-t border-border/50">
            {interfaces.map(([iface, addr]) => (
              <li key={iface} className="flex items-baseline justify-between">
                <span className="font-mono text-xs text-muted-foreground">
                  {iface}
                </span>
                <span className="font-mono text-xs">{addr || "—"}</span>
              </li>
            ))}
          </ul>
        ) : (
          <p className="text-xs text-muted-foreground pt-2 border-t border-border/50">
            no interfaces reported
          </p>
        )}
      </CardContent>
    </Card>
  );
}
