// MSP — configure a Betaflight or iNav flight controller. The agent is a
// byte-pipe for MSP, so the cockpit dials the FC over the agent's transparent
// proxy (`gs.mavlink_ws` ticket, WS :8765) and runs the codec itself.
//
// Settings: iNav reads the name-indexed MSP2 settings (labels and ranges from
// the FC); Betaflight dumps the CLI and overlays the bundled catalog. Save
// writes the drafts and persists them (EEPROM / `save noreboot`) behind a
// guarded hold; the client re-reads the FC's armed flag first and refuses
// unless it reports disarmed. Sensors: live MSP telemetry. CLI (Betaflight):
// the interactive CLI, opened only while disarmed; a line that reboots or
// wipes the FC goes out through a guarded control.

import { useMemo, useRef, useState, useEffect } from "react";
import { useTranslation } from "react-i18next";

import { Field, FieldPanel, OptionSheet, PickerChip, ValueEntry } from "@/components/fc/fc-editors";
import { ConsoleScaffold } from "@/components/ui/console-scaffold";
import { ActionButton, EmptyNote } from "@/components/ui/data";
import { GuardedButton } from "@/components/ui/guarded-button";
import { ListItem } from "@/components/ui/list-item";
import { SettingRow } from "@/components/ui/setting-row";
import { TwoPane } from "@/components/ui/two-pane";
import { useFcIdentity } from "@/hooks/use-fc-identity";
import { useDraftGuard } from "@/hooks/use-draft-guard";
import { useMspCli, useMspSettings, useMspTelemetry } from "@/hooks/use-msp";
import { cliCommandRisk, mspSaveBlock, parseEntry, useMspDrafts } from "@/lib/fc-config";
import type { MspVariant } from "@/shared/api/fc-firmware";
import type { MspSetting } from "@/shared/api/msp/fc-settings";
import { gpsFixLabel } from "@/shared/api/msp/telemetry-decoders";
import { toast } from "@/shared/api/toast";
import { DASH, fmtNum, fmtPct, fmtRelativeTime, fmtVoltage } from "@/shared/format";
import "@/shared/i18n";

function fwLabel(fw: MspVariant): string {
  return fw === "betaflight" ? "Betaflight" : "iNav";
}

export function MspScreen() {
  const { t } = useTranslation();
  const identity = useFcIdentity();
  const fw = identity.data?.msp ?? null;
  const [tab, setTab] = useState(0);

  const tabs = [t("cockpit.msp.tabSettings"), t("telemetry.tabs.sensors")];
  if (fw === "betaflight") tabs.push(t("cockpit.msp.tabCli"));

  let body;
  if (!identity.ready) body = <EmptyNote>{t("cockpit.msp.identifying")}</EmptyNote>;
  else if (!fw) body = <EmptyNote>{t("cockpit.msp.notMsp")}</EmptyNote>;
  else if (tab === 0)
    body = (
      <SettingsTab
        firmware={fw}
        firmwareVersion={identity.data?.firmwareVersion ?? undefined}
        saveBlock={mspSaveBlock(identity.data?.mspArmed)}
      />
    );
  else if (tab === 1) body = <SensorsTab firmware={fw} />;
  else body = <CliTab />;

  return (
    <ConsoleScaffold
      title={fw ? t("cockpit.msp.titleFw", { fw: fwLabel(fw) }) : t("cockpit.msp.title")}
      tabs={tabs}
      selectedTab={Math.min(tab, tabs.length - 1)}
      onSelectTab={setTab}
    >
      {body}
    </ConsoleScaffold>
  );
}

