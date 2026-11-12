// System — the node's identity, health and box-level operations, for every
// profile. Reads the pairing info (name, mDNS reach name, profile, role), the
// status poll (version, uptime), `GET /api/system` (CPU / RAM / disk / temps),
// `GET /api/v1/diagnostics` (board identity), `GET /api/services` (the ados-*
// units) and, on a ground station, the recording list.
//
// Write actions: record start/stop, a guarded per-service restart
// (`POST /api/services/{name}/restart`) and a guarded board reboot
// (`POST /api/v1/setup/reboot`). Honest surfaces: a null reading renders a
// dash, a restart reply is judged by its body (the route answers 200 for every
// outcome), and buttons follow the next poll rather than an optimistic flip.

import { useCallback, useEffect, useState } from "react";
import { Check, Circle, Copy, Power, RotateCw, ScrollText, Square, Usb } from "lucide-react";
import { useTranslation } from "react-i18next";

import { HardwareCard } from "@/components/system/hardware-card";
import { SparklinesCard } from "@/components/system/sparklines-card";
import { FcCard } from "@/components/system/fc-card";
import { VideoCard } from "@/components/system/video-card";
import { NetworkCard } from "@/components/system/network-card";
import { CloudCard } from "@/components/system/cloud-card";
import { StatusTilesCard } from "@/components/system/status-tiles-card";
import { ConsoleScaffold } from "@/components/ui/console-scaffold";
import { ActionButton, Dot, EmptyNote, ErrorNote, MeterTile, Row, SectionHeader, StaleBadge, Tile, TileGrid } from "@/components/ui/data";
import { readFailed } from "@/lib/read-failed";
import { GuardedButton } from "@/components/ui/guarded-button";
import { ListItem } from "@/components/ui/list-item";
import { startRecording, stopRecording } from "@/lib/api";
import { fmtBytes, fmtClock, fmtGb, fmtMb } from "@/lib/format-status";
import { maxTemp, parseServices, restartVerdict, serviceTone, shortServiceName } from "@/lib/system-ops";
import { profileWordKey, roleWordKey } from "@/lib/profile-words";
import { serviceStateKey } from "@/lib/screen-words";
import { prettify } from "@/lib/settings-schema";
import { apiFetch } from "@/shared/api-fetch";
import { rebootAgent } from "@/shared/api/setup-actions";
import { toast, toastFromError } from "@/shared/api/toast";
import { DASH, fmtInt, fmtPct, fmtTemp, fmtUptime } from "@/shared/format";
import "@/shared/i18n";
import { probePairingInfo, useProfile, type PairingInfoLite } from "@/shared/use-profile";
import { useResource } from "@/shared/use-resource";
import { useConfigStore } from "@/stores/config-store";
import { useStatusStore } from "@/stores/status-store";
import type { ScreenAction } from "@/nav/navigator";

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

/** The node's mDNS reach name with a copy control. Only the name the agent
 *  reports avahi as publishing, never one built from the device label. */
function CopyHost({ host }: { host: string }) {
  const { t } = useTranslation();
  const [copied, setCopied] = useState(false);
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(host);
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1500);
    } catch {
      toast.err(t("cockpit.system.copyFailed"));
    }
  };
  const Icon = copied ? Check : Copy;
  return (
    <button
      type="button"
      onClick={() => void copy()}
      aria-label={t("layout.header.copyHost", { host })}
      className="touch-target inline-flex items-center gap-[0.35rem] rounded-md px-[0.4rem] font-mono text-[0.82rem] text-surface-foreground hover:bg-input/40"
    >
      <span className="select-text">{host}</span>
      <Icon className={copied ? "h-[0.9rem] w-[0.9rem] text-ok" : "h-[0.9rem] w-[0.9rem] text-muted-foreground"} aria-hidden />
    </button>
  );
}

