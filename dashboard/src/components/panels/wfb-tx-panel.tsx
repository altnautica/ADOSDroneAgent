import type { TFunction } from "i18next";
import { Antenna } from "lucide-react";
import { Trans, useTranslation } from "react-i18next";

import { Badge } from "@/components/ui/badge";
import {
  Card,
  CardContent,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { useWfb } from "@/hooks/use-wfb";
import { fmtBitrate, fmtNum } from "@/shared/format";

export function WfbTxPanel() {
  const { t } = useTranslation();
  const wfb = useWfb();
  const w = wfb.data;

  const state = (w?.state ?? "unknown").toLowerCase();
  const iface = w?.interface ?? "";
  const channel = w?.actual_channel ?? null;
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

  const badge = stateBadge(state, t);

  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          <Antenna className="h-3.5 w-3.5" />
          {t("wfb.tx.title")}
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
            <Trans i18nKey="wfb.tx.notRunning" components={{ code: <code /> }} />
          </p>
        ) : (
          <div className="grid grid-cols-2 gap-x-4 gap-y-1.5 text-sm">
            <div className="text-xs text-muted-foreground">{t("wfb.fields.interface")}</div>
            <div className="font-mono">{iface || "—"}</div>

            <div className="text-xs text-muted-foreground">{t("wfb.fields.channel")}</div>
            <div className="font-mono">
              {channel != null
                ? `${channel}${freq ? ` (${fmtNum(freq, 0)} MHz)` : ""}`
                : "—"}
            </div>

            {bw != null && bw > 0 && (
              <>
                <div className="text-xs text-muted-foreground">{t("wfb.fields.bandwidth")}</div>
                <div className="font-mono">{`${fmtNum(bw, 0)} MHz`}</div>
              </>
            )}

            <div className="text-xs text-muted-foreground">{t("wfb.fields.txPower")}</div>
            <div className="font-mono">
              {txPower != null
                ? `${fmtNum(txPower, 0)} dBm${
                    txPowerMax != null ? ` / ${fmtNum(txPowerMax, 0)}` : ""
                  }`
                : "—"}
            </div>

            {mcs != null && (
              <>
                <div className="text-xs text-muted-foreground">{t("wfb.fields.mcs")}</div>
                <div className="font-mono">{mcs}</div>
              </>
            )}

            {regDomain && (
              <>
                <div className="text-xs text-muted-foreground">{t("wfb.fields.domain")}</div>
                <div className="font-mono">{regDomain}</div>
              </>
            )}

            <div className="text-xs text-muted-foreground">{t("wfb.fields.bitrate")}</div>
            <div className="font-mono">{fmtBitrate(bitrate)}</div>

            {packetsSent > 0 && (
              <>
                <div className="text-xs text-muted-foreground">{t("wfb.fields.packets")}</div>
                <div className="font-mono">{packetsSent.toLocaleString()}</div>
              </>
            )}

            {restarts > 0 && (
              <>
                <div className="text-xs text-muted-foreground">{t("wfb.fields.restarts")}</div>
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
  t: TFunction,
): { label: string; variant: "ok" | "warn" | "info" | "default" } | null {
  switch (state) {
    case "active":
    case "ready":
      return { label: state, variant: "ok" };
    case "connecting":
      return { label: t("wfb.state.connecting"), variant: "info" };
    case "error":
      return { label: t("wfb.state.error"), variant: "warn" };
    case "disabled":
      return { label: t("wfb.state.disabled"), variant: "default" };
    default:
      return state ? { label: state, variant: "default" } : null;
  }
}
