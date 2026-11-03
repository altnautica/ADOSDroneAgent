// Sensors: the sensor hub, the rig file, and the clock sources. A picked rig
// file is parsed first; only a JSON object arms the guarded replace
// (`PUT /api/v1/sensors/rig`), and the agent's 400 `E_RIG_INVALID` for a
// document that fails the contract is shown under the picker.

import { useCallback, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { FileJson, Upload } from "lucide-react";

import { ActionButton, EmptyNote, Row, StaleBadge } from "@/components/ui/data";
import { GuardedButton } from "@/components/ui/guarded-button";
import { getClock, getSensors, rigRejection } from "@/lib/sensors-model";
import { CLOCK_POLL_MS, NO_RIG, SENSORS_POLL_MS, formatOffset, pickRig, rigPayload, type RigPick } from "@/lib/settings/sensors";
import { formatSigma, putRig, type SensorsStatus } from "@/shared/api/sensors";
import { toast } from "@/shared/api/toast";
import { fmtNum } from "@/shared/format";
import { useResource } from "@/shared/use-resource";
import "@/shared/i18n";

import { SectionGroup } from "./section-kit";

function HubGroup({ sensors }: { sensors: SensorsStatus | null }) {
  const { t } = useTranslation();
  const hub = sensors?.hub ?? null;
  return (
    <SectionGroup title={t("settings.sensors.hubTitle")}>
      {!hub ? (
        <EmptyNote>{t("sensors.notConfigured")}</EmptyNote>
      ) : (
        <>
          <Row label={t("settings.sensors.transport")} value={hub.transport} />
          <Row
            label={t("settings.sensors.status")}
            value={hub.connected ? t("sensors.connected") : t("sensors.disconnected")}
            tone={hub.connected ? "ok" : "err"}
          />
          <Row label={t("settings.sensors.firmware")} value={hub.fw ?? t("common.dash")} />
          <Row label={t("settings.sensors.board")} value={hub.board ?? t("common.dash")} />
          <Row
            label={t("settings.sensors.errors")}
            value={t("settings.sensors.errorsValue", { crc: hub.crc_errors, dropped: hub.dropped })}
          />
        </>
      )}
    </SectionGroup>
  );
}

function RigGroup({ sensors, onSaved }: { sensors: SensorsStatus | null; onSaved: () => void }) {
  const { t } = useTranslation();
  const fileRef = useRef<HTMLInputElement>(null);
  const [pick, setPick] = useState<RigPick>(NO_RIG);
  const [rejected, setRejected] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const choose = async (file: File | undefined) => {
    setRejected(null);
    setPick(file ? pickRig(file.name, await file.text()) : NO_RIG);
  };

  const replace = async () => {
    const rig = rigPayload(pick);
    if (!rig) return;
    setBusy(true);
    setRejected(null);
    try {
      const res = await putRig(rig);
      toast.ok(t("settings.sensors.rigSaved", { rig: res.rig_id, calib: res.calib_version }));
      setPick(NO_RIG);
      if (fileRef.current) fileRef.current.value = "";
      onSaved();
    } catch (err) {
      setRejected(t("settings.sensors.rigRejected", rigRejection(err)));
    } finally {
      setBusy(false);
    }
  };

  const problem =
    rejected ??
    (pick.state === "invalid"
      ? pick.reason === "parse"
        ? t("settings.sensors.rigParse", { detail: pick.detail })
        : t("settings.sensors.rigShape")
      : null);

  return (
    <SectionGroup title={t("settings.sensors.rigTitle")}>
      <Row
        label={t("settings.sensors.rigCurrent", {
          rig: sensors?.rig_id ?? t("common.dash"),
          calib: sensors?.calib_version ?? t("common.dash"),
        })}
      />
      <input
        ref={fileRef}
        type="file"
        accept="application/json,.json"
        className="hidden"
        aria-label={t("settings.sensors.rigFile")}
        onChange={(e) => void choose(e.target.files?.[0])}
      />
      <Row
        label={t("settings.sensors.rigFile")}
        hint={pick.state === "empty" ? t("cockpit.settingsSections.sensors.noFile") : pick.name}
        left={<FileJson className="h-[1.1rem] w-[1.1rem] text-muted-foreground" aria-hidden />}
      />
      <div className="flex flex-wrap gap-[0.5rem] px-[0.75rem]">
        <ActionButton
          label={t("cockpit.settingsSections.sensors.chooseFile")}
          icon={FileJson}
          disabled={busy}
          onClick={() => fileRef.current?.click()}
        />
        <GuardedButton
          label={t("settings.sensors.rigUpload")}
          guardLabel={t("cockpit.settingsSections.sensors.rigGuard")}
          icon={Upload}
          disabled={busy || rigPayload(pick) === null}
          onFired={() => void replace()}
        />
      </div>
      {pick.state === "ready" && !rejected ? (
        <p className="px-[0.75rem] text-[0.75rem] text-muted-foreground">
          {t("cockpit.settingsSections.sensors.rigReady", { name: pick.name })}
        </p>
      ) : null}
      {problem ? (
        <p className="px-[0.75rem] text-[0.75rem] text-err" role="alert">
          {problem}
        </p>
      ) : null}
    </SectionGroup>
  );
}

function ClockGroup() {
  const { t } = useTranslation();
  const clock = useResource(useCallback((s: AbortSignal) => getClock(s), []), CLOCK_POLL_MS);
  const c = clock.data;
  return (
    <SectionGroup title={t("settings.sensors.clockTitle")}>
      <div className="flex justify-end px-[0.75rem]">
        <StaleBadge stale={clock.stale} />
      </div>
      {!c?.present ? (
        <EmptyNote>{t("sensors.notConfigured")}</EmptyNote>
      ) : (
        <div className="overflow-x-auto px-[0.75rem]">
          <table className="w-full text-left text-[0.8rem]">
            <thead className="text-muted-foreground">
              <tr>
                <th className="py-[0.3rem] font-normal">{t("settings.sensors.col.id")}</th>
                <th className="font-normal">{t("settings.sensors.col.kind")}</th>
                <th className="font-normal">{t("settings.sensors.col.locked")}</th>
                <th className="font-normal">{t("settings.sensors.col.offset")}</th>
                <th className="font-normal">{t("settings.sensors.col.skew")}</th>
                <th className="font-normal">{t("settings.sensors.col.sigma")}</th>
              </tr>
            </thead>
            <tbody className="font-mono text-surface-foreground">
              {c.sources.map((s) => (
                <tr key={s.id} className="border-t border-border">
                  <td className="py-[0.35rem]">{s.id}</td>
                  <td>{s.kind}</td>
                  <td className={s.locked ? "text-ok" : "text-muted-foreground"}>
                    {s.locked ? t("sensors.locked") : t("sensors.unlocked")}
                  </td>
                  <td>{formatOffset(s.offset_ns)}</td>
                  <td>{t("settings.sensors.ppm", { ppm: fmtNum(s.skew_ppm, 1) })}</td>
                  <td>{formatSigma(s.sigma_ns)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </SectionGroup>
  );
}

export function SensorsSection() {
  const { t } = useTranslation();
  const sensors = useResource(useCallback((s: AbortSignal) => getSensors(s), []), SENSORS_POLL_MS);
  return (
    <div className="flex flex-col gap-[0.9rem]">
      {sensors.ready && !sensors.data ? <EmptyNote>{t("sensors.loadFailed")}</EmptyNote> : null}
      <HubGroup sensors={sensors.data} />
      <RigGroup sensors={sensors.data} onSaved={sensors.refresh} />
      <ClockGroup />
    </div>
  );
}
