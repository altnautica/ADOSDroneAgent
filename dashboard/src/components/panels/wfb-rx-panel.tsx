import { Antenna } from "lucide-react";

import {
  Card,
  CardContent,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { useSnapshot } from "@/hooks/use-snapshot";
import { fmtBitrate, fmtNum, fmtRssi } from "@/lib/format";

export function WfbRxPanel() {
  const snap = useSnapshot();
  const w = snap.data?.wfb_rx;

  const adapter = w?.adapter ?? "";
  const channel = w?.channel ?? null;
  const freq = w?.freq_mhz ?? null;
  const rssi = w?.rssi_dbm ?? null;
  const loss = w?.packet_loss_pct ?? null;
  const fecRecovered = w?.fec_recovered ?? null;
  const fecFailed = w?.fec_failed ?? null;
  const bitrate = w?.bitrate_kbps ?? null;
  const streams = w?.streams ?? [];

  const lossSeverity =
    loss == null
      ? null
      : loss < 1
        ? "ok"
        : loss < 5
          ? "warn"
          : "err";

  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          <Antenna className="h-3.5 w-3.5" />
          WFB Receive
        </CardTitle>
      </CardHeader>
      <CardContent>
        {!adapter ? (
          <p className="text-xs text-muted-foreground">
            no RTL8812EU adapter detected. plug a USB Wi-Fi dongle that the
            agent recognises and the WFB-rx service will start.
          </p>
        ) : (
          <div className="grid grid-cols-2 gap-x-4 gap-y-1.5 text-sm">
            <div className="text-xs text-muted-foreground">adapter</div>
            <div className="font-mono">{adapter}</div>

            <div className="text-xs text-muted-foreground">channel</div>
            <div className="font-mono">
              {channel != null
                ? `${channel}${freq != null ? ` (${fmtNum(freq, 0)} MHz)` : ""}`
                : "—"}
            </div>

            <div className="text-xs text-muted-foreground">rssi</div>
            <div className="font-mono">{fmtRssi(rssi)}</div>

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

            {streams.length > 0 && (
              <>
                <div className="text-xs text-muted-foreground col-span-2 pt-2 border-t border-border/50">
                  streams ({streams.length})
                </div>
              </>
            )}
          </div>
        )}
      </CardContent>
    </Card>
  );
}
