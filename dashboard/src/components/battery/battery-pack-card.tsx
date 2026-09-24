import { Badge } from "@/components/ui/badge";
import { Card, CardContent } from "@/components/ui/card";
import type {
  BatteryAnomaly,
  BatteryPack,
  BatteryPredictionState,
  BatteryRuleId,
} from "@/hooks/use-battery-health";
import { fmtNum, fmtRelativeTime, fmtVoltage } from "@/lib/format";
import { cn } from "@/lib/utils";

const RULE_LABEL: Record<BatteryRuleId, string> = {
  cell_critical: "Cell critical",
  cell_low: "Cell low",
  cell_divergence: "Cell divergence",
  voltage_drop: "Voltage sag",
  temp_spike: "Temperature spike",
  predictive_low: "Reserve soon",
};

/** Formats an anomaly's value/threshold in the unit the engine reports for
 * that rule: cell rules in V, divergence in mV, sag in V/s, temperature in
 * °C/s, prediction in seconds to reserve. */
function fmtRuleValue(rule: BatteryRuleId, v: number): string {
  switch (rule) {
    case "cell_critical":
    case "cell_low":
      return `${v.toFixed(2)} V`;
    case "cell_divergence":
      return `${Math.round(v)} mV`;
    case "voltage_drop":
      return `${v.toFixed(2)} V/s`;
    case "temp_spike":
      return `${v.toFixed(1)} °C/s`;
    case "predictive_low":
      return `${Math.round(v)} s`;
  }
}

function fmtEta(s: number | null): string {
  if (s == null) return "—";
  const m = Math.floor(s / 60);
  const rem = s % 60;
  return m > 0 ? `${m}m ${String(rem).padStart(2, "0")}s` : `${rem}s`;
}

const PREDICTION: Record<
  BatteryPredictionState,
  { label: string; variant: "default" | "ok" | "warn" | "err" }
> = {
  idle: { label: "Idle", variant: "default" },
  normal: { label: "Draining", variant: "ok" },
  high: { label: "Draining fast", variant: "warn" },
  past: { label: "Past reserve", variant: "err" },
};

function Stat({ label, value }: { label: string; value: string }) {
  return (
    <div className="space-y-0.5">
      <div className="text-[11px] uppercase tracking-wider text-muted-foreground">
        {label}
      </div>
      <div className="font-mono text-sm">{value}</div>
    </div>
  );
}

function CellBar({ pack }: { pack: BatteryPack }) {
  if (!pack.cells_plausible) {
    return (
      <p className="text-xs text-muted-foreground">
        Per-cell data not reported by the flight controller.
      </p>
    );
  }
  return (
    <div className="space-y-1.5">
      <div className="flex flex-wrap gap-1.5">
        {pack.cell_voltages_v.map((v, i) => (
          <div
            key={i}
            className={cn(
              "min-w-[4.5rem] rounded-md border px-2 py-1 text-center",
              i === pack.weakest_cell_index
                ? "border-warn/60 bg-warn/10 text-warn"
                : "border-border bg-muted/30",
            )}
          >
            <div className="text-[10px] text-muted-foreground">C{i + 1}</div>
            <div className="font-mono text-xs">{v.toFixed(3)}</div>
          </div>
        ))}
      </div>
      <p className="text-[11px] text-muted-foreground">
        Spread {pack.divergence_mv == null ? "—" : `${Math.round(pack.divergence_mv)} mV`}
        {" · "}min {fmtVoltage(pack.min_cell_v)} · max {fmtVoltage(pack.max_cell_v)}
      </p>
    </div>
  );
}

