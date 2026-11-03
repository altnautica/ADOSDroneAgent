// Battery: the battery engine's live per-pack health (`GET /api/v1/battery`,
// every 2 s) and the warning thresholds, each a single config-leaf write that
// rolls back to the agent's value when refused.

import { useCallback } from "react";
import { Trans, useTranslation } from "react-i18next";

import { EmptyNote, StaleBadge, Tile, TileGrid } from "@/components/ui/data";
import { SwitchSettingRow } from "@/components/ui/setting-row";
import {
  BATTERY_POLL_MS,
  EVENT_STATE_LABEL,
  PREDICTION,
  RECENT_EVENTS,
  RULE_LABEL,
  SEVERITY_LABEL,
  THRESHOLD_FIELDS,
  fmtEta,
  fmtRuleValue,
  getBatteryDetail,
  liveView,
  type BatteryAnomaly,
  type BatteryPackDetail,
} from "@/lib/settings/battery";
import { toneClass } from "@/lib/tone";
import { cn } from "@/lib/utils";
import { DASH, fmtNum, fmtRelativeTime, fmtVoltage } from "@/shared/format";
import { useResource } from "@/shared/use-resource";
import type { AgentConfig } from "@/shared/api/types";

import { LeafNumberRow, SectionGroup } from "./section-kit";
import { useAgentConfig, useLeafWriter } from "./section-hooks";

export function BatterySection() {
  const { t } = useTranslation();
  return (
    <div className="flex flex-col gap-[0.8rem]">
      <SectionGroup title={`${t("settingsMore.battery.livePacks")} · ${t("settingsMore.battery.refresh")}`}>
        <LivePacks />
      </SectionGroup>
      <SectionGroup title={t("settingsMore.battery.thresholds")}>
        <Thresholds />
      </SectionGroup>
    </div>
  );
}

function LivePacks() {
  const { t } = useTranslation();
  const health = useResource(
    useCallback((s: AbortSignal) => getBatteryDetail(s), []),
    BATTERY_POLL_MS,
  );
  const view = liveView(health.data, health.ready);

  if (view === "loading") return <EmptyNote>{t("settingsMore.battery.readingState")}</EmptyNote>;
  if (view === "error") return <EmptyNote>{t("settingsMore.battery.healthReadError")}</EmptyNote>;
  if (view === "off") return <EmptyNote>{t("settingsMore.battery.monitoringOff")}</EmptyNote>;

  return (
    <div className="flex flex-col gap-[0.5rem]">
      {health.data?.stale || health.stale ? (
        <div className="flex items-center justify-between rounded-md bg-warn/10 px-[0.75rem] py-[0.4rem] text-[0.8rem] text-warn">
          <span>{t("settingsMore.battery.stale")}</span>
          <StaleBadge stale={health.stale} />
        </div>
      ) : null}
      {view === "empty" ? (
        <EmptyNote>{t("settingsMore.battery.noPacks")}</EmptyNote>
      ) : (
        health.data?.packs.map((p) => <PackCard key={p.id} pack={p} />)
      )}
    </div>
  );
}

function CellStrip({ pack }: { pack: BatteryPackDetail }) {
  const { t } = useTranslation();
  if (!pack.cells_plausible) {
    return <p className="px-[0.15rem] text-[0.8rem] text-muted-foreground">{t("settingsMore.battery.noCellData")}</p>;
  }
  return (
    <div className="flex flex-col gap-[0.3rem]">
      <div className="flex flex-wrap gap-[0.3rem]">
        {pack.cell_voltages_v.map((v, i) => (
          <div
            key={i}
            className={cn(
              "min-w-[4.5rem] rounded-md border px-[0.5rem] py-[0.25rem] text-center",
              i === pack.weakest_cell_index ? "border-warn/60 bg-warn/10 text-warn" : "border-border bg-input/40",
            )}
          >
            <div className="text-[0.7rem] text-muted-foreground">{t("settingsMore.battery.cellLabel", { n: i + 1 })}</div>
            <div className="font-mono text-[0.8rem]">{v.toFixed(3)}</div>
          </div>
        ))}
      </div>
      <p className="px-[0.15rem] text-[0.75rem] text-muted-foreground">
        {t("settingsMore.battery.cellSpread", {
          spread: pack.divergence_mv == null ? DASH : `${Math.round(pack.divergence_mv)} mV`,
          min: fmtVoltage(pack.min_cell_v),
          max: fmtVoltage(pack.max_cell_v),
        })}
      </p>
    </div>
  );
}

function AnomalyLine({ a }: { a: BatteryAnomaly }) {
  const { t } = useTranslation();
  return (
    <div className="flex items-center justify-between gap-[0.6rem] text-[0.8rem]">
      <div className="flex items-center gap-[0.5rem]">
        <span className={cn("font-semibold", toneClass(a.severity === "critical" ? "err" : "warn"))}>
          {t(SEVERITY_LABEL[a.severity])}
        </span>
        <span className="text-surface-foreground">{t(RULE_LABEL[a.rule])}</span>
        {a.cleared_at_ms != null ? <span className="text-muted-foreground">{t("settingsMore.battery.clearing")}</span> : null}
      </div>
      <span className="font-mono text-muted-foreground">
        {fmtRuleValue(a.rule, a.value)} / {fmtRuleValue(a.rule, a.threshold)}
      </span>
    </div>
  );
}

