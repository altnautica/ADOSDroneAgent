import { Antenna } from "lucide-react";

import { Badge } from "@/components/ui/badge";
import {
  Card,
  CardContent,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { useWfb } from "@/hooks/use-wfb";
import { fmtBitrate, fmtNum } from "@/lib/format";

export function WfbTxPanel() {
  const wfb = useWfb();
  const w = wfb.data;

  const state = (w?.state ?? "unknown").toLowerCase();
  const iface = w?.interface ?? "";
  const channel = w?.channel ?? null;
  const freq = w?.frequency_mhz ?? null;
  const bw = w?.bandwidth_mhz ?? null;
  const txPower = w?.tx_power_dbm ?? null;
  const txPowerMax = w?.tx_power_max_dbm ?? null;
  const mcs = w?.mcs_index ?? null;
  const regDomain = w?.regulatory_domain ?? "";
  const bitrate = w?.bitrate_kbps ?? 0;
  const packetsSent = w?.packets_received ?? 0; // tx adapter reports its own packet counter on receive too
  const restarts = w?.restart_count ?? 0;

  const adapterMissing =
    state === "disabled" || (state === "error" && !iface);

  const badge = stateBadge(state);

  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          <Antenna className="h-3.5 w-3.5" />
          WFB Transmit
          {badge && (
            <Badge variant={badge.variant} className="font-normal ml-auto">
              {badge.label}
            </Badge>
          )}
        </CardTitle>
      </CardHeader>
      <CardContent>
        {adapterMissing ? (
          <p className="text-xs text-muted-foreground">
            WFB-tx is not running. Plug in an RTL8812EU dongle and set the
            channel + tx power in Settings — the agent starts the
            transmit pipeline automatically when both are present.
          </p>
        ) : (
          <div className="grid grid-cols-2 gap-x-4 gap-y-1.5 text-sm">
            <div className="text-xs text-muted-foreground">interface</div>
            <div className="font-mono">{iface || "—"}</div>

            <div className="text-xs text-muted-foreground">channel</div>
            <div className="font-mono">
              {channel != null
                ? `${channel}${freq ? ` (${fmtNum(freq, 0)} MHz)` : ""}`
                : "—"}
            </div>

            {bw != null && bw > 0 && (
              <>
                <div className="text-xs text-muted-foreground">bandwidth</div>
                <div className="font-mono">{fmtNum(bw, 0)} MHz</div>
              </>
            )}

            <div className="text-xs text-muted-foreground">tx power</div>
            <div className="font-mono">
              {txPower != null
                ? `${fmtNum(txPower, 0)} dBm${
                    txPowerMax != null ? ` / ${fmtNum(txPowerMax, 0)}` : ""
                  }`
                : "—"}
            </div>

            {mcs != null && (
              <>
                <div className="text-xs text-muted-foreground">MCS</div>
                <div className="font-mono">{mcs}</div>
              </>
            )}

            {regDomain && (
              <>
                <div className="text-xs text-muted-foreground">domain</div>
                <div className="font-mono">{regDomain}</div>
              </>
            )}

            <div className="text-xs text-muted-foreground">bitrate</div>
            <div className="font-mono">{fmtBitrate(bitrate)}</div>

            {packetsSent > 0 && (
              <>
                <div className="text-xs text-muted-foreground">packets</div>
                <div className="font-mono">{packetsSent.toLocaleString()}</div>
              </>
            )}

            {restarts > 0 && (
              <>
                <div className="text-xs text-muted-foreground">restarts</div>
                <div className="font-mono">{restarts}</div>
              </>
            )}
          </div>
        )}
      </CardContent>
    </Card>
  );
}

function stateBadge(
  state: string,
): { label: string; variant: "ok" | "warn" | "info" | "default" } | null {
  switch (state) {
    case "active":
    case "ready":
      return { label: state, variant: "ok" };
    case "connecting":
      return { label: "connecting", variant: "info" };
    case "error":
      return { label: "error", variant: "warn" };
    case "disabled":
      return { label: "disabled", variant: "default" };
    default:
      return state ? { label: state, variant: "default" } : null;
  }
}