export function SystemScreen({ dispatch }: { dispatch: (action: ScreenAction) => void }) {
  const { t } = useTranslation();
  const status = useStatusStore((s) => s.status);
  const profile = useProfile();
  // The recorder and its file list exist only on a ground station.
  const hasRecorder = profile === "ground_station";

  const [identity, setIdentity] = useState<PairingInfoLite | null>(null);
  useEffect(() => {
    let cancelled = false;
    void probePairingInfo().then((info) => {
      if (!cancelled) setIdentity(info);
    });
    return () => {
      cancelled = true;
    };
  }, [profile]);

  const sys = useResource<SystemResources>(
    useCallback((s) => apiFetch<SystemResources>("/api/system", { signal: s }), []),
    5000,
  );
  const diag = useResource<Diagnostics>(
    useCallback((s) => apiFetch<Diagnostics>("/api/v1/diagnostics", { signal: s }), []),
    10_000,
  );
  const services = useResource(
    useCallback(async (s: AbortSignal) => parseServices(await apiFetch<unknown>("/api/services", { signal: s })), []),
    8000,
  );
  const recording = useResource<RecordingList | null>(
    useCallback(
      (s) =>
        hasRecorder
          ? apiFetch<RecordingList>("/api/v1/ground-station/recording/list", { signal: s })
          : Promise.resolve(null),
      [hasRecorder],
    ),
    3000,
  );

  const [restarting, setRestarting] = useState<string | null>(null);
  const [rebooting, setRebooting] = useState(false);
  // Settings changes a service reads only at startup: the same list the shell
  // banner shows, here as a row any input can reach.
  const pendingReboot = useConfigStore((s) => s.pendingRebootPaths);
  const [recordBusy, setRecordBusy] = useState(false);

  const serviceWord = (v: string | null | undefined) => {
    const key = serviceStateKey(v);
    return key ? t(key) : DASH;
  };

  const r = sys.data;
  const box = status?.system;
  const temp = maxTemp(r?.temperatures) ?? box?.temp_c ?? null;
  const board = diag.data?.board;
  const inventory = services.data;
  // Null while neither the recorder nor the status has said: never "idle".
  const isRecording = recording.data?.recording ?? status?.recording ?? null;

  const restartService = async (name: string) => {
    if (restarting) return;
    const short = shortServiceName(name);
    setRestarting(name);
    try {
      const verdict = restartVerdict(
        await apiFetch<unknown>(`/api/services/${encodeURIComponent(name)}/restart`, { method: "POST", body: {} }),
      );
      if (verdict.ok) toast.ok(verdict.message ?? t("diagnostics.services.restarted", { name: short }));
      else toast.err(t("diagnostics.services.notRestarted", { name: short }), verdict.message ?? undefined);
    } catch (err) {
      toastFromError(err, t("diagnostics.services.restartFailed"));
    } finally {
      setRestarting(null);
      services.refresh();
    }
  };

  const reboot = async () => {
    if (rebooting) return;
    setRebooting(true);
    // If the board never goes down, return to the armed state so the operator
    // can retry rather than sit on "Rebooting…".
    window.setTimeout(() => setRebooting(false), 25_000);
    try {
      await rebootAgent();
      toast.ok(t("diagnostics.reboot.queued"), t("cockpit.system.rebootDetail"));
    } catch (err) {
      setRebooting(false);
      toastFromError(err, t("diagnostics.reboot.failed"));
    }
  };

  const toggleRecord = async () => {
    if (recordBusy) return;
    setRecordBusy(true);
    try {
      await (isRecording ? stopRecording() : startRecording());
    } catch (err) {
      toastFromError(err, t("cockpit.system.recordFailed"));
    } finally {
      setRecordBusy(false);
      recording.refresh();
    }
  };

  const role = identity?.role ?? null;
  const mdnsHost = identity?.mdns_host?.trim() || null;

  return (
    <ConsoleScaffold stale={sys.stale || services.stale}
      title={t("cockpit.system.title")}
      actions={
        <div className="flex items-center gap-[0.5rem]">
          <span className="font-mono text-[0.78rem] text-muted-foreground">{box?.agent_version ?? DASH}</span>
          <StaleBadge stale={sys.stale || services.stale} />
        </div>
      }
    >
      <div className="flex flex-col gap-[0.15rem]">
        <SectionHeader>{t("cockpit.system.identity")}</SectionHeader>
        <Row label={t("cockpit.system.name")} value={identity?.name?.trim() || DASH} />
        <Row label={t("cockpit.system.reachName")} value={mdnsHost ? <CopyHost host={mdnsHost} /> : DASH} />
        <Row
          label={t("cockpit.system.profile")}
          value={profile && profile !== "unknown" ? t(profileWordKey(profile)) : DASH}
          hint={role ? t("cockpit.system.roleHint", { role: t(roleWordKey(role)) }) : undefined}
        />
        <Row label={t("cockpit.system.version")} value={box?.agent_version ?? DASH} mono />
        <Row label={t("cockpit.system.uptime")} value={fmtUptime(box?.uptime_seconds)} />
        {board?.name ? (
          <Row label={t("cockpit.system.board")} value={board.name} hint={[board.soc, board.arch].filter(Boolean).join(" · ") || undefined} />
        ) : null}
        {board?.ram_total_mb ? <Row label={t("cockpit.system.ramInstalled")} value={fmtMb(board.ram_total_mb)} /> : null}
        {pendingReboot.length > 0 ? (
          <Row
            label={t("cockpit.system.rebootToApply")}
            value={t("cockpit.system.rebootPending", {
              paths: pendingReboot.map((p) => prettify(p.split(".").pop() ?? p)).join(", "),
            })}
            tone="warn"
            mono={false}
          />
        ) : null}
        <div className="mt-[0.3rem] flex">
          <GuardedButton
            label={
              rebooting
                ? t("diagnostics.reboot.rebooting")
                : pendingReboot.length > 0
                  ? t("cockpit.system.rebootToApply")
                  : t("diagnostics.reboot.button")
            }
            guardLabel={t("cockpit.system.rebootGuard")}
            icon={Power}
            disabled={rebooting}
            onFired={() => void reboot()}
          />
        </div>

        <SectionHeader>{t("cockpit.system.tools")}</SectionHeader>
        <ListItem
          icon={Usb}
          headline={t("peripherals.page.title")}
          supporting={t("cockpit.system.peripheralsHint")}
          onClick={() => dispatch({ kind: "open-detail", id: "peripherals" })}
        />
        <ListItem
          icon={ScrollText}
          headline={t("logs.page.title")}
          supporting={t("cockpit.system.logsHint")}
          onClick={() => dispatch({ kind: "open-detail", id: "logs" })}
        />

        <SectionHeader>{t("cockpit.system.health")}</SectionHeader>
        <StatusTilesCard />
        <SparklinesCard />
        <FcCard />
        <VideoCard />
        <NetworkCard />
        <CloudCard />
        <TileGrid>
          <MeterTile label={t("diagnostics.metrics.cpu")} value={r?.cpu_percent} display={fmtPct(r?.cpu_percent)} />
          <MeterTile
            label={t("diagnostics.metrics.memory")}
            value={r?.memory_used_mb}
            max={r?.memory_total_mb ?? undefined}
            display={r?.memory_total_mb ? `${fmtMb(r?.memory_used_mb)} / ${fmtMb(r.memory_total_mb)}` : fmtPct(r?.memory_percent)}
          />
          <MeterTile
            label={t("diagnostics.metrics.disk")}
            value={r?.disk_used_gb}
            max={r?.disk_total_gb ?? undefined}
            display={r?.disk_total_gb ? `${fmtGb(r?.disk_used_gb)} / ${fmtGb(r.disk_total_gb)}` : fmtPct(r?.disk_percent)}
          />
          <Tile
            label={t("diagnostics.metrics.temp")}
            value={fmtTemp(temp)}
            tone={temp != null && temp >= 80 ? "err" : temp != null && temp >= 70 ? "warn" : undefined}
          />
          <Tile label={t("cockpit.system.cores")} value={fmtInt(r?.cpu_count)} />
        </TileGrid>
        <HardwareCard />

        {hasRecorder ? (
          <>
            <SectionHeader>{t("cockpit.system.recording")}</SectionHeader>
            {isRecording == null ? (
              readFailed(recording) ? (
                <ErrorNote onRetry={recording.refresh}>{t("cockpit.readFailed.recording")}</ErrorNote>
              ) : (
                <EmptyNote>{t("common.loading")}</EmptyNote>
              )
            ) : (
            <div className="flex items-center gap-[0.5rem]">
              <Dot tone={isRecording ? "err" : "muted"} />
              <span className="flex-1 truncate text-[0.85rem] text-surface-foreground">
                {isRecording ? (recording.data?.current_filename ?? t("cockpit.system.recordingNow")) : t("cockpit.system.recordingIdle")}
              </span>
              <ActionButton
                label={isRecording ? t("cockpit.system.stop") : t("cockpit.system.record")}
                icon={isRecording ? Square : Circle}
                onClick={() => void toggleRecord()}
                variant={isRecording ? "danger" : "default"}
                busy={recordBusy}
              />
            </div>
            )}
            {recording.data?.items?.length ? (
              <div className="mt-[0.2rem] flex flex-col gap-[0.15rem]">
                {recording.data.items.slice(0, 4).map((it, i) => (
                  <Row key={(it.filename ?? "") + i} label={it.filename ?? DASH} hint={fmtClock(it.mtime)} value={fmtBytes(it.size_bytes)} />
                ))}
              </div>
            ) : null}
          </>
        ) : null}

        <SectionHeader>{t("diagnostics.services.title")}</SectionHeader>
        {inventory && !inventory.systemdAvailable ? (
          <EmptyNote>{t("cockpit.system.noSystemd")}</EmptyNote>
        ) : inventory?.services.length ? (
          inventory.services.map((s) => (
            <div key={s.name} className="flex items-center gap-[0.5rem] rounded-md bg-input/30 px-[0.6rem] py-[0.3rem]">
              <Dot tone={serviceTone(s)} />
              <div className="min-w-0 flex-1">
                <div className="truncate text-[0.82rem] text-surface-foreground">{shortServiceName(s.name)}</div>
                <div className="truncate text-[0.75rem] text-muted-foreground">
                  {serviceWord(s.state)}
                  {s.memory_mb != null ? ` · ${fmtMb(s.memory_mb)}` : ""}
                </div>
              </div>
              <GuardedButton
                label={restarting === s.name ? t("cockpit.system.restarting") : t("diagnostics.services.restart")}
                guardLabel={t("cockpit.system.restartGuard")}
                icon={RotateCw}
                disabled={restarting !== null}
                onFired={() => void restartService(s.name)}
              />
            </div>
          ))
        ) : readFailed(services) ? (
          <ErrorNote onRetry={services.refresh}>{t("cockpit.readFailed.services")}</ErrorNote>
        ) : (
          <EmptyNote>{services.ready ? t("cockpit.system.noServices") : t("diagnostics.services.loading")}</EmptyNote>
        )}
      </div>
    </ConsoleScaffold>
  );
}
