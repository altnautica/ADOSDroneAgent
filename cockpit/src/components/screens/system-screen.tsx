// System — box health and operations. Reads `GET /api/system` (CPU / RAM / disk /
// temperatures), the GS-status composite (agent version + uptime, drift-proof),
// `GET /api/v1/diagnostics` (board identity), `GET /api/services` (the ados-*
// unit list), and the recording list. Write actions: record start/stop and a
// two-tap-confirmed per-service restart (`POST /api/services/{name}/restart`).
// Honest surfaces: a null reading renders a dash, and a button reflects the
// agent's real state on the next poll rather than an optimistic flip.

import { useCallback, useState } from "react";
import { Circle, RotateCw, Square } from "lucide-react";

import { Panel, PanelHeader } from "@/components/ui/panel";
import {
  ActionButton,
  ConfirmButton,
  Dot,
  EmptyNote,
  MeterTile,
  Row,
  SectionHeader,
  StaleBadge,
  Tile,
  TileGrid,
  type Tone,
} from "@/components/ui/data";
import { useResource } from "@/hooks/use-resource";
import { useTelemetryContext } from "@/hooks/telemetry-context";
import { apiFetch, startRecording, stopRecording } from "@/lib/api";
import { DASH, fmtInt, fmtPct, fmtTemp, fmtUptime } from "@/lib/format";
import { fmtBytes, fmtClock, fmtGb, fmtMb } from "@/lib/format-status";

interface SystemResources {
  cpu_percent?: number | null;
  cpu_count?: number | null;
  memory_total_mb?: number | null;
  memory_used_mb?: number | null;
  memory_percent?: number | null;
  disk_total_gb?: number | null;
  disk_used_gb?: number | null;
  disk_percent?: number | null;
  temperatures?: Record<string, number>;
  available?: boolean;
}
interface ServiceEntry {
  name: string;
  active?: boolean;
  state?: string;
  sub_state?: string;
  memory_mb?: number | null;
}
interface ServicesResponse {
  services?: ServiceEntry[];
  systemd_available?: boolean;
}
interface Diagnostics {
  board?: { name?: string | null; soc?: string | null; arch?: string | null; ram_total_mb?: number | null };
}
interface RecordingItem {
  filename?: string;
  size_bytes?: number;
  mtime?: number;
}
interface RecordingList {
  recording?: boolean;
  current_filename?: string | null;
  items?: RecordingItem[];
}

function serviceTone(s: ServiceEntry): Tone {
  if (s.state === "failed" || s.sub_state === "failed") return "err";
  if (s.active) return "ok";
  if (s.state === "activating") return "warn";
  return "muted";
}

function maxTemp(temps: Record<string, number> | undefined): number | null {
  if (!temps) return null;
  const vals = Object.values(temps).filter((v) => typeof v === "number" && Number.isFinite(v));
  return vals.length ? Math.max(...vals) : null;
}

/** A short unit name without the `ados-`/`.service` decoration. */
function shortName(name: string): string {
  return name.replace(/^ados-/, "").replace(/\.service$/, "");
}

