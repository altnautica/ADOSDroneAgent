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
const THRESHOLD_FIELDS: ReadonlyArray<{
  key: BatteryKey;
  label: string;
  hint: string;
  min: number;
  max: number;
}> = [
  {
    key: "low_cell_mv",
    label: "Low cell (mV)",
    hint: "A cell below this raises a warning. Must stay above the critical level.",
    min: 2500,
    max: 4200,
  },
  {
    key: "critical_cell_mv",
    label: "Critical cell (mV)",
    hint: "A cell below this raises a critical alert. Must stay below the low level.",
    min: 2500,
    max: 4000,
  },
  {
    key: "cell_divergence_mv",
    label: "Cell divergence (mV)",
    hint: "Spread between the highest and lowest cell that flags an unbalanced pack.",
    min: 10,
    max: 500,
  },
  {
    key: "voltage_drop_mv_per_s",
    label: "Voltage sag (mV/s)",
    hint: "Pack voltage falling faster than this between samples flags a sag.",
    min: 100,
    max: 5000,
  },
  {
    key: "temp_spike_dc_per_s",
    label: "Temperature spike (0.1 °C/s)",
    hint: "Temperature rising faster than this flags a spike. 50 means 5.0 °C/s.",
    min: 5,
    max: 200,
  },
  {
    key: "predictive_window_s",
    label: "Prediction window (s)",
    hint: "How much recent history the time-to-reserve estimate averages over.",
    min: 5,
    max: 300,
  },
  {
    key: "reserve_percent",
    label: "Reserve (%)",
    hint: "The remaining charge the time-to-reserve estimate counts down to.",
    min: 5,
    max: 50,
  },
];

function LiveSection() {
  const health = useBatteryHealth();

  if (health.isLoading) {
    return <p className="text-[11px] text-muted-foreground/70">Reading battery state…</p>;
  }
  if (health.isError || !health.data) {
    return (
      <div className="rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-[11px] text-destructive">
        Could not read battery health from this node.
      </div>
    );
  }
  const { enabled, stale, packs } = health.data;
  if (!enabled) {
    return (
      <Card>
        <CardContent className="pt-5 pb-5 text-sm text-muted-foreground">
          Battery monitoring is off. Turn it on below to see live pack health.
        </CardContent>
      </Card>
    );
  }
  return (
    <div className="space-y-3">
      {stale && (
        <div className="rounded-md border border-warn/40 bg-warn/5 px-3 py-2 text-[11px] text-warn">
          No fresh battery data from the flight controller for over 5 s. Values
          below are the last known readings.
        </div>
      )}
      {packs.length === 0 ? (
        <Card>
          <CardContent className="pt-5 pb-5 text-sm text-muted-foreground">
            No battery reported by the flight controller yet.
          </CardContent>
        </Card>
      ) : (
        packs.map((p) => <BatteryPackCard key={p.id} pack={p} />)
      )}
    </div>
  );
}

function ThresholdSection() {
  const config = useConfig();

  if (config.isLoading) {
    return <p className="text-[11px] text-muted-foreground/70">Reading config…</p>;
  }
  if (config.isError) {
    return (
      <div className="rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-[11px] text-destructive">
        Could not read the battery config from this node.
      </div>
    );
  }
  const battery = config.data?.battery;
  if (!battery) {
    return (
      <Card>
        <CardContent className="pt-5 pb-5 text-sm text-muted-foreground">
          Battery thresholds are not exposed by this agent version.
        </CardContent>
      </Card>
    );
  }

  return (
    <Card>
      <CardContent className="pt-5 pb-5 space-y-5">
        <ConfigToggle
          configKey="battery.enabled"
          label="Battery health monitoring"
          hint="Evaluate cell, sag, temperature and time-to-reserve rules on every pack and log each alert."
          value={battery.enabled}
        />
        {THRESHOLD_FIELDS.map((f) => (
          <div key={f.key} className="border-t border-border pt-5">
            <ConfigNumberField
              configKey={`battery.${f.key}`}
              id={`battery-${f.key}`}
              label={f.label}
              hint={f.hint}
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
  return (
    <div className="space-y-8">
      <section className="space-y-3">
        <div className="flex items-center gap-2 text-xs font-medium uppercase tracking-wider text-muted-foreground">
          Live packs
          <Badge variant="default" className="font-normal">
            2 s refresh
          </Badge>
        </div>
        <LiveSection />
      </section>

      <section className="space-y-3">
        <div className="text-xs font-medium uppercase tracking-wider text-muted-foreground">
          Thresholds
        </div>
        <ThresholdSection />
      </section>
    </div>
  );
}
