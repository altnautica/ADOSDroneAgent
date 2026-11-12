import { Antenna } from "lucide-react";
import { Trans, useTranslation } from "react-i18next";

import { Badge } from "@/components/ui/badge";
import {
  Card,
  CardContent,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { useResource } from "@/hooks/use-resource";
import { useNow } from "@/hooks/use-now";
import { useWfb, WFB_POLL_MS } from "@/hooks/use-wfb";
import { liveData } from "@/lib/freshness";
import { cn } from "@/lib/utils";
import { wfbStale, wfbStateBadge } from "@/lib/wfb-state";
import { linkKeyStatusView } from "@/shared/link-key-status";
import { fmtBitrate, fmtNum, fmtDbm } from "@/shared/format";

export function WfbRxPanel() {
  const { t } = useTranslation();
  const wfb = useWfb();
  const now = useNow(1_000);
  // Agent down: the last read is not shown as current.
  const w = liveData(wfb, WFB_POLL_MS, now);
  const videoConfig = useResource<unknown>("video-config", "/api/video/config", 5_000);
  const keyStatus = linkKeyStatusView(videoConfig.data);

  const state = (w?.state ?? "unknown").toLowerCase();
  const stale = wfbStale(state);
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
    loss == null || stale ? null : loss < 1 ? "ok" : loss < 5 ? "warn" : "err";

  const badge = wfbStateBadge(state);

  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          <Antenna className="h-3.5 w-3.5" />
          {t("wfb.rx.title")}
          {badge && (
            <Badge variant={badge.variant} className="ml-auto">
              {badge.label}
            </Badge>
          )}
        </CardTitle>
      </CardHeader>
      <CardContent className={cn(stale && "opacity-50")}>
        {adapterMissing ? (
          <p className="text-xs text-muted-foreground">
            <Trans i18nKey="wfb.rx.notRunning" components={{ code: <code /> }} />
          </p>
        ) : (
          <div className="grid grid-cols-2 gap-x-4 gap-y-1.5 text-sm">
            <div className="text-xs text-muted-foreground">{t("wfb.fields.interface")}</div>
            <div className="font-mono">{iface || "—"}</div>

            <div className="text-xs text-muted-foreground">{t("wfb.fields.channel")}</div>
            <div className="font-mono">
              {channel != null
                ? `${channel}${freq != null ? ` (${fmtNum(freq, 0)} MHz)` : ""}`
                : "—"}
            </div>

            <div className="text-xs text-muted-foreground">{t("wfb.fields.rssi")}</div>
            <div className="font-mono">{fmtDbm(rssi)}</div>

            {snr != null && (
              <>
                <div className="text-xs text-muted-foreground">{t("wfb.fields.snr")}</div>
                <div className="font-mono">{`${fmtNum(snr, 1)} dB`}</div>
              </>
            )}

            {noise != null && (
              <>
                <div className="text-xs text-muted-foreground">{t("wfb.fields.noise")}</div>
                <div className="font-mono">{fmtDbm(noise)}</div>
              </>
            )}

            <div className="text-xs text-muted-foreground">{t("wfb.fields.bitrate")}</div>
            <div className="font-mono">{fmtBitrate(bitrate)}</div>

            <div className="text-xs text-muted-foreground">{t("wfb.fields.packetLoss")}</div>
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

            <div className="text-xs text-muted-foreground">{t("wfb.fields.fec")}</div>
            <div className="font-mono text-xs">
              {fecRecovered != null || fecFailed != null
                ? t("wfb.rx.fecCounts", {
                    recovered: fecRecovered ?? 0,
                    failed: fecFailed ?? 0,
                  })
                : "—"}
            </div>

            {restarts > 0 && (
              <>
                <div className="text-xs text-muted-foreground">{t("wfb.fields.restarts")}</div>
                <div className="font-mono">{restarts}</div>
              </>
            )}

            {keyStatus && (
              <>
                <div className="text-xs text-muted-foreground">{t("wfb.fields.linkKey")}</div>
                <div className={`font-mono ${keyStatus.variant === "warn" ? "text-warn" : ""}`}>
                  {t(keyStatus.labelKey)}
                </div>
              </>
            )}
          </div>
        )}
      </CardContent>
    </Card>
  );
}
