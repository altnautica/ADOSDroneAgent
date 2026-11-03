// The last minute of battery voltage, RC RSSI, video bitrate and CPU on the
// System screen. Each headline is the source's current value (a dash once the
// source is gone); the ring only feeds the trace, so a dropped FC or stream
// never leaves a stale reading looking live. Self-polling; plain SVG, no chart
// library.

import { useCallback, useRef } from "react";
import { useTranslation } from "react-i18next";

import { pushSample, sparkPoints, type TimePoint } from "@/lib/home-panels";
import { apiFetch } from "@/shared/api-fetch";
import type { AgentHeartbeat, DashboardSnapshot } from "@/shared/api/types";
import { DASH } from "@/shared/format";
import "@/shared/i18n";
import { useResource } from "@/shared/use-resource";

const W = 120;
const H = 32;

interface Rings {
  battery: TimePoint[];
  rc: TimePoint[];
  bitrate: TimePoint[];
  cpu: TimePoint[];
}

function Spark({ label, unit, value, points, hot }: { label: string; unit: string; value: string; points: TimePoint[]; hot?: boolean }) {
  const { t } = useTranslation();
  return (
    <div className="flex flex-col gap-[0.2rem] rounded-md bg-input/40 px-[0.6rem] py-[0.4rem]">
      <div className="flex items-baseline justify-between">
        <span className="text-[0.75rem] uppercase tracking-wide text-muted-foreground">{label}</span>
        <span className="text-[0.7rem] text-muted-foreground">{t("panels.sparklines.last60s")}</span>
      </div>
      <div className="flex items-baseline gap-[0.25rem]">
        <span className={`font-mono text-[1.05rem] font-semibold ${hot ? "text-err" : "text-surface-foreground"}`}>{value}</span>
        <span className="text-[0.75rem] text-muted-foreground">{unit}</span>
      </div>
      <svg viewBox={`0 0 ${W} ${H}`} preserveAspectRatio="none" className="h-[2rem] w-full" aria-hidden>
        <polyline points={sparkPoints(points, W, H)} fill="none" stroke="currentColor" strokeWidth={1.5} className={hot ? "text-err" : "text-hud-primary"} vectorEffect="non-scaling-stroke" />
      </svg>
    </div>
  );
}

export function SparklinesCard() {
  const { t } = useTranslation();
  const rings = useRef<Rings>({ battery: [], rc: [], bitrate: [], cpu: [] });

  // Each successful poll appends its sample to the ring and hands a copy of
  // the rings back with the reading, so render never reads the ref.
  const snap = useResource(
    useCallback(
      (s: AbortSignal) =>
        apiFetch<DashboardSnapshot>("/api/v1/dashboard/snapshot", { signal: s }).then((d) => {
          const now = Math.floor(Date.now() / 1000);
          const r = rings.current;
          r.battery = pushSample(r.battery, now, d?.fc?.battery?.voltage);
          r.rc = pushSample(r.rc, now, d?.fc?.rc);
          r.bitrate = pushSample(r.bitrate, now, d?.video?.bitrate_kbps);
          return { d, battery: r.battery, rc: r.rc, bitrate: r.bitrate };
        }),
      [],
    ),
    1000,
  );
  const hb = useResource(
    useCallback(
      (s: AbortSignal) =>
        apiFetch<AgentHeartbeat>("/api/status", { signal: s }).then((d) => {
          rings.current.cpu = pushSample(rings.current.cpu, Math.floor(Date.now() / 1000), d?.health?.cpu_percent);
          return { d, cpu: rings.current.cpu };
        }),
      [],
    ),
    5000,
  );

  const num = (v: number | null | undefined) => (typeof v === "number" && !Number.isNaN(v) ? v : null);
  const bat = num(snap.data?.d?.fc?.battery?.voltage);
  const rc = num(snap.data?.d?.fc?.rc);
  const bitrate = num(snap.data?.d?.video?.bitrate_kbps);
  const cpu = num(hb.data?.d?.health?.cpu_percent);

  return (
    <div className="grid grid-cols-2 gap-[0.4rem] landscape:grid-cols-4">
      <Spark label={t("panels.sparklines.battery")} unit="V" value={bat != null ? bat.toFixed(2) : DASH} points={snap.data?.battery ?? []} />
      <Spark label={t("panels.sparklines.rcRssi")} unit="%" value={rc != null ? String(Math.round(rc)) : DASH} points={snap.data?.rc ?? []} />
      <Spark label={t("panels.sparklines.bitrate")} unit="kbps" value={bitrate != null ? String(Math.round(bitrate)) : DASH} points={snap.data?.bitrate ?? []} />
      <Spark label={t("panels.sparklines.cpu")} unit="%" value={cpu != null ? cpu.toFixed(0) : DASH} points={hb.data?.cpu ?? []} hot={cpu != null && cpu > 80} />
    </div>
  );
}
