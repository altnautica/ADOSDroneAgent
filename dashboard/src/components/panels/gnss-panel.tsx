import { useQueryClient } from "@tanstack/react-query";
import { Satellite } from "lucide-react";
import { useState } from "react";
import { useTranslation } from "react-i18next";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { ConfirmDialog } from "@/components/settings/confirm-dialog";
import { useResource } from "@/hooks/use-resource";
import { GNSS_PATH, putGnssBase, type GnssStatus } from "@/shared/api/sensors";
import { toast, toastFromError } from "@/shared/api/toast";
import { fmtNum } from "@/shared/format";
import { gnssBaseLabel, gnssFixLabel } from "@/lib/state-labels";

/** Receivers, base mode (survey/off) and the RTCM output of the GS GNSS. */
export function GnssPanel() {
  const { t } = useTranslation();
  const qc = useQueryClient();
  const gnss = useResource<GnssStatus>("gnss", GNSS_PATH, 2_000);
  const [minDur, setMinDur] = useState("300");
  const [accuracy, setAccuracy] = useState("2.0");
  const [busy, setBusy] = useState(false);
  // Survey restarts and base off both stop the RTCM corrections drones use.
  const [baseConfirm, setBaseConfirm] = useState<"survey" | "off" | null>(null);
  const g = gnss.data;

  async function setBase(survey: boolean) {
    setBusy(true);
    try {
      await putGnssBase(
        survey
          ? { mode: "survey", min_dur_s: Number(minDur), accuracy_m: Number(accuracy) }
          : { mode: "off" },
      );
      toast.ok(t("gnss.baseSaved"));
      await qc.invalidateQueries({ queryKey: ["gnss"] });
    } catch (err) {
      toastFromError(err, t("settings.shared.applyFailed"));
    } finally {
      setBusy(false);
    }
  }

  const surveyValid = Number(minDur) > 0 && Number(accuracy) > 0;

  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          <Satellite className="h-3.5 w-3.5" />
          {t("gnss.title")}
        </CardTitle>
      </CardHeader>
      <CardContent className="space-y-4 text-sm">
        {!g ? (
          <p className="text-muted-foreground">{gnss.isError ? t("gnss.loadFailed") : t("common.loading")}</p>
        ) : !g.present ? (
          <p className="text-muted-foreground">{t("gnss.absent")}</p>
        ) : (
          <>
            <table className="w-full text-left text-xs">
              <thead className="text-muted-foreground">
                <tr>
                  <th className="py-1">{t("gnss.col.id")}</th>
                  <th>{t("gnss.col.model")}</th>
                  <th>{t("gnss.col.fix")}</th>
                  <th>{t("gnss.col.sats")}</th>
                  <th>{t("gnss.col.hdop")}</th>
                  <th>{t("gnss.col.pps")}</th>
                </tr>
              </thead>
              <tbody className="font-mono">
                {g.receivers.map((r) => (
                  <tr key={r.id}>
                    <td className="py-1">{r.id}</td>
                    <td>{r.model ?? t("common.dash")}</td>
                    <td>{gnssFixLabel(r.fix)}</td>
                    <td>{r.sats}</td>
                    <td>{r.hdop == null ? t("common.dash") : fmtNum(r.hdop, 1)}</td>
                    <td>
                      <Badge variant={r.pps_locked ? "success" : "neutral"}>
                        {r.pps_locked ? t("gnss.ppsLocked") : t("gnss.ppsFree")}
                      </Badge>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
            {g.base && (
              <div className="space-y-2">
                <div className="flex items-center gap-2">
                  <span className="text-xs text-muted-foreground">{t("gnss.base")}</span>
                  <Badge variant={g.base.state === "failed" ? "error" : g.base.state === "off" ? "neutral" : "success"}>
                    {gnssBaseLabel(g.base.state)}
                  </Badge>
                  {g.base.state === "surveying" && (
                    <span className="font-mono text-xs">
                      {t("gnss.surveyProgress", {
                        elapsed: g.base.elapsed_s,
                        min: g.base.min_dur_s,
                        acc: g.base.accuracy_m == null ? t("common.dash") : fmtNum(g.base.accuracy_m, 1),
                        target: g.base.target_accuracy_m == null ? t("common.dash") : fmtNum(g.base.target_accuracy_m, 1),
                      })}
                    </span>
                  )}
                </div>
                <div className="font-mono text-xs">
                  {t("gnss.rtcmOut", {
                    rate: fmtNum(g.base.rtcm_out.msgs_per_s, 1),
                    bytes: g.base.rtcm_out.bytes_total,
                    targets: g.base.rtcm_out.targets,
                  })}
                </div>
                <div className="flex flex-wrap items-end gap-3">
                  <div className="space-y-1">
                    <Label htmlFor="gnss-min-dur">{t("gnss.minDur")}</Label>
                    <Input id="gnss-min-dur" className="w-24" inputMode="numeric" value={minDur} onChange={(e) => setMinDur(e.target.value)} />
                  </div>
                  <div className="space-y-1">
                    <Label htmlFor="gnss-acc">{t("gnss.accuracy")}</Label>
                    <Input id="gnss-acc" className="w-24" inputMode="decimal" value={accuracy} onChange={(e) => setAccuracy(e.target.value)} />
                  </div>
                  <Button size="sm" disabled={busy || !surveyValid} onClick={() => setBaseConfirm("survey")}>
                    {t("gnss.startSurvey")}
                  </Button>
                  <Button size="sm" variant="outline" disabled={busy || g.base.mode === "off"} onClick={() => setBaseConfirm("off")}>
                    {t("gnss.baseOff")}
                  </Button>
                </div>
              </div>
            )}
          </>
        )}
      </CardContent>
      <ConfirmDialog
        open={baseConfirm !== null}
        onOpenChange={(open) => {
          if (!open) setBaseConfirm(null);
        }}
        title={baseConfirm === "off" ? t("gnss.baseOffTitle") : t("gnss.surveyTitle")}
        description={t("gnss.rtcmLoss", { count: g?.base?.rtcm_out.targets ?? 0 })}
        confirmLabel={baseConfirm === "off" ? t("gnss.baseOff") : t("gnss.startSurvey")}
        destructive={baseConfirm === "off"}
        onConfirm={() => setBase(baseConfirm === "survey")}
      />
    </Card>
  );
}
