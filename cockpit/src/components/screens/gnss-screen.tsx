// GNSS — the node's receivers, the base station (survey-in and its RTCM
// output) and dual-antenna heading. Starting a survey or turning the base off
// interrupts the RTCM stream the rovers correct from, so both fire from a
// guarded button.

import { useCallback, useState } from "react";
import { useTranslation } from "react-i18next";

import { ConsoleScaffold } from "@/components/ui/console-scaffold";
import { EmptyNote, ProgressBar, Row, SectionHeader, StaleBadge } from "@/components/ui/data";
import { GuardedButton } from "@/components/ui/guarded-button";
import { ListItem } from "@/components/ui/list-item";
import { TextSettingRow } from "@/components/ui/setting-row";
import { canTurnBaseOff, surveyFraction, surveyRequest, type BaseRequest } from "@/lib/gnss-model";
import { apiFetch } from "@/shared/api-fetch";
import { GNSS_PATH, putGnssBase, type GnssStatus } from "@/shared/api/sensors";
import { toast, toastFromError } from "@/shared/api/toast";
import { DASH, fmtNum } from "@/shared/format";
import "@/shared/i18n";
import { useResource } from "@/shared/use-resource";

const GNSS_POLL_MS = 2_000;

export function GnssScreen() {
  const { t } = useTranslation();
  const [minDur, setMinDur] = useState("300");
  const [accuracy, setAccuracy] = useState("2.0");
  const [busy, setBusy] = useState(false);

  const gnss = useResource(
    useCallback((s: AbortSignal) => apiFetch<GnssStatus>(GNSS_PATH, { signal: s }), []),
    GNSS_POLL_MS,
  );
  const g = gnss.data;

  const setBase = (req: BaseRequest) => {
    setBusy(true);
    putGnssBase(req)
      .then(() => {
        toast.ok(t("gnss.baseSaved"));
        gnss.refresh();
      })
      .catch((err: unknown) => toastFromError(err, t("settings.shared.applyFailed")))
      .finally(() => setBusy(false));
  };

  const survey = surveyRequest(minDur, accuracy);
  const fraction = surveyFraction(g?.base);

  let body;
  if (!gnss.ready) {
    body = <EmptyNote>{t("common.loading")}</EmptyNote>;
  } else if (!g) {
    body = <EmptyNote>{t("gnss.loadFailed")}</EmptyNote>;
  } else if (!g.present) {
    body = <EmptyNote>{t("gnss.absent")}</EmptyNote>;
  } else {
    const base = g.base;
    body = (
      <div className="flex flex-col gap-[0.4rem]">
        <SectionHeader>{t("cockpit.gnss.receivers")}</SectionHeader>
        {g.receivers.length === 0 ? (
          <EmptyNote>{t("cockpit.gnss.noReceivers")}</EmptyNote>
        ) : (
          g.receivers.map((r) => (
            <ListItem
              key={r.id}
              headline={`${r.id} · ${r.model ?? DASH}`}
              supporting={t("cockpit.gnss.receiverLine", {
                fix: r.fix,
                sats: r.sats,
                hdop: r.hdop == null ? DASH : fmtNum(r.hdop, 1),
                rtk: r.rtk,
              })}
              trailing={
                <span className={r.pps_locked ? "text-[0.8rem] text-ok" : "text-[0.8rem] text-muted-foreground"}>
                  {r.pps_locked ? t("gnss.ppsLocked") : t("gnss.ppsFree")}
                </span>
              }
            />
          ))
        )}
        {g.heading?.available ? (
          <Row
            label={t("cockpit.gnss.heading")}
            value={g.heading.deg == null ? DASH : t("cockpit.gnss.deg", { deg: fmtNum(g.heading.deg, 1) })}
          />
        ) : null}
        {base ? (
          <>
            <SectionHeader>{t("gnss.base")}</SectionHeader>
            <Row
              label={t("cockpit.gnss.baseState")}
              value={base.state}
              tone={base.state === "failed" ? "err" : base.state === "off" ? "muted" : "ok"}
            />
            {fraction != null ? (
              <>
                <ProgressBar value={fraction} max={1} tone="ok" />
                <div className="font-mono text-[0.8rem]">
                  {t("gnss.surveyProgress", {
                    elapsed: base.elapsed_s,
                    min: base.min_dur_s,
                    acc: base.accuracy_m == null ? DASH : fmtNum(base.accuracy_m, 1),
                    target: base.target_accuracy_m == null ? DASH : fmtNum(base.target_accuracy_m, 1),
                  })}
                </div>
              </>
            ) : null}
            <Row
              label={t("cockpit.gnss.rtcm")}
              value={t("gnss.rtcmOut", {
                rate: fmtNum(base.rtcm_out.msgs_per_s, 1),
                bytes: base.rtcm_out.bytes_total,
                targets: base.rtcm_out.targets,
              })}
            />
            <TextSettingRow title={t("gnss.minDur")} value={minDur} onChange={setMinDur} inputMode="numeric" enabled={!busy} />
            <TextSettingRow title={t("gnss.accuracy")} value={accuracy} onChange={setAccuracy} inputMode="decimal" enabled={!busy} />
            <div className="flex flex-wrap gap-[0.5rem] pt-[0.3rem]">
              <GuardedButton
                label={t("gnss.startSurvey")}
                guardLabel={t("cockpit.gnss.surveyGuard")}
                disabled={busy || !survey}
                onFired={() => survey && setBase(survey)}
              />
              <GuardedButton
                label={t("gnss.baseOff")}
                guardLabel={t("cockpit.gnss.offGuard")}
                disabled={!canTurnBaseOff(base, busy)}
                onFired={() => setBase({ mode: "off" })}
              />
            </div>
          </>
        ) : null}
      </div>
    );
  }

  return (
    <ConsoleScaffold title={t("gnss.title")} actions={<StaleBadge stale={gnss.stale} />}>
      {body}
    </ConsoleScaffold>
  );
}
