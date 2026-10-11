import { Network as NetworkIcon } from "lucide-react";
import { useTranslation } from "react-i18next";

import {
  Card,
  CardContent,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { useStatus } from "@/hooks/use-status";
import { fmtDbm } from "@/shared/format";

export function NetworkPanel() {
  const { t } = useTranslation();
  const status = useStatus();

  const ips = status.data?.network?.ip_addresses ?? {};
  const uplink = status.data?.network?.uplink_kind;
  const rssi = status.data?.network?.rssi_dbm ?? null;
  const interfaces = Object.entries(ips);

  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          <NetworkIcon className="h-3.5 w-3.5" />
          {t("panels.network.title")}
        </CardTitle>
      </CardHeader>
      <CardContent className="space-y-2">
        <div className="flex items-baseline justify-between text-sm">
          <span className="text-xs text-muted-foreground">{t("panels.network.uplink")}</span>
          <span className="font-mono">{uplink ?? "—"}</span>
        </div>
        <div className="flex items-baseline justify-between text-sm">
          <span className="text-xs text-muted-foreground">{t("panels.network.rssi")}</span>
          <span className="font-mono">{fmtDbm(rssi)}</span>
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
            {t("panels.network.noInterfaces")}
          </p>
        )}
      </CardContent>
    </Card>
  );
}
