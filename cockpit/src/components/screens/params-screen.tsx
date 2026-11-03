// Params — the MAVLink flight controller's parameters and live sensors.
//
// Parameters: the agent's cache (`GET /api/params`), categorised by prefix and
// decorated from the offline metadata catalog for the FC (enum labels, bitmask
// flags, ranges, units). Edits are session drafts; Save writes each one in turn
// (`POST /api/params/{name}`) behind a guarded hold, and only while the FC
// itself reports disarmed (`paramWriteBlock`). Sensors: the live vehicle state
// (`GET /api/telemetry`). A Betaflight/iNav FC has no parameter cache: it is
// configured on the MSP screen.

import { useCallback, useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";

import { BitmaskSheet, Field, FieldPanel, OptionSheet, PickerChip, ValueEntry } from "@/components/fc/fc-editors";
import { ConsoleScaffold } from "@/components/ui/console-scaffold";
import { ActionButton, EmptyNote, ProgressBar, StaleBadge } from "@/components/ui/data";
import { GuardedButton } from "@/components/ui/guarded-button";
import { ListItem } from "@/components/ui/list-item";
import { SettingRow, SwitchSettingRow } from "@/components/ui/setting-row";
import { TwoPane } from "@/components/ui/two-pane";
import { useFcIdentity } from "@/hooks/use-fc-identity";
import { useDraftGuard } from "@/hooks/use-draft-guard";
import { getTelemetry } from "@/lib/api";
import {
  displayValue,
  getParams,
  paramHint,
  parseEntry,
  postParam,
  saveParamDrafts,
  useParamDrafts,
  type FcIdentity,
} from "@/lib/fc-config";
import { loadParamMetadata, type ParamMetadata } from "@/shared/api/param-metadata";
import { buildRows, categoryCounts, filterRows, formatParamValue, paramWriteBlock, type ParamRow } from "@/shared/api/params";
import { toast } from "@/shared/api/toast";
import { DASH, fmtNum, fmtPct, fmtVoltage } from "@/shared/format";
import "@/shared/i18n";
import { useResource } from "@/shared/use-resource";

const PARAMS_POLL_MS = 10_000;
const TELEMETRY_POLL_MS = 1000;

/** The metadata catalog for the FC, loaded once per firmware type. */
function useParamMetadata(firmwareType: FcIdentity["firmwareType"] | undefined): Map<string, ParamMetadata> | null {
  const [meta, setMeta] = useState<{ key: string; map: Map<string, ParamMetadata> } | null>(null);
  useEffect(() => {
    if (!firmwareType || firmwareType === "unknown") return;
    let live = true;
    loadParamMetadata({ firmwareType })
      .then((map) => {
        if (live) setMeta({ key: firmwareType, map });
      })
      .catch(() => undefined);
    return () => {
      live = false;
    };
  }, [firmwareType]);
  return meta && meta.key === firmwareType ? meta.map : null;
}

export function ParamsScreen() {
  const { t } = useTranslation();
  const [tab, setTab] = useState(0);
  const identity = useFcIdentity();
  const msp = identity.data?.msp ?? null;

  let body;
  if (msp) body = <EmptyNote>{t("cockpit.params.mspFc")}</EmptyNote>;
  else if (tab === 0) body = <ParametersTab identity={identity.data} />;
  else body = <SensorsTab />;

  return (
    <ConsoleScaffold
      title={t("cockpit.params.title")}
      tabs={[t("telemetry.tabs.parameters"), t("telemetry.tabs.sensors")]}
      selectedTab={tab}
      onSelectTab={setTab}
      actions={<StaleBadge stale={identity.stale} />}
    >
      {body}
    </ConsoleScaffold>
  );
}

type Editing = { kind: "enum" | "bits"; row: ParamRow } | null;

function ParametersTab({ identity }: { identity: FcIdentity | null }) {
  const { t } = useTranslation();
  const params = useResource(useCallback((s: AbortSignal) => getParams(s), []), PARAMS_POLL_MS);
  const meta = useParamMetadata(identity?.firmwareType);
  const { drafts, setDraft, clearAll, discardDraft } = useParamDrafts();
  const [category, setCategory] = useState<string | null>(null);
  const [picked, setPicked] = useState(false);
  const [search, setSearch] = useState("");
  const [modifiedOnly, setModifiedOnly] = useState(false);
  const [progress, setProgress] = useState<{ done: number; total: number } | null>(null);
  const [editing, setEditing] = useState<Editing>(null);

  const writeBlock = paramWriteBlock(identity?.armed);
  const dirtyCount = drafts.size;
  // Drafts survive moving between screens; leaving with any asks first, and a
  // "discard" there drops them.
  useDraftGuard(dirtyCount > 0, "params", clearAll);

  const rows = useMemo(() => buildRows(params.data?.params ?? {}), [params.data]);
  const counts = useMemo(() => categoryCounts(rows), [rows]);
  const visible = useMemo(
    () => filterRows(rows, { category, search, modifiedOnly, modified: new Set(drafts.keys()) }),
    [rows, category, search, modifiedOnly, drafts],
  );

  async function save() {
    setProgress({ done: 0, total: drafts.size });
    const out = await saveParamDrafts(drafts, postParam, (done, total) => setProgress({ done, total }));
    for (const name of out.acked) discardDraft(name);
    for (const f of out.failed) toast.err(t("telemetry.params.saveItemFailed", { name: f.name }), f.message);
    setProgress(null);
    params.refresh();
    if (out.failed.length === 0 && out.pending.length === 0) {
      toast.ok(t("telemetry.params.savedCount", { count: out.acked.length }));
    } else {
      toast.info(
        t("telemetry.params.savedPartial", { ok: out.acked.length, failed: out.failed.length, pending: out.pending.length }),
        t("telemetry.params.savedPartialHint"),
      );
    }
  }

  const data = params.data;
  if (!params.ready) return <EmptyNote>{t("telemetry.params.loading")}</EmptyNote>;
  if (!data && params.error) {
    return (
      <div className="flex flex-col items-start gap-[0.5rem]">
        <EmptyNote>{t("telemetry.params.serviceUnreachable")}</EmptyNote>
        <ActionButton label={t("cockpit.fc.retry")} onClick={params.refresh} />
      </div>
    );
  }
  if (rows.length === 0) {
    if (data?.priming) {
      const got = data.progress?.got ?? 0;
      const expected = data.progress?.expected ?? 0;
      return (
        <div className="flex flex-col gap-[0.4rem]">
          <EmptyNote>{t("telemetry.params.priming")}</EmptyNote>
          <div className="text-[0.8rem] text-muted-foreground">
            {t("telemetry.params.primingProgress", { got, expected: expected || "?" })}
          </div>
          {expected > 0 ? <ProgressBar value={got} max={expected} /> : null}
        </div>
      );
    }
    if (data?.priming_timeout || data?.priming_send_failed) {
      return (
        <div className="flex flex-col gap-[0.4rem]">
          <EmptyNote>{t("telemetry.params.fcUnreachable")}</EmptyNote>
          <div className="text-[0.8rem] text-muted-foreground">
            {data.priming_send_failed ? t("telemetry.params.primingSendFailed") : t("telemetry.params.primingTimeout")}
          </div>
        </div>
      );
    }
    return (
      <div className="flex flex-col gap-[0.4rem]">
        <EmptyNote>{t("telemetry.params.emptyTitle")}</EmptyNote>
        <div className="text-[0.8rem] text-muted-foreground">{t("telemetry.params.emptyBody")}</div>
      </div>
    );
  }

  const categories = (
    <div className="flex flex-col gap-[0.15rem] pr-[0.4rem]">
      <ListItem
        headline={t("telemetry.params.all")}
        trailing={<span className="font-mono text-[0.8rem]">{rows.length}</span>}
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
      {visible.length === 0 ? <EmptyNote>{t("telemetry.params.noMatch")}</EmptyNote> : null}
      {visible.map((row) => {
        const m = meta?.get(row.name);
        const draft = drafts.get(row.name);
        const current = draft ? draft.draft : row.value;
        const hint = paramHint(m);
        const summary = [displayValue(current, m), hint].filter(Boolean).join(" · ");
        const readOnly = m?.readOnly === true;
        let control;
        if (m?.values && m.values.size > 0) {
          control = (
            <PickerChip
              text={m.values.get(current) ?? t("fc.enum.custom", { value: formatParamValue(current) })}
              label={row.name}
              dirty={!!draft}
              disabled={readOnly}
              onOpen={() => setEditing({ kind: "enum", row })}
            />
          );
        } else if (m?.bitmask && m.bitmask.size > 0) {
          control = (
            <PickerChip
              text={t("telemetry.params.setBits")}
              label={row.name}
              dirty={!!draft}
              disabled={readOnly}
              onOpen={() => setEditing({ kind: "bits", row })}
            />
          );
        } else {
          control = (
            <ValueEntry
              value={formatParamValue(current)}
              label={row.name}
              dirty={!!draft}
              disabled={readOnly}
              onCommit={(text) => {
                const v = parseEntry(text);
                if (v === null) toast.err(t("cockpit.params.badNumber", { name: row.name }));
                else setDraft(row.name, row.value, v);
              }}
            />
          );
        }
        return (
          <SettingRow
            key={row.name}
            title={<span className="font-mono" title={m?.humanName || m?.description || row.name}>{row.name}</span>}
            summary={summary}
          >
            {control}
          </SettingRow>
        );
      })}
    </div>
  );

  const editingMeta = editing ? meta?.get(editing.row.name) : undefined;
  const editingValue = editing ? (drafts.get(editing.row.name)?.draft ?? editing.row.value) : 0;

  return (
    <div className="flex h-full min-h-0 flex-col gap-[0.5rem]">
      <div className="flex flex-wrap items-center gap-[0.5rem]">
        <input
          type="search"
          value={search}
          onChange={(e) => setSearch(e.target.value)}
          placeholder={t("telemetry.params.searchPlaceholder")}
          aria-label={t("telemetry.params.searchPlaceholder")}
          className="touch-target min-w-[12rem] flex-1 rounded-md border border-border bg-input px-[0.6rem] text-[0.9rem] text-surface-foreground"
        />
        <div className="min-w-[12rem]">
          <SwitchSettingRow title={t("telemetry.params.modifiedOnly")} checked={modifiedOnly} onChange={setModifiedOnly} />
        </div>
        <ActionButton
          label={t("telemetry.params.discard")}
          disabled={dirtyCount === 0 || progress !== null}
          onClick={clearAll}
        />
        <GuardedButton
          label={
            progress
              ? t("cockpit.params.saving", { done: progress.done, total: progress.total })
              : t("telemetry.params.saveButton", { n: dirtyCount || "" })
          }
          guardLabel={t("cockpit.params.saveGuard", { count: dirtyCount })}
          disabled={dirtyCount === 0 || progress !== null || writeBlock !== null}
          onFired={() => void save()}
        />
      </div>
      {writeBlock && dirtyCount > 0 ? <p className="text-[0.8rem] text-warn">{writeBlock}</p> : null}
      {dirtyCount > 0 && !writeBlock ? (
        <p className="text-[0.75rem] text-muted-foreground">{t("telemetry.params.confirmWarning")}</p>
      ) : null}
      <div className="min-h-0 flex-1">
        <TwoPane showDetailInPortrait={picked} list={categories} detail={list} />
      </div>
      {editing?.kind === "enum" && editingMeta?.values ? (
        <OptionSheet
          title={editing.row.name}
          options={[...editingMeta.values.entries()]
            .sort((a, b) => a[0] - b[0])
            .map(([code, label]) => ({ key: String(code), label: `${formatParamValue(code)}: ${label}` }))}
          selected={String(editingValue)}
          onPick={(key) => setDraft(editing.row.name, editing.row.value, Number(key))}
          onDismiss={() => setEditing(null)}
        />
      ) : null}
      {editing?.kind === "bits" && editingMeta?.bitmask ? (
        <BitmaskSheet
          name={editing.row.name}
          bitmask={editingMeta.bitmask}
          value={editingValue}
          onApply={(next) => setDraft(editing.row.name, editing.row.value, next)}
          onDismiss={() => setEditing(null)}
        />
      ) : null}
    </div>
  );
}

function SensorsTab() {
  const { t } = useTranslation();
  const tel = useResource(useCallback((s: AbortSignal) => getTelemetry(s), []), TELEMETRY_POLL_MS);
  const v = tel.data;
  if (!v && tel.error) return <EmptyNote>{t("telemetry.sensors.readFailed")}</EmptyNote>;
  const deg = (rad: number | null | undefined) => (rad == null ? DASH : `${fmtNum((rad * 180) / Math.PI, 1)}°`);
  const lat = v?.position?.lat;
  const lon = v?.position?.lon;
  return (
    <div className="grid grid-cols-1 gap-[0.5rem] landscape:grid-cols-3">
      <FieldPanel title={t("telemetry.sensors.attitude")}>
        <Field label={t("telemetry.sensors.roll")} value={deg(v?.attitude?.roll)} />
        <Field label={t("telemetry.sensors.pitch")} value={deg(v?.attitude?.pitch)} />
        <Field label={t("telemetry.sensors.yaw")} value={deg(v?.attitude?.yaw)} />
      </FieldPanel>
      <FieldPanel title={t("telemetry.sensors.battery")}>
        <Field label={t("telemetry.sensors.voltage")} value={fmtVoltage(v?.battery?.voltage ?? null)} />
        <Field
          label={t("telemetry.sensors.current")}
          value={v?.battery?.current != null ? t("cockpit.fc.amps", { v: fmtNum(v.battery.current, 2) }) : DASH}
        />
        <Field label={t("telemetry.sensors.remaining")} value={fmtPct(v?.battery?.remaining ?? null)} />
      </FieldPanel>
      <FieldPanel title={t("telemetry.sensors.gps")}>
        <Field label={t("telemetry.sensors.fixType")} value={v?.gps?.fix_type != null ? String(v.gps.fix_type) : DASH} />
        <Field label={t("telemetry.sensors.sats")} value={v?.gps?.satellites != null ? String(v.gps.satellites) : DASH} />
        <Field label={t("telemetry.sensors.hdop")} value={fmtNum(v?.gps?.eph ?? null, 2)} />
        <Field label={t("telemetry.sensors.position")} value={lat != null && lon != null ? `${fmtNum(lat, 5)}, ${fmtNum(lon, 5)}` : DASH} />
        <Field
          label={t("telemetry.sensors.altRel")}
          value={v?.position?.alt_rel != null ? t("cockpit.fc.meters", { v: fmtNum(v.position.alt_rel, 1) }) : DASH}
        />
      </FieldPanel>
      <FieldPanel title={t("cockpit.params.vehicle")}>
        <Field label={t("telemetry.sensors.mode")} value={v?.mode ?? DASH} />
        <Field
          label={t("telemetry.sensors.armed")}
          value={
            v?.armed === true
              ? t("telemetry.sensors.armedValue")
              : v?.armed === false
                ? t("telemetry.sensors.disarmedValue")
                : DASH
          }
        />
      </FieldPanel>
    </div>
  );
}
