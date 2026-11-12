// Sensors — the sensor hub, the clock and the GNSS summary over the per-stream
// list (kind, role, source, rate against expected, age, σ), plus the rig file
// upload that tells the agent which sensors the rig carries and how they are
// calibrated.

import { useCallback, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { Upload } from "lucide-react";

import { ConsoleScaffold } from "@/components/ui/console-scaffold";
import { ActionButton, Dot, EmptyNote, ErrorNote, Row, SectionHeader, StaleBadge } from "@/components/ui/data";
import { ListItem } from "@/components/ui/list-item";
import { GuardedButton } from "@/components/ui/guarded-button";
import { getClock, getSensors, rigRejection, sensorTone } from "@/lib/sensors-model";
import { NO_RIG, pickRig, rigPayload, type RigPick } from "@/lib/settings/sensors";
import { formatSigma, putRig, rateVerdict } from "@/shared/api/sensors";
import { toast } from "@/shared/api/toast";
import { DASH, fmtNum } from "@/shared/format";
import "@/shared/i18n";
import { useResource } from "@/shared/use-resource";

const POLL_MS = 2_000;

export function SensorsScreen() {
  const { t } = useTranslation();
  const [uploading, setUploading] = useState(false);
  const [rig, setRig] = useState<RigPick>(NO_RIG);
  const fileRef = useRef<HTMLInputElement>(null);

  const sensors = useResource(useCallback((s: AbortSignal) => getSensors(s), []), POLL_MS);
  const clock = useResource(useCallback((s: AbortSignal) => getClock(s), []), POLL_MS);

  // Pick parses the file; only a parsed object reaches the guarded replace.
  const pick = (file: File) => {
    file
      .text()
      .then((text) => setRig(pickRig(file.name, text)))
      .catch((err: unknown) => setRig({ state: "invalid", name: file.name, reason: "parse", detail: String(err) }))
      .finally(() => {
        if (fileRef.current) fileRef.current.value = "";
      });
  };

  const replace = () => {
    const payload = rigPayload(rig);
    if (!payload) return;
    setUploading(true);
    putRig(payload)
      .then((res) => {
        toast.ok(t("settings.sensors.rigSaved", { rig: res.rig_id, calib: res.calib_version }));
        setRig(NO_RIG);
        sensors.refresh();
      })
      .catch((err: unknown) => toast.err(t("settings.sensors.rigRejected", rigRejection(err))))
      .finally(() => setUploading(false));
  };

  const s = sensors.data;
  const c = clock.data;

  const rigSection = (
    <>
      <SectionHeader>{t("settings.sensors.rigTitle")}</SectionHeader>
      <Row
        label={s?.rig_id ? t("settings.sensors.rigCurrent", { rig: s.rig_id, calib: s.calib_version ?? DASH }) : t("sensors.notConfigured")}
      />
      <input
        ref={fileRef}
        type="file"
        accept="application/json,.json"
        className="hidden"
        aria-label={t("settings.sensors.rigFile")}
        onChange={(e) => {
          const f = e.target.files?.[0];
          if (f) pick(f);
        }}
      />
      <ActionButton label={t("settings.sensors.rigFile")} icon={Upload} disabled={uploading} onClick={() => fileRef.current?.click()} />
      {rig.state === "invalid" ? (
        <Row
          label={rig.name}
          tone="err"
          value={rig.reason === "parse" ? t("settings.sensors.rigParse", { detail: rig.detail }) : t("settings.sensors.rigShape")}
        />
      ) : null}
      {rig.state === "ready" ? (
        <>
          <Row label={rig.name} tone="ok" value={t("cockpit.sensors.rigReady")} />
          <GuardedButton
            label={t("settings.sensors.rigUpload")}
            guardLabel={t("cockpit.sensors.rigGuard")}
            icon={Upload}
            disabled={uploading}
            onFired={replace}
          />
        </>
      ) : null}
    </>
  );

  let body;
  if (!sensors.ready) {
    body = <EmptyNote>{t("common.loading")}</EmptyNote>;
  } else if (!s) {
    body = <ErrorNote onRetry={sensors.refresh}>{t("sensors.loadFailed")}</ErrorNote>;
  } else if (!s.present) {
    body = (
      <div className="flex flex-col gap-[0.4rem]">
        <EmptyNote>{t("sensors.absent")}</EmptyNote>
        {rigSection}
      </div>
    );
  } else {
    const hub = s.hub;
    body = (
      <div className="flex flex-col gap-[0.4rem]">
        <Row
          label={t("sensors.hub")}
          tone={hub ? (hub.connected ? "ok" : "err") : "muted"}
          value={
            hub
              ? t("sensors.hubLine", {
                  state: hub.connected ? t("sensors.connected") : t("sensors.disconnected"),
                  transport: hub.transport,
                  crc: hub.crc_errors,
                  dropped: hub.dropped,
                })
              : t("sensors.notConfigured")
          }
        />
        <Row
          label={t("sensors.clock")}
          tone={c?.present ? (c.locked ? "ok" : "warn") : "muted"}
          value={
            c?.present
              ? t("sensors.clockLine", {
                  timebase: c.timebase ?? "--",
                  state: c.locked ? t("sensors.locked") : t("sensors.unlocked"),
                  sigma: formatSigma(c.sigma_ns),
                })
              : t("sensors.notConfigured")
          }
        />
        <Row
          label={t("sensors.gnss")}
          value={
            s.gnss
              ? t("sensors.gnssLine", { fix: s.gnss.fix, sats: s.gnss.sats, hdop: fmtNum(s.gnss.hdop, 1), rtk: s.gnss.rtk })
              : t("sensors.notConfigured")
          }
        />
        <SectionHeader>{t("cockpit.sensors.streams")}</SectionHeader>
        {s.sensors.length === 0 ? (
          <EmptyNote>{t("cockpit.sensors.noStreams")}</EmptyNote>
        ) : (
          s.sensors.map((row) => {
            const verdict = rateVerdict(row);
            return (
              <ListItem
                key={row.id}
                headline={`${row.id} · ${row.kind}`}
                supporting={t("cockpit.sensors.streamLine", {
                  role: row.role,
                  source: row.source,
                  age: row.age_ms == null ? DASH : t("sensors.ageMs", { ms: fmtNum(row.age_ms, 0) }),
                  sigma: formatSigma(row.sigma_ns),
                })}
                trailing={
                  <span className="flex items-center gap-[0.3rem] font-mono text-[0.8rem]">
                    <Dot tone={sensorTone(row)} />
                    {row.expected_hz == null
                      ? t("sensors.rateOnly", { rate: fmtNum(row.rate_hz, 1) })
                      : t("sensors.rateVs", { rate: fmtNum(row.rate_hz, 1), expected: fmtNum(row.expected_hz, 1) })}
                    {verdict !== "none" ? <span className="text-muted-foreground">{t(`sensors.verdict.${verdict}`)}</span> : null}
                  </span>
                }
              />
            );
          })
        )}
        {rigSection}
      </div>
    );
  }

  return (
    <ConsoleScaffold stale={sensors.stale || clock.stale} title={t("sensors.title")} actions={<StaleBadge stale={sensors.stale || clock.stale} />}>
      {body}
    </ConsoleScaffold>
  );
}
