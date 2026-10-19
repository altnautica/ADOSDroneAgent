import { useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { useTranslation } from "react-i18next";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { useResource } from "@/hooks/use-resource";
import {
  CLOCK_PATH,
  SENSORS_PATH,
  formatSigma,
  parseRigFile,
  putRig,
  type ClockStatus,
  type SensorsStatus,
} from "@/lib/sensors";
import { toast } from "@/lib/toast";
import { ApiError, errorDetail } from "@/shared/api-fetch";
import { fmtNum } from "@/shared/format";

function HubCard({ sensors }: { sensors: SensorsStatus | undefined }) {
  const { t } = useTranslation();
  const hub = sensors?.hub ?? null;
  return (
    <Card>
      <CardHeader>
        <CardTitle>{t("settings.sensors.hubTitle")}</CardTitle>
      </CardHeader>
      <CardContent className="text-sm">
        {!hub ? (
          <p className="text-muted-foreground">{t("sensors.notConfigured")}</p>
        ) : (
          <div className="grid grid-cols-2 gap-x-4 gap-y-1.5">
            <div className="text-xs text-muted-foreground">{t("settings.sensors.transport")}</div>
            <div className="font-mono text-xs">{hub.transport}</div>
            <div className="text-xs text-muted-foreground">{t("settings.sensors.status")}</div>
            <div>
              <Badge variant={hub.connected ? "ok" : "err"}>
                {hub.connected ? t("sensors.connected") : t("sensors.disconnected")}
              </Badge>
            </div>
            <div className="text-xs text-muted-foreground">{t("settings.sensors.firmware")}</div>
            <div className="font-mono text-xs">{hub.fw ?? t("common.dash")}</div>
            <div className="text-xs text-muted-foreground">{t("settings.sensors.board")}</div>
            <div className="font-mono text-xs">{hub.board ?? t("common.dash")}</div>
            <div className="text-xs text-muted-foreground">{t("settings.sensors.errors")}</div>
            <div className="font-mono text-xs">
              {t("settings.sensors.errorsValue", { crc: hub.crc_errors, dropped: hub.dropped })}
            </div>
          </div>
        )}
      </CardContent>
    </Card>
  );
}

function RigCard({ sensors }: { sensors: SensorsStatus | undefined }) {
  const { t } = useTranslation();
  const qc = useQueryClient();
  const [rig, setRig] = useState<Record<string, unknown> | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  async function pick(file: File | undefined) {
    setRig(null);
    setError(null);
    if (!file) return;
    const parsed = parseRigFile(await file.text());
    if (parsed.ok) setRig(parsed.rig);
    else setError(parsed.reason === "parse" ? t("settings.sensors.rigParse", { detail: parsed.detail }) : t("settings.sensors.rigShape"));
  }

  async function upload() {
    if (!rig) return;
    setBusy(true);
    setError(null);
    try {
      const res = await putRig(rig);
      toast.ok(t("settings.sensors.rigSaved", { rig: res.rig_id, calib: res.calib_version }));
      setRig(null);
      await qc.invalidateQueries({ queryKey: ["sensors"] });
    } catch (err) {
      setError(
        err instanceof ApiError
          ? t("settings.sensors.rigRejected", { code: err.code ?? err.status, detail: errorDetail(err.body, err.message) })
          : String(err),
      );
    } finally {
      setBusy(false);
    }
  }

  return (
    <Card>
      <CardHeader>
        <CardTitle>{t("settings.sensors.rigTitle")}</CardTitle>
      </CardHeader>
      <CardContent className="space-y-3 text-sm">
        <div className="font-mono text-xs">
          {t("settings.sensors.rigCurrent", {
            rig: sensors?.rig_id ?? t("common.dash"),
            calib: sensors?.calib_version ?? t("common.dash"),
          })}
        </div>
        <div className="flex flex-wrap items-end gap-3">
          <div className="space-y-1">
            <Label htmlFor="rig-file">{t("settings.sensors.rigFile")}</Label>
            <Input id="rig-file" type="file" accept="application/json,.json" onChange={(e) => pick(e.target.files?.[0])} />
          </div>
          <Button size="sm" disabled={busy || !rig} onClick={upload}>
            {t("settings.sensors.rigUpload")}
          </Button>
        </div>
        {error && (
          <p className="text-xs text-destructive" role="alert">
            {error}
          </p>
        )}
      </CardContent>
    </Card>
  );
}

function ClockCard() {
  const { t } = useTranslation();
  const clock = useResource<ClockStatus>("clock", CLOCK_PATH, 5_000);
  const c = clock.data;
  return (
    <Card>
      <CardHeader>
        <CardTitle>{t("settings.sensors.clockTitle")}</CardTitle>
      </CardHeader>
      <CardContent className="text-sm">
        {!c?.present ? (
          <p className="text-muted-foreground">{t("sensors.notConfigured")}</p>
        ) : (
          <table className="w-full text-left text-xs">
            <thead className="text-muted-foreground">
              <tr>
                <th className="py-1">{t("settings.sensors.col.id")}</th>
                <th>{t("settings.sensors.col.kind")}</th>
                <th>{t("settings.sensors.col.locked")}</th>
                <th>{t("settings.sensors.col.offset")}</th>
                <th>{t("settings.sensors.col.skew")}</th>
                <th>{t("settings.sensors.col.sigma")}</th>
              </tr>
            </thead>
            <tbody className="font-mono">
              {c.sources.map((s) => (
                <tr key={s.id}>
                  <td className="py-1">{s.id}</td>
                  <td>{s.kind}</td>
                  <td>
                    <Badge variant={s.locked ? "ok" : "default"}>
                      {s.locked ? t("sensors.locked") : t("sensors.unlocked")}
                    </Badge>
                  </td>
                  <td>{`${s.offset_ns < 0 ? "-" : ""}${formatSigma(Math.abs(s.offset_ns))}`}</td>
                  <td>{t("settings.sensors.ppm", { ppm: fmtNum(s.skew_ppm, 1) })}</td>
                  <td>{formatSigma(s.sigma_ns)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </CardContent>
    </Card>
  );
}

export function SensorsSettings() {
  const sensors = useResource<SensorsStatus>("sensors", SENSORS_PATH, 5_000);
  return (
    <div className="space-y-6">
      <HubCard sensors={sensors.data} />
      <RigCard sensors={sensors.data} />
      <ClockCard />
    </div>
  );
}
