import { Activity } from "lucide-react";
import { useTranslation } from "react-i18next";

import { Badge, type BadgeProps } from "@/components/ui/badge";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { useResource } from "@/hooks/use-resource";
import {
  CLOCK_PATH,
  SENSORS_PATH,
  formatSigma,
  rateVerdict,
  type ClockStatus,
  type RateVerdict,
  type SensorsStatus,
} from "@/lib/sensors";
import { fmtNum } from "@/shared/format";

const VERDICT_VARIANT: Record<RateVerdict, BadgeProps["variant"]> = {
  ok: "ok",
  degraded: "warn",
  low: "err",
  none: "default",
};

export interface SensorsViewProps {
  sensors: SensorsStatus;
  clock: ClockStatus | null;
}

/** Hub, clock and GNSS summary over the per-stream table (kind, role, source,
 *  rate vs expected, age, σ). */
export function SensorsView({ sensors, clock }: SensorsViewProps) {
  const { t } = useTranslation();
  if (!sensors.present) return <p className="text-sm text-muted-foreground">{t("sensors.absent")}</p>;
  const hub = sensors.hub;
  return (
    <div className="space-y-4 text-sm">
      <div className="grid grid-cols-1 gap-2 sm:grid-cols-3">
        <div>
          <div className="text-xs text-muted-foreground">{t("sensors.hub")}</div>
          <div className="font-mono text-xs">
            {hub
              ? t("sensors.hubLine", {
                  state: hub.connected ? t("sensors.connected") : t("sensors.disconnected"),
                  transport: hub.transport,
                  crc: hub.crc_errors,
                  dropped: hub.dropped,
                })
              : t("sensors.notConfigured")}
          </div>
        </div>
        <div>
          <div className="text-xs text-muted-foreground">{t("sensors.clock")}</div>
          <div className="font-mono text-xs">
            {clock?.present
              ? t("sensors.clockLine", {
                  timebase: clock.timebase ?? "--",
                  state: clock.locked ? t("sensors.locked") : t("sensors.unlocked"),
                  sigma: formatSigma(clock.sigma_ns),
                })
              : t("sensors.notConfigured")}
          </div>
        </div>
        <div>
          <div className="text-xs text-muted-foreground">{t("sensors.gnss")}</div>
          <div className="font-mono text-xs">
            {sensors.gnss
              ? t("sensors.gnssLine", {
                  fix: sensors.gnss.fix,
                  sats: sensors.gnss.sats,
                  hdop: fmtNum(sensors.gnss.hdop, 1),
                  rtk: sensors.gnss.rtk,
                })
              : t("sensors.notConfigured")}
          </div>
        </div>
      </div>
      <table className="w-full text-left text-xs">
        <thead className="text-muted-foreground">
          <tr>
            <th className="py-1">{t("sensors.col.id")}</th>
            <th>{t("sensors.col.kind")}</th>
            <th>{t("sensors.col.role")}</th>
            <th>{t("sensors.col.source")}</th>
            <th>{t("sensors.col.rate")}</th>
            <th>{t("sensors.col.age")}</th>
            <th>{t("sensors.col.sigma")}</th>
          </tr>
        </thead>
        <tbody className="font-mono">
          {sensors.sensors.map((s) => {
            const verdict = rateVerdict(s);
            return (
              <tr key={s.id}>
                <td className="py-1">{s.id}</td>
                <td>{s.kind}</td>
                <td>{s.role}</td>
                <td>{s.source}</td>
                <td>
                  <span className="mr-2">
                    {s.expected_hz == null
                      ? t("sensors.rateOnly", { rate: fmtNum(s.rate_hz, 1) })
                      : t("sensors.rateVs", { rate: fmtNum(s.rate_hz, 1), expected: fmtNum(s.expected_hz, 1) })}
                  </span>
                  {verdict !== "none" && (
                    <Badge variant={VERDICT_VARIANT[verdict]}>{t(`sensors.verdict.${verdict}`)}</Badge>
                  )}
                </td>
                <td>{s.age_ms == null ? t("common.dash") : t("sensors.ageMs", { ms: fmtNum(s.age_ms, 0) })}</td>
                <td>{formatSigma(s.sigma_ns)}</td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}

export function SensorsPanel() {
  const { t } = useTranslation();
  const sensors = useResource<SensorsStatus>("sensors", SENSORS_PATH, 2_000);
  const clock = useResource<ClockStatus>("clock", CLOCK_PATH, 2_000);
  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          <Activity className="h-3.5 w-3.5" />
          {t("sensors.title")}
        </CardTitle>
      </CardHeader>
      <CardContent>
        {sensors.data ? (
          <SensorsView sensors={sensors.data} clock={clock.data ?? null} />
        ) : (
          <p className="text-sm text-muted-foreground">
            {sensors.isError ? t("sensors.loadFailed") : t("common.loading")}
          </p>
        )}
      </CardContent>
    </Card>
  );
}
