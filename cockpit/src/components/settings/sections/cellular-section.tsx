// Settings → Cellular. A ground station shows its live modem (presence and
// facts polled every 10 s) and writes through the ground-station modem routes,
// rendering each write's read-back; every other profile has no modem manager
// and writes the `network.cellular.*` config leaves. Turning the uplink off and
// changing the APN are slides; turning it on and the data cap write at once.

import { useState } from "react";
import { useTranslation } from "react-i18next";

import { Toggle } from "@/components/settings/widgets";
import { EmptyNote, Row, StaleBadge } from "@/components/ui/data";
import { SettingRow, TextSettingRow } from "@/components/ui/setting-row";
import {
  CELLULAR_POLL_MS,
  capMbToGb,
  changeFailedKey,
  changeSavedKey,
  configLeaf,
  modemFacts,
  parseCapGb,
  presenceKey,
  submitCellular,
  type CellularChange,
} from "@/lib/settings/cellular";
import { getModemStatus, getModemView, setModem, type ModemView } from "@/shared/api/modem";
import { toast, toastFromError } from "@/shared/api/toast";
import { useProfile } from "@/shared/use-profile";
import { useResource } from "@/shared/use-resource";

import { ApplyBar, SectionGroup, SlideConfirmDialog } from "./section-kit";
import { useAgentConfig, useLeafWriter } from "./section-hooks";

// An example APN value, not copy.
const APN_PLACEHOLDER = "internet";

export function CellularSection() {
  const { t } = useTranslation();
  const profile = useProfile();
  if (profile === null) return <EmptyNote>{t("cockpit.settingsSections.loading")}</EmptyNote>;
  return profile === "ground_station" ? <GsCellular /> : <ConfigCellular />;
}

function GsCellular() {
  const { t } = useTranslation();
  const view = useResource(() => getModemView(), CELLULAR_POLL_MS);
  const presence = useResource(() => getModemStatus(), CELLULAR_POLL_MS);
  // A write's read-back, laid over the snapshot it was written against.
  const [readBack, setReadBack] = useState<{ of: ModemView | null; view: ModemView | null }>({
    of: null,
    view: null,
  });
  const modem = readBack.view && readBack.of === view.data ? readBack.view : view.data;

  const write = async (change: CellularChange): Promise<boolean> => {
    try {
      const next = await setModem(change);
      setReadBack({ of: view.data, view: next });
      toast.ok(t(changeSavedKey(change, true)));
      return true;
    } catch (err) {
      toastFromError(err, t(changeFailedKey(change, true)));
      return false;
    }
  };

  const facts = modem ? modemFacts(modem) : null;
  const unknown = t("settings.shared.unknown");

  return (
    <div className="flex flex-col gap-[0.6rem]">
      {view.ready && !modem ? <EmptyNote>{t("settings.cellular.readFailed")}</EmptyNote> : null}

      <SectionGroup title={t("settings.cellular.modemTitle")}>
        <Row
          label={t("settings.cellular.presence")}
          value={t(presenceKey(presence.data))}
          left={<StaleBadge stale={view.stale || presence.stale} />}
          mono={false}
        />
        {facts ? (
          <>
            <Row label={t("settings.cellular.state")} value={facts.state ?? unknown} />
            {facts.operator ? <Row label={t("settings.cellular.operator")} value={facts.operator} /> : null}
            {facts.technology ? <Row label={t("settings.cellular.technology")} value={facts.technology} /> : null}
            {facts.signalQuality !== null ? (
              <Row label={t("settings.cellular.signal")} value={`${facts.signalQuality}%`} />
            ) : null}
            {facts.ip ? <Row label={t("settings.cellular.ip")} value={facts.ip} /> : null}
            {facts.capMb !== null && facts.capMb > 0 ? (
              <Row
                label={t("settings.cellular.dataUsed")}
                value={`${facts.usedMb ?? 0} / ${facts.capMb} MB (${facts.percent !== null ? facts.percent.toFixed(0) : "0"}%)`}
              />
            ) : (
              <Row label={t("settings.cellular.dataUsed")} value={t("settings.cellular.noCap")} mono={false} />
            )}
          </>
        ) : null}
      </SectionGroup>

      {modem ? (
        <CellularWrites
          enabled={modem.enabled === true}
          apn={typeof modem.apn === "string" ? modem.apn : ""}
          apnHint={t("settings.cellular.apnHintExample")}
          enableHint={t("settings.cellular.uplinkHintGs")}
          capGb={capMbToGb(facts?.capMb)}
          write={write}
        />
      ) : null}
    </div>
  );
}

