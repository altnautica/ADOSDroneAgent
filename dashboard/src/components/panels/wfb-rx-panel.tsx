import { Antenna } from "lucide-react";

import { Badge } from "@/components/ui/badge";
import {
  Card,
  CardContent,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { useWfb } from "@/hooks/use-wfb";
import { fmtBitrate, fmtNum, fmtRssi } from "@/lib/format";

export function WfbRxPanel() {
  const wfb = useWfb();
  const w = wfb.data;

  const state = (w?.state ?? "unknown").toLowerCase();
  const iface = w?.interface ?? "";
  const channel = w?.actual_channel ?? null;
  const freq = w?.frequency_mhz ?? null;
  const rssi = w?.rssi_dbm ?? null;
  const snr = w?.snr_db ?? null;
  const noise = w?.noise_dbm ?? null;
  const loss = typeof w?.loss_percent === "number" ? w.loss_percent : null;
  const fecRecovered = w?.fec_recovered ?? null;
  const fecFailed = w?.fec_failed ?? null;
  const bitrate = w?.bitrate_kbps ?? 0;
  const restarts = w?.restart_count ?? 0;
  const adapterMissing =
    state === "disabled" || (state === "error" && !iface);

  const lossSeverity =
    loss == null ? null : loss < 1 ? "ok" : loss < 5 ? "warn" : "err";

  const badge = stateBadge(state);

  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          <Antenna className="h-3.5 w-3.5" />
          WFB Receive
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
            WFB-rx is not running. Plug in an RTL8812EU dongle and set the
            channel in Settings — the agent starts the receive pipeline
            automatically when an adapter and channel are both present.
          </p>
        ) : (
          <div className="grid grid-cols-2 gap-x-4 gap-y-1.5 text-sm">
            <div className="text-xs text-muted-foreground">interface</div>
            <div className="font-mono">{iface || "—"}</div>

            <div className="text-xs text-muted-foreground">channel</div>
            <div className="font-mono">
              {channel != null
                ? `${channel}${freq != null ? ` (${fmtNum(freq, 0)} MHz)` : ""}`
                : "—"}
            </div>

            <div className="text-xs text-muted-foreground">rssi</div>
            <div className="font-mono">{fmtRssi(rssi)}</div>

            {snr != null && (
              <>
                <div className="text-xs text-muted-foreground">snr</div>
                <div className="font-mono">{fmtNum(snr, 1)} dB</div>
              </>
            )}

            {noise != null && (
              <>
                <div className="text-xs text-muted-foreground">noise</div>
                <div className="font-mono">{fmtRssi(noise)}</div>
              </>
            )}

            <div className="text-xs text-muted-foreground">bitrate</div>
            <div className="font-mono">{fmtBitrate(bitrate)}</div>

            <div className="text-xs text-muted-foreground">packet loss</div>
            <div
              className={`font-mono ${
                lossSeverity === "err"
                  ? "text-destructive"
                  : lossSeverity === "warn"
                    ? "text-warn"
                    : ""
              }`}
            >
              {loss != null ? `${fmtNum(loss, 1)}%` : "—"}
            </div>

            <div className="text-xs text-muted-foreground">FEC</div>
            <div className="font-mono text-xs">
              {fecRecovered != null || fecFailed != null
                ? `${fecRecovered ?? 0} ok · ${fecFailed ?? 0} fail`
                : "—"}
            </div>

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