function SettingsTab({
  firmware,
  firmwareVersion,
  saveBlock,
}: {
  firmware: MspVariant;
  firmwareVersion?: string;
  /** Set while the agent last read the FC as armed: save is disabled. */
  saveBlock: string | null;
}) {
  const { t } = useTranslation();
  const { settings, loading, error, saving, refresh, apply } = useMspSettings(firmware, firmwareVersion);
  // Drafts outlive a tab or screen switch (each tab owns its own MSP session).
  const { drafts, setDraft, clearAll } = useMspDrafts();
  const [category, setCategory] = useState<string | null>(null);
  const [picked, setPicked] = useState(false);
  const [search, setSearch] = useState("");
  const [editing, setEditing] = useState<MspSetting | null>(null);

  const counts = useMemo(() => {
    const c: Record<string, number> = {};
    for (const s of settings) c[s.category] = (c[s.category] || 0) + 1;
    return c;
  }, [settings]);
  const visible = useMemo(() => {
    const q = search.trim().toLowerCase();
    return settings.filter((s) => (!category || s.category === category) && (!q || s.name.toLowerCase().includes(q)));
  }, [settings, category, search]);

  const dirtyCount = drafts.size;
  useDraftGuard(dirtyCount > 0, "msp", clearAll);

  const edit = (s: MspSetting, next: string) => setDraft(s.name, next, s.value);

  async function save() {
    const res = await apply([...drafts.entries()].map(([name, value]) => ({ name, value })));
    if (res.ok) {
      clearAll();
      toast.ok(res.message || t("fc.settings.savedToFc"));
    } else {
      toast.err(t("fc.settings.saveFailed"), res.message);
    }
  }

  if (loading) return <EmptyNote>{t("fc.settings.reading", { fw: fwLabel(firmware) })}</EmptyNote>;
  if (error) {
    return (
      <div className="flex flex-col items-start gap-[0.5rem]">
        <EmptyNote>{t("fc.settings.readFailed")}</EmptyNote>
        <div className="text-[0.8rem] text-muted-foreground">{error}</div>
        <ActionButton label={t("cockpit.fc.retry")} onClick={refresh} />
      </div>
    );
  }
  if (settings.length === 0) return <EmptyNote>{t("fc.settings.empty")}</EmptyNote>;

  const categories = (
    <div className="flex flex-col gap-[0.15rem] pr-[0.4rem]">
      <ListItem
        headline={t("telemetry.params.all")}
        trailing={<span className="font-mono text-[0.8rem]">{settings.length}</span>}
        selected={category === null}
        onClick={() => {
          setCategory(null);
          setPicked(true);
        }}
      />
      {Object.entries(counts)
        .sort(([a], [b]) => a.localeCompare(b))
        .map(([cat, n]) => (
          <ListItem
            key={cat}
            headline={<span className="font-mono">{cat}</span>}
            trailing={<span className="font-mono text-[0.8rem]">{n}</span>}
            selected={category === cat}
            onClick={() => {
              setCategory(cat);
              setPicked(true);
            }}
          />
        ))}
    </div>
  );

  const list = (
    <div className="flex flex-col gap-[0.1rem] pl-[0.4rem]">
      {visible.length === 0 ? <EmptyNote>{t("fc.settings.noMatch")}</EmptyNote> : null}
      {visible.map((s) => {
        const draft = drafts.get(s.name);
        const current = draft ?? s.value;
        const shown = draft !== undefined ? (s.options?.find((o) => o.send === draft)?.label ?? draft) : s.displayValue;
        const hint = s.range ? `${s.range.min}…${s.range.max}` : "";
        let control;
        if (s.options && s.options.length > 0) {
          control = (
            <PickerChip text={shown} label={s.name} dirty={draft !== undefined} onOpen={() => setEditing(s)} />
          );
        } else {
          control = (
            <ValueEntry
              value={current}
              label={s.name}
              numeric={!!s.range}
              dirty={draft !== undefined}
              onCommit={(text) => {
                if (s.range) {
                  const v = parseEntry(text);
                  if (v === null || v < s.range.min || v > s.range.max) {
                    toast.err(t("cockpit.msp.outOfRange", { name: s.name, min: s.range.min, max: s.range.max }));
                    return;
                  }
                }
                edit(s, text.trim());
              }}
            />
          );
        }
        return (
          <SettingRow key={s.name} title={<span className="font-mono">{s.name}</span>} summary={[shown, hint].filter(Boolean).join(" · ")}>
            {control}
          </SettingRow>
        );
      })}
    </div>
  );

  return (
    <div className="flex h-full min-h-0 flex-col gap-[0.5rem]">
      <div className="flex flex-wrap items-center gap-[0.5rem]">
        <input
          type="search"
          value={search}
          onChange={(e) => setSearch(e.target.value)}
          placeholder={t("fc.settings.searchPlaceholder", { fw: fwLabel(firmware) })}
          aria-label={t("fc.settings.searchPlaceholder", { fw: fwLabel(firmware) })}
          className="touch-target min-w-[12rem] flex-1 rounded-md border border-border bg-input px-[0.6rem] text-[0.9rem] text-surface-foreground"
        />
        <ActionButton
          label={t("telemetry.params.discard")}
          disabled={dirtyCount === 0 || saving}
          onClick={clearAll}
        />
        <GuardedButton
          label={saving ? t("fc.settings.saving") : t("telemetry.params.saveButton", { n: dirtyCount || "" })}
          guardLabel={t("cockpit.msp.saveGuard", { count: dirtyCount })}
          disabled={dirtyCount === 0 || saving || saveBlock !== null}
          onFired={() => void save()}
        />
      </div>
      {dirtyCount > 0 && saveBlock ? <p className="text-[0.8rem] text-warn">{saveBlock}</p> : null}
      {dirtyCount > 0 && !saveBlock ? (
        <p className="text-[0.75rem] text-muted-foreground">{t("cockpit.msp.saveNote")}</p>
      ) : null}
      <div className="min-h-0 flex-1">
        <TwoPane showDetailInPortrait={picked} list={categories} detail={list} />
      </div>
      {editing?.options ? (
        <OptionSheet
          title={editing.name}
          options={editing.options.map((o) => ({ key: o.send, label: o.label }))}
          selected={drafts.get(editing.name) ?? editing.value}
          onPick={(key) => edit(editing, key)}
          onDismiss={() => setEditing(null)}
        />
      ) : null}
    </div>
  );
}