function ConfigCellular() {
  const { t } = useTranslation();
  const { config, ready, get } = useAgentConfig();
  const { save } = useLeafWriter();

  if (!ready) return <EmptyNote>{t("settings.shared.readingConfig")}</EmptyNote>;
  if (config == null) return <EmptyNote>{t("settings.cellular.configReadFailed")}</EmptyNote>;

  const write = async (change: CellularChange): Promise<boolean> => {
    const leaf = configLeaf(change);
    if (!leaf) return false;
    const ok = await save(leaf.key, leaf.value, { quiet: true });
    if (ok) toast.ok(t(changeSavedKey(change, false)));
    return ok;
  };

  return (
    <div className="flex flex-col gap-[0.6rem]">
      <p className="px-[0.75rem] text-[0.8rem] text-muted-foreground">{t("settings.cellular.noModemManagerNote")}</p>
      <CellularWrites
        enabled={get<boolean>("network.cellular.enabled", false)}
        apn={get<string>("network.cellular.apn", "")}
        apnHint={t("settings.cellular.apnHint")}
        enableHint={t("settings.cellular.uplinkHintConfig")}
        capGb={null}
        write={write}
      />
    </div>
  );
}

/** The uplink switch, the APN and (on a live modem) the data cap. A disable or
 *  an APN change waits in `staged` for its slide. */
function CellularWrites({
  enabled,
  apn,
  apnHint,
  enableHint,
  capGb,
  write,
}: {
  enabled: boolean;
  apn: string;
  apnHint: string;
  enableHint: string;
  /** The cap as a GB string, or null where the data cap does not apply. */
  capGb: string | null;
  write: (change: CellularChange) => Promise<boolean>;
}) {
  const { t } = useTranslation();
  const [busy, setBusy] = useState(false);
  const [staged, setStaged] = useState<CellularChange | null>(null);
  const [apnDraft, setApnDraft] = useState<string | null>(null);
  const [capDraft, setCapDraft] = useState<string | null>(null);

  const run = async (change: CellularChange) => {
    setBusy(true);
    try {
      const ok = await write(change);
      if (ok && "apn" in change) setApnDraft(null);
      if (ok && "cap_gb" in change) setCapDraft(null);
    } finally {
      setBusy(false);
    }
  };
  const submit = (change: CellularChange) =>
    submitCellular(change, { write: (c) => void run(c), stage: setStaged });

  const apnValue = apnDraft ?? apn;
  const apnDirty = apnDraft !== null && apnDraft.trim() !== apn;
  const capValue = capDraft ?? capGb ?? "";
  const capDirty = capDraft !== null && capDraft !== capGb;
  const capParsed = parseCapGb(capValue);

  return (
    <>
      <SectionGroup>
        <SettingRow title={t("settings.cellular.uplinkTitle")} summary={enableHint}>
          <Toggle
            on={enabled}
            disabled={busy}
            label={t("settings.cellular.uplinkTitle")}
            onChange={(next) => submit({ enabled: next })}
          />
        </SettingRow>
      </SectionGroup>

      <SectionGroup>
        <TextSettingRow
          title={t("settings.cellular.apn")}
          summary={apnHint}
          value={apnValue}
          placeholder={APN_PLACEHOLDER}
          enabled={!busy}
          onChange={setApnDraft}
        />
        <ApplyBar
          dirty={apnDirty}
          busy={busy}
          onApply={() => submit({ apn: apnValue.trim() })}
          onRevert={() => setApnDraft(null)}
        />
      </SectionGroup>

      {capGb !== null ? (
        <SectionGroup>
          <TextSettingRow
            title={t("settings.cellular.capLabel")}
            summary={capDirty && capParsed === null ? t("settings.cellular.capInvalid") : t("settings.cellular.capHint")}
            value={capValue}
            placeholder="0"
            inputMode="decimal"
            enabled={!busy}
            onChange={setCapDraft}
          />
          <ApplyBar
            dirty={capDirty}
            canApply={capParsed !== null}
            busy={busy}
            onApply={() => capParsed !== null && submit({ cap_gb: capParsed })}
            onRevert={() => setCapDraft(null)}
          />
        </SectionGroup>
      ) : null}

      {staged && "enabled" in staged ? (
        <SlideConfirmDialog
          title={t("cockpit.settingsSections.cellular.disableTitle")}
          slideLabel={t("cockpit.settingsSections.cellular.disableSlide")}
          onConfirmed={() => {
            setStaged(null);
            void run(staged);
          }}
          onDismiss={() => setStaged(null)}
        >
          {t("cockpit.settingsSections.cellular.disableBody")}
        </SlideConfirmDialog>
      ) : null}

      {staged && "apn" in staged ? (
        <SlideConfirmDialog
          title={t("cockpit.settingsSections.cellular.apnTitle")}
          slideLabel={t("cockpit.settingsSections.cellular.apnSlide")}
          onConfirmed={() => {
            setStaged(null);
            void run(staged);
          }}
          onDismiss={() => setStaged(null)}
        >
          <p className="font-mono text-surface-foreground">{staged.apn}</p>
          <p>{t("cockpit.settingsSections.cellular.apnBody")}</p>
        </SlideConfirmDialog>
      ) : null}
    </>
  );
}
