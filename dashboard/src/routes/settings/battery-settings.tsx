import { useTranslation } from "react-i18next";

import { BatteryPackCard } from "@/components/battery/battery-pack-card";
import {
  ConfigNumberField,
  ConfigToggle,
} from "@/components/settings/config-fields";
import { Badge } from "@/components/ui/badge";
import { Card, CardContent } from "@/components/ui/card";
import { useBatteryHealth } from "@/hooks/use-battery-health";
import { useConfig, type AgentConfig } from "@/hooks/use-config";

type BatteryKey = Exclude<keyof NonNullable<AgentConfig["battery"]>, "enabled">;

/** Threshold fields with the bounds the agent config model enforces. */
const THRESHOLD_FIELDS = [
  {
    key: "low_cell_mv",
    labelKey: "settingsMore.battery.lowCellLabel",
    hintKey: "settingsMore.battery.lowCellHint",
    min: 2500,
    max: 4200,
  },
  {
    key: "critical_cell_mv",
    labelKey: "settingsMore.battery.criticalCellLabel",
    hintKey: "settingsMore.battery.criticalCellHint",
    min: 2500,
    max: 4000,
  },
  {
    key: "cell_divergence_mv",
    labelKey: "settingsMore.battery.divergenceLabel",
    hintKey: "settingsMore.battery.divergenceHint",
    min: 10,
    max: 500,
  },
  {
    key: "voltage_drop_mv_per_s",
    labelKey: "settingsMore.battery.sagLabel",
    hintKey: "settingsMore.battery.sagHint",
    min: 100,
    max: 5000,
  },
  {
    key: "temp_spike_dc_per_s",
    labelKey: "settingsMore.battery.tempSpikeLabel",
    hintKey: "settingsMore.battery.tempSpikeHint",
    min: 5,
    max: 200,
  },
  {
    key: "predictive_window_s",
    labelKey: "settingsMore.battery.windowLabel",
    hintKey: "settingsMore.battery.windowHint",
    min: 5,
    max: 300,
  },
  {
    key: "reserve_percent",
    labelKey: "settingsMore.battery.reserveLabel",
    hintKey: "settingsMore.battery.reserveHint",
    min: 5,
    max: 50,
  },
] as const satisfies ReadonlyArray<{
  key: BatteryKey;
  labelKey: string;
  hintKey: string;
  min: number;
  max: number;
}>;

function LiveSection() {
  const { t } = useTranslation();
  const health = useBatteryHealth();

  if (health.isLoading) {
    return <p className="text-[11px] text-muted-foreground/70">{t("settingsMore.battery.readingState")}</p>;
  }
  if (health.isError || !health.data) {
    return (
      <div className="rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-[11px] text-destructive">
        {t("settingsMore.battery.healthReadError")}
      </div>
    );
  }
  const { enabled, stale, packs } = health.data;
  if (!enabled) {
    return (
      <Card>
        <CardContent className="pt-5 pb-5 text-sm text-muted-foreground">
          {t("settingsMore.battery.monitoringOff")}
        </CardContent>
      </Card>
    );
  }
  return (
    <div className="space-y-3">
      {stale && (
        <div className="rounded-md border border-warn/40 bg-warn/5 px-3 py-2 text-[11px] text-warn">
          {t("settingsMore.battery.stale")}
        </div>
      )}
      {packs.length === 0 ? (
        <Card>
          <CardContent className="pt-5 pb-5 text-sm text-muted-foreground">
            {t("settingsMore.battery.noPacks")}
          </CardContent>
        </Card>
      ) : (
        packs.map((p) => <BatteryPackCard key={p.id} pack={p} />)
      )}
    </div>
  );
}

function ThresholdSection() {
  const { t } = useTranslation();
  const config = useConfig();

  if (config.isLoading) {
    return <p className="text-[11px] text-muted-foreground/70">{t("settingsMore.security.readingConfig")}</p>;
  }
  if (config.isError) {
    return (
      <div className="rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-[11px] text-destructive">
        {t("settingsMore.battery.configReadError")}
      </div>
    );
  }
  const battery = config.data?.battery;
  if (!battery) {
    return (
      <Card>
        <CardContent className="pt-5 pb-5 text-sm text-muted-foreground">
          {t("settingsMore.battery.thresholdsUnavailable")}
        </CardContent>
      </Card>
    );
  }

  return (
    <Card>
      <CardContent className="pt-5 pb-5 space-y-5">
        <ConfigToggle
          configKey="battery.enabled"
          label={t("settingsMore.battery.monitoringLabel")}
          hint={t("settingsMore.battery.monitoringHint")}
          value={battery.enabled}
        />
        {THRESHOLD_FIELDS.map((f) => (
          <div key={f.key} className="border-t border-border pt-5">
            <ConfigNumberField
              configKey={`battery.${f.key}`}
              id={`battery-${f.key}`}
              label={t(f.labelKey)}
              hint={t(f.hintKey)}
              value={battery[f.key]}
              integer
              min={f.min}
              max={f.max}
            />
          </div>
        ))}
      </CardContent>
    </Card>
  );
}

export function BatterySettings() {
  const { t } = useTranslation();
  return (
    <div className="space-y-8">
      <section className="space-y-3">
        <div className="flex items-center gap-2 text-xs font-medium uppercase tracking-wider text-muted-foreground">
          {t("settingsMore.battery.livePacks")}
          <Badge variant="default" className="font-normal">
            {t("settingsMore.battery.refresh")}
          </Badge>
        </div>
        <LiveSection />
      </section>

      <section className="space-y-3">
        <div className="text-xs font-medium uppercase tracking-wider text-muted-foreground">
          {t("settingsMore.battery.thresholds")}
        </div>
        <ThresholdSection />
      </section>
    </div>
  );
}