function SensorsTab({ firmware }: { firmware: MspVariant }) {
  const { t } = useTranslation();
  const { snap: tel, stale } = useMspTelemetry(firmware);
  const fw = fwLabel(firmware);

  let header: string;
  let tone = "text-muted-foreground";
  if (!tel || tel.linkState === "connecting") header = t("fc.msp.connecting", { fw });
  else if (tel.linkState === "error") {
    header = tel.error ? t("fc.msp.linkErrorDetail", { error: tel.error }) : t("fc.msp.linkError");
    tone = "text-err";
  } else if (tel.linkState === "closed") {
    header = t("fc.msp.linkClosed");
    tone = "text-warn";
  } else if (tel.lastFrameAt == null) header = t("fc.msp.waitingFirstFrame", { fw });
  else {
    header = t("fc.msp.live", { fw }) + (stale ? t("fc.msp.stale", { age: fmtRelativeTime(tel.lastFrameAt) }) : "");
    tone = stale ? "text-warn" : "text-ok";
  }

  const gps = tel?.gps;
  const dopKind = gps?.dopKind ?? (firmware === "betaflight" ? "pdop" : "hdop");
  return (
    <div className="flex flex-col gap-[0.5rem]">
      <div className={`text-[0.8rem] ${tone}`}>{header}</div>
      <div className="grid grid-cols-1 gap-[0.5rem] landscape:grid-cols-3">
        <FieldPanel title={t("telemetry.sensors.attitude")}>
          <Field label={t("telemetry.sensors.roll")} value={tel?.attitude ? `${fmtNum(tel.attitude.roll, 1)}°` : DASH} />
          <Field label={t("telemetry.sensors.pitch")} value={tel?.attitude ? `${fmtNum(tel.attitude.pitch, 1)}°` : DASH} />
          <Field label={t("telemetry.sensors.yaw")} value={tel?.attitude ? `${fmtNum(tel.attitude.yaw, 0)}°` : DASH} />
        </FieldPanel>
        <FieldPanel title={t("telemetry.sensors.battery")}>
          <Field label={t("telemetry.sensors.voltage")} value={tel?.analog ? fmtVoltage(tel.analog.voltage) : DASH} />
          <Field
            label={t("telemetry.sensors.current")}
            value={tel?.analog ? t("cockpit.fc.amps", { v: fmtNum(tel.analog.amperage, 2) }) : DASH}
          />
          <Field label={t("fc.msp.drawn")} value={tel?.analog ? t("cockpit.msp.mah", { v: tel.analog.mAhDrawn }) : DASH} />
          <Field
            label={t("fc.msp.rssi")}
            value={tel?.analog ? fmtPct(Math.max(0, Math.min(100, (tel.analog.rssi / 1023) * 100))) : DASH}
          />
        </FieldPanel>
        <FieldPanel title={t("telemetry.sensors.gps")}>
          <Field label={t("fc.msp.fix")} value={gps ? gpsFixLabel(gps.fixType, firmware) : DASH} />
          <Field label={t("telemetry.sensors.sats")} value={gps ? String(gps.numSat) : DASH} />
          <Field
            label={dopKind === "pdop" ? t("fc.msp.pdop") : t("telemetry.sensors.hdop")}
            value={gps?.dop != null ? fmtNum(gps.dop, 2) : DASH}
          />
          <Field
            label={t("telemetry.sensors.position")}
            value={gps && (gps.lat !== 0 || gps.lon !== 0) ? `${fmtNum(gps.lat, 5)}, ${fmtNum(gps.lon, 5)}` : DASH}
          />
          <Field label={t("fc.msp.speed")} value={gps ? t("cockpit.fc.mps", { v: fmtNum(gps.speed / 100, 1) }) : DASH} />
        </FieldPanel>
        <FieldPanel title={t("fc.msp.altitudeTitle")}>
          <Field
            label={t("fc.msp.altitude")}
            value={tel?.altitude ? t("cockpit.fc.meters", { v: fmtNum(tel.altitude.altitude, 1) }) : DASH}
          />
          <Field
            label={t("fc.msp.vario")}
            value={tel?.altitude?.vario != null ? t("cockpit.fc.mps", { v: fmtNum(tel.altitude.vario / 100, 1) }) : DASH}
          />
        </FieldPanel>
        <FieldPanel title={t("fc.msp.statusTitle")}>
          <Field
            label={t("telemetry.sensors.armed")}
            value={
              tel?.status
                ? tel.status.armed
                  ? t("telemetry.sensors.armedValue")
                  : t("telemetry.sensors.disarmedValue")
                : DASH
            }
          />
          <Field label={t("fc.msp.cpuLoad")} value={tel?.status ? fmtPct(tel.status.cpuLoad) : DASH} />
          <Field label={t("fc.msp.cycleTime")} value={tel?.status ? t("cockpit.msp.micros", { v: tel.status.cycleTime }) : DASH} />
          <Field label={t("fc.msp.i2cErrors")} value={tel?.status ? String(tel.status.i2cErrors) : DASH} />
          {tel?.status?.hardwareFailure != null ? (
            <Field
              label={t("fc.msp.hardware")}
              value={tel.status.hardwareFailure ? t("fc.msp.fault") : t("fc.msp.healthy")}
            />
          ) : null}
        </FieldPanel>
        <FieldPanel title={t("telemetry.tabs.sensors")}>
          {tel?.sensors ? (
            <div className="flex flex-wrap gap-[0.3rem] pt-[0.2rem]">
              {tel.sensors.map((s) => (
                <span
                  key={s.labelKey}
                  className={`rounded border px-[0.35rem] py-[0.1rem] text-[0.7rem] uppercase tracking-wider ${
                    s.present ? "border-ok/40 text-ok" : "border-border text-muted-foreground"
                  }`}
                >
                  {t(s.labelKey)}
                  {s.present ? "" : t("cockpit.msp.sensorAbsent")}
                </span>
              ))}
            </div>
          ) : (
            <div className="text-[0.8rem] text-muted-foreground">{DASH}</div>
          )}
        </FieldPanel>
      </div>
      <FieldPanel title={t("fc.msp.rcChannels")}>
        {tel?.rc && tel.rc.length > 0 ? (
          <div className="grid grid-cols-4 gap-[0.4rem] pt-[0.2rem] landscape:grid-cols-8">
            {tel.rc.map((v, i) => (
              <div key={i} className="font-mono text-[0.8rem]">
                <div className="text-[0.7rem] text-muted-foreground">{t("fc.msp.channel", { n: i + 1 })}</div>
                <div>{v}</div>
              </div>
            ))}
          </div>
        ) : (
          <div className="text-[0.8rem] text-muted-foreground">{DASH}</div>
        )}
      </FieldPanel>
    </div>
  );
}