/** One pack: cells (weakest highlighted), electrical readings, the
 *  time-to-reserve prediction, active anomalies and recent raise/clear events. */
function PackCard({ pack }: { pack: BatteryPackDetail }) {
  const { t } = useTranslation();
  const prediction = PREDICTION[pack.prediction.state];
  const recent = pack.history.slice(0, RECENT_EVENTS);
  return (
    <div className="flex flex-col gap-[0.6rem] rounded-lg bg-input/30 p-[0.75rem]">
      <div className="flex items-center justify-between">
        <span className="text-[0.9rem] font-semibold text-surface-foreground">
          {t("settingsMore.battery.packTitle", { id: pack.id })}
        </span>
        <div className="flex items-center gap-[0.6rem] text-[0.8rem]">
          <span className={toneClass(prediction.tone)}>{t(prediction.labelKey)}</span>
          {pack.anomalies.length === 0 ? <span className="text-ok">{t("settingsMore.battery.healthy")}</span> : null}
        </div>
      </div>

      <CellStrip pack={pack} />

      <TileGrid>
        <Tile label={t("settingsMore.battery.statVoltage")} value={fmtVoltage(pack.voltage_v)} />
        <Tile label={t("settingsMore.battery.statCurrent")} value={pack.current_a == null ? DASH : `${fmtNum(pack.current_a)} A`} />
        <Tile label={t("settingsMore.battery.statRemaining")} value={pack.remaining_pct == null ? DASH : `${pack.remaining_pct}%`} />
        <Tile label={t("settingsMore.battery.statUsed")} value={pack.consumed_mah == null ? DASH : `${pack.consumed_mah} mAh`} />
        <Tile label={t("settingsMore.battery.statEnergy")} value={pack.consumed_wh == null ? DASH : `${fmtNum(pack.consumed_wh)} Wh`} />
        <Tile label={t("settingsMore.battery.statTemp")} value={pack.temperature_c == null ? DASH : `${fmtNum(pack.temperature_c)} °C`} />
      </TileGrid>

      <div className="rounded-md border border-border px-[0.6rem] py-[0.4rem] text-[0.8rem] text-muted-foreground">
        <Trans
          i18nKey="settingsMore.battery.predictionLine"
          values={{
            eta: fmtEta(pack.prediction.eta_s),
            drain: pack.prediction.drop_pct_per_s == null ? DASH : `${pack.prediction.drop_pct_per_s.toFixed(2)} %/s`,
            current: pack.prediction.mean_current_a == null ? DASH : `${fmtNum(pack.prediction.mean_current_a)} A`,
          }}
          components={{ v: <span className="font-mono text-surface-foreground" /> }}
        />
      </div>

      {pack.anomalies.length > 0 ? (
        <div className="flex flex-col gap-[0.3rem] border-t border-border pt-[0.5rem]">
          <span className="text-[0.75rem] uppercase tracking-wide text-muted-foreground">
            {t("settingsMore.battery.activeAnomalies")}
          </span>
          {pack.anomalies.map((a) => (
            <AnomalyLine key={a.rule} a={a} />
          ))}
        </div>
      ) : null}

      {recent.length > 0 ? (
        <div className="flex flex-col gap-[0.25rem] border-t border-border pt-[0.5rem]">
          <span className="text-[0.75rem] uppercase tracking-wide text-muted-foreground">
            {t("settingsMore.battery.recentEvents")}
          </span>
          {recent.map((e) => (
            <div key={`${e.rule}:${e.state}:${e.at_ms}`} className="flex items-center justify-between text-[0.8rem] text-muted-foreground">
              <span>
                <Trans
                  i18nKey="settingsMore.battery.eventLine"
                  values={{ rule: t(RULE_LABEL[e.rule]), state: t(EVENT_STATE_LABEL[e.state]), value: fmtRuleValue(e.rule, e.value) }}
                  components={{ rule: <span className="text-surface-foreground" /> }}
                />
              </span>
              <span>{fmtRelativeTime(e.at_ms)}</span>
            </div>
          ))}
        </div>
      ) : null}
    </div>
  );
}

function Thresholds() {
  const { t } = useTranslation();
  const { config, ready, get } = useAgentConfig();
  const { save } = useLeafWriter();

  if (!ready) return <EmptyNote>{t("settingsMore.security.readingConfig")}</EmptyNote>;
  if (config == null) return <EmptyNote>{t("settingsMore.battery.configReadError")}</EmptyNote>;
  const battery = get<AgentConfig["battery"] | null>("battery", null);
  if (!battery) return <EmptyNote>{t("settingsMore.battery.thresholdsUnavailable")}</EmptyNote>;

  return (
    <>
      <SwitchSettingRow
        title={t("settingsMore.battery.monitoringLabel")}
        summary={t("settingsMore.battery.monitoringHint")}
        checked={battery.enabled === true}
        onChange={(next) => void save("battery.enabled", next)}
      />
      {THRESHOLD_FIELDS.map((f) => {
        const value = battery[f.key];
        return (
          <LeafNumberRow
            key={f.key}
            title={t(f.labelKey)}
            summary={t(f.hintKey)}
            value={typeof value === "number" ? value : f.min}
            enabled={typeof value === "number"}
            min={f.min}
            max={f.max}
            step={f.step}
            onCommit={(next) => save(`battery.${f.key}`, next)}
          />
        );
      })}
    </>
  );
}