function AnomalyRow({ a }: { a: BatteryAnomaly }) {
  const clearing = a.cleared_at_ms != null;
  return (
    <div className="flex items-center justify-between gap-3 text-xs">
      <div className="flex items-center gap-2">
        <Badge variant={a.severity === "critical" ? "err" : "warn"}>
          {a.severity}
        </Badge>
        <span className="font-medium">{RULE_LABEL[a.rule]}</span>
        {clearing && <span className="text-muted-foreground">clearing</span>}
      </div>
      <span className="font-mono text-muted-foreground">
        {fmtRuleValue(a.rule, a.value)} / {fmtRuleValue(a.rule, a.threshold)}
      </span>
    </div>
  );
}

/** One battery pack as reported by the agent battery engine: cells (weakest
 * highlighted), electrical readings, time-to-reserve prediction, the active
 * anomalies and the recent raise/clear history. */
export function BatteryPackCard({ pack }: { pack: BatteryPack }) {
  const prediction = PREDICTION[pack.prediction.state];
  const recent = pack.history.slice(0, 8);

  return (
    <Card>
      <CardContent className="pt-5 pb-5 space-y-5">
        <div className="flex items-center justify-between">
          <div className="text-sm font-semibold">Pack {pack.id}</div>
          <div className="flex items-center gap-2">
            <Badge variant={prediction.variant}>{prediction.label}</Badge>
            {pack.anomalies.length === 0 && <Badge variant="ok">Healthy</Badge>}
          </div>
        </div>

        <CellBar pack={pack} />

        <div className="grid grid-cols-3 gap-4 sm:grid-cols-6">
          <Stat label="Voltage" value={fmtVoltage(pack.voltage_v)} />
          <Stat
            label="Current"
            value={pack.current_a == null ? "—" : `${fmtNum(pack.current_a)} A`}
          />
          <Stat
            label="Remaining"
            value={pack.remaining_pct == null ? "—" : `${pack.remaining_pct}%`}
          />
          <Stat
            label="Used"
            value={pack.consumed_mah == null ? "—" : `${pack.consumed_mah} mAh`}
          />
          <Stat
            label="Energy"
            value={pack.consumed_wh == null ? "—" : `${fmtNum(pack.consumed_wh)} Wh`}
          />
          <Stat
            label="Temp"
            value={pack.temperature_c == null ? "—" : `${fmtNum(pack.temperature_c)} °C`}
          />
        </div>

        <div className="rounded-md border border-border px-3 py-2 text-xs text-muted-foreground">
          Time to reserve{" "}
          <span className="font-mono text-foreground">
            {fmtEta(pack.prediction.eta_s)}
          </span>
          {" · "}drain{" "}
          <span className="font-mono text-foreground">
            {pack.prediction.drop_pct_per_s == null
              ? "—"
              : `${pack.prediction.drop_pct_per_s.toFixed(2)} %/s`}
          </span>
          {" · "}mean current{" "}
          <span className="font-mono text-foreground">
            {pack.prediction.mean_current_a == null
              ? "—"
              : `${fmtNum(pack.prediction.mean_current_a)} A`}
          </span>
        </div>

        {pack.anomalies.length > 0 && (
          <div className="space-y-2 border-t border-border pt-4">
            <div className="text-xs font-medium uppercase tracking-wider text-muted-foreground">
              Active anomalies
            </div>
            {pack.anomalies.map((a) => (
              <AnomalyRow key={a.rule} a={a} />
            ))}
          </div>
        )}

        {recent.length > 0 && (
          <div className="space-y-1.5 border-t border-border pt-4">
            <div className="text-xs font-medium uppercase tracking-wider text-muted-foreground">
              Recent events
            </div>
            {recent.map((e) => (
              <div
                key={`${e.rule}:${e.state}:${e.at_ms}`}
                className="flex items-center justify-between text-xs text-muted-foreground"
              >
                <span>
                  <span className="text-foreground">{RULE_LABEL[e.rule]}</span>{" "}
                  {e.state} at {fmtRuleValue(e.rule, e.value)}
                </span>
                <span>{fmtRelativeTime(e.at_ms)}</span>
              </div>
            ))}
          </div>
        )}
      </CardContent>
    </Card>
  );
}