function CliTab() {
  const { t } = useTranslation();
  const cli = useMspCli();
  const [line, setLine] = useState("");
  const logRef = useRef<HTMLPreElement>(null);
  useEffect(() => {
    const el = logRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [cli.transcript]);

  if (cli.phase === "connecting") return <EmptyNote>{t("cockpit.msp.cliConnecting")}</EmptyNote>;
  if (cli.phase !== "open") {
    return (
      <div className="flex flex-col items-start gap-[0.5rem]">
        <EmptyNote>{cli.phase === "refused" ? t("cockpit.msp.cliRefused") : t("cockpit.msp.cliError")}</EmptyNote>
        {cli.reason ? <div className="text-[0.8rem] text-muted-foreground">{cli.reason}</div> : null}
        <ActionButton label={t("cockpit.fc.retry")} onClick={cli.reopen} />
      </div>
    );
  }

  const risky = cliCommandRisk(line) === "reboot";
  const send = () => {
    const l = line.trim();
    if (!l) return;
    cli.send(l);
    setLine("");
  };
  return (
    <div className="flex h-full min-h-0 flex-col gap-[0.5rem]">
      <pre
        ref={logRef}
        className="min-h-0 flex-1 overflow-y-auto whitespace-pre-wrap rounded-md bg-black/60 p-[0.6rem] font-mono text-[0.8rem] text-surface-foreground"
      >
        {cli.transcript}
      </pre>
      <div className="flex flex-wrap items-center gap-[0.5rem]">
        <input
          type="text"
          value={line}
          onChange={(e) => setLine(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && !risky) {
              e.preventDefault();
              send();
            }
          }}
          placeholder={t("cockpit.msp.cliPlaceholder")}
          aria-label={t("cockpit.msp.cliPlaceholder")}
          autoCapitalize="off"
          spellCheck={false}
          className="touch-target min-w-[12rem] flex-1 rounded-md border border-border bg-input px-[0.6rem] font-mono text-[0.9rem] text-surface-foreground"
        />
        {risky ? (
          <GuardedButton label={t("cockpit.msp.cliSendReboot")} guardLabel={t("cockpit.msp.cliGuard")} onFired={send} />
        ) : (
          <ActionButton label={t("cockpit.msp.cliSend")} variant="primary" disabled={!line.trim()} onClick={send} />
        )}
        <GuardedButton
          label={t("cockpit.msp.saveReboot")}
          guardLabel={t("cockpit.msp.cliGuard")}
          onFired={() => cli.send("save")}
        />
      </div>
      {risky ? <p className="text-[0.75rem] text-warn">{t("cockpit.msp.cliRebootNote")}</p> : null}
    </div>
  );
}