export function SystemScreen() {
  const { status } = useTelemetryContext();
  const sys = useResource<SystemResources>(
    useCallback((s) => apiFetch<SystemResources>("/api/system", { signal: s }), []),
    2000,
  );
  const diag = useResource<Diagnostics>(
    useCallback((s) => apiFetch<Diagnostics>("/api/v1/diagnostics", { signal: s }), []),
    5000,
  );
  const services = useResource<ServicesResponse>(
    useCallback((s) => apiFetch<ServicesResponse>("/api/services", { signal: s }), []),
    3000,
  );
  const recording = useResource<RecordingList>(
    useCallback((s) => apiFetch<RecordingList>("/api/v1/ground-station/recording/list", { signal: s }), []),
    3000,
  );

  const [restarting, setRestarting] = useState<string | null>(null);
  const [recordBusy, setRecordBusy] = useState(false);

  const r = sys.data;
  const box = status?.system;
  const temp = maxTemp(r?.temperatures) ?? box?.temp_c ?? null;
  const board = diag.data?.board;
  const svcList = services.data?.services ?? [];
  const isRecording = recording.data?.recording ?? status?.recording ?? false;

  const restartService = async (name: string) => {
    if (restarting) return;
    setRestarting(name);
    try {
      await apiFetch(`/api/services/${encodeURIComponent(name)}/restart`, { method: "POST", body: {} });
    } catch {
      // the next services poll reflects the real unit state.
    } finally {
      setRestarting(null);
      services.refresh();
    }
  };

  const toggleRecord = async () => {
    if (recordBusy) return;
    setRecordBusy(true);
    try {
      await (isRecording ? stopRecording() : startRecording());
    } catch {
      // the recording poll keeps the button honest.
    } finally {
      setRecordBusy(false);
      recording.refresh();
    }
  };

  return (
    <Panel>
      <PanelHeader
        title="System"
        right={
          <div className="flex items-center gap-[0.5rem]">
            <span className="font-mono text-[0.78rem] text-muted-foreground">{box?.agent_version ?? DASH}</span>
            <StaleBadge stale={sys.stale} />
          </div>
        }
      />

      <div className="flex flex-col gap-[0.15rem]">
        <SectionHeader>Box health</SectionHeader>
        <TileGrid>
          <MeterTile label="CPU" value={r?.cpu_percent} display={fmtPct(r?.cpu_percent)} />
          <MeterTile
            label="RAM"
            value={r?.memory_used_mb}
            max={r?.memory_total_mb ?? undefined}
            display={r?.memory_total_mb ? `${fmtMb(r?.memory_used_mb)} / ${fmtMb(r.memory_total_mb)}` : fmtPct(r?.memory_percent)}
          />
          <MeterTile
            label="Disk"
            value={r?.disk_used_gb}
            max={r?.disk_total_gb ?? undefined}
            display={r?.disk_total_gb ? `${fmtGb(r?.disk_used_gb)} / ${fmtGb(r.disk_total_gb)}` : fmtPct(r?.disk_percent)}
          />
          <Tile label="Temp" value={fmtTemp(temp)} tone={temp != null && temp >= 80 ? "err" : temp != null && temp >= 70 ? "warn" : undefined} />
          <Tile label="Uptime" value={fmtUptime(box?.uptime_seconds)} />
          <Tile label="Cores" value={fmtInt(r?.cpu_count)} />
        </TileGrid>

        <SectionHeader>Agent &amp; board</SectionHeader>
        <Row label="Agent version" value={box?.agent_version ?? DASH} />
        {board?.name ? <Row label="Board" value={board.name} hint={[board.soc, board.arch].filter(Boolean).join(" · ") || undefined} /> : null}
        {board?.ram_total_mb ? <Row label="RAM installed" value={fmtMb(board.ram_total_mb)} /> : null}

        <SectionHeader>Recording</SectionHeader>
        <div className="flex items-center gap-[0.5rem]">
          <Dot tone={isRecording ? "err" : "muted"} />
          <span className="flex-1 truncate text-[0.85rem] text-surface-foreground">
            {isRecording ? recording.data?.current_filename ?? "recording…" : "idle"}
          </span>
          <ActionButton
            label={isRecording ? "Stop" : "Record"}
            icon={isRecording ? Square : Circle}
            onClick={toggleRecord}
            variant={isRecording ? "danger" : "default"}
            busy={recordBusy}
          />
        </div>
        {recording.data?.items?.length ? (
          <div className="mt-[0.2rem] flex flex-col gap-[0.15rem]">
            {recording.data.items.slice(0, 4).map((it, i) => (
              <Row
                key={(it.filename ?? "") + i}
                label={it.filename ?? DASH}
                hint={fmtClock(it.mtime)}
                value={fmtBytes(it.size_bytes)}
              />
            ))}
          </div>
        ) : null}

        <SectionHeader>
          Services{services.data?.systemd_available === false ? " (systemd unavailable)" : ""}
        </SectionHeader>
        {svcList.length ? (
          svcList.map((s) => (
            <div key={s.name} className="flex items-center gap-[0.5rem] rounded-md bg-input/30 px-[0.6rem] py-[0.3rem]">
              <Dot tone={serviceTone(s)} />
              <div className="min-w-0 flex-1">
                <div className="truncate text-[0.82rem] text-surface-foreground">{shortName(s.name)}</div>
                <div className="truncate text-[0.66rem] text-muted-foreground">
                  {s.state ?? DASH}
                  {s.sub_state && s.sub_state !== s.state ? ` · ${s.sub_state}` : ""}
                  {s.memory_mb != null ? ` · ${fmtMb(s.memory_mb)}` : ""}
                </div>
              </div>
              <ConfirmButton
                label={`Restart ${shortName(s.name)}`}
                confirmLabel="Confirm restart"
                icon={RotateCw}
                onConfirm={() => restartService(s.name)}
                busy={restarting === s.name}
                compact
              />
            </div>
          ))
        ) : (
          <EmptyNote>No agent services reported.</EmptyNote>
        )}
      </div>
    </Panel>
  );
}
