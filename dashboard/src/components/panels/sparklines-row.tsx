import { Sparkline } from "@/components/chart/sparkline";
import { Card, CardContent } from "@/components/ui/card";
import { useHeartbeat } from "@/hooks/use-heartbeat";
import { useSnapshot } from "@/hooks/use-snapshot";
import { useTimeSeries } from "@/hooks/use-time-series";

interface CardProps {
  label: string;
  unit: string;
  value: string;
  data: { time: number; value: number }[];
  tone?: "primary" | "emerald" | "amber" | "red";
}

function MiniCard({ label, unit, value, data, tone = "primary" }: CardProps) {
  return (
    <Card>
      <CardContent className="pt-4 pb-3 space-y-2">
        <div className="flex items-baseline justify-between">
          <div className="text-[11px] uppercase tracking-wider text-muted-foreground">
            {label}
          </div>
          <div className="text-xs text-muted-foreground/70">last 60s</div>
        </div>
        <div className="flex items-baseline gap-1">
          <span className="font-mono text-xl tabular-nums leading-none">
            {value}
          </span>
          <span className="text-xs text-muted-foreground">{unit}</span>
        </div>
        <Sparkline data={data} tone={tone} height={48} />
      </CardContent>
    </Card>
  );
}

export function SparklinesRow() {
  const snap = useSnapshot();
  const heartbeat = useHeartbeat();

  const battery = useTimeSeries(snap.data?.fc?.battery?.voltage, (v) =>
    typeof v === "number" ? v : null,
  );
  const link = useTimeSeries(snap.data?.fc?.link_quality, (v) =>
    typeof v === "number" ? v : null,
  );
  const bitrate = useTimeSeries(snap.data?.video?.bitrate_kbps, (v) =>
    typeof v === "number" ? v : null,
  );
  const cpu = useTimeSeries(heartbeat.data?.health?.cpu_percent, (v) =>
    typeof v === "number" ? v : null,
  );

  const lastBat = battery[battery.length - 1]?.value;
  const lastLink = link[link.length - 1]?.value;
  const lastBitrate = bitrate[bitrate.length - 1]?.value;
  const lastCpu = cpu[cpu.length - 1]?.value;

  return (
    <div className="grid grid-cols-2 lg:grid-cols-4 gap-3">
      <MiniCard
        label="Battery"
        unit="V"
        value={lastBat != null ? lastBat.toFixed(2) : "—"}
        data={battery}
        tone={lastBat != null && lastBat < 14 ? "amber" : "emerald"}
      />
      <MiniCard
        label="Link"
        unit="%"
        value={lastLink != null ? Math.round(lastLink).toString() : "—"}
        data={link}
        tone="primary"
      />
      <MiniCard
        label="Bitrate"
        unit="kbps"
        value={lastBitrate != null ? Math.round(lastBitrate).toString() : "—"}
        data={bitrate}
        tone="primary"
      />
      <MiniCard
        label="CPU"
        unit="%"
        value={lastCpu != null ? lastCpu.toFixed(0) : "—"}
        data={cpu}
        tone={lastCpu != null && lastCpu > 80 ? "red" : "emerald"}
      />
    </div>
  );
}
