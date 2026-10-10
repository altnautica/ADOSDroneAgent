// The safety band at the top of the Feed, always visible: exit (not on the
// kiosk), node name and reach, arm state, flight mode, battery with time to
// reserve, GPS, link, video, the pilot-in-command holder when reported, the
// preflight checks, the recording timer, the flight timer and a guarded Kill.
// Values never wrap; on a narrow panel the band scrolls sideways instead of
// overlapping anything. Every chip pairs its colour with text.

import { useEffect, useState, type ReactNode } from "react";
import {
  ArrowLeft,
  Battery,
  ClipboardCheck,
  OctagonX,
  Radio,
  Satellite,
  ShieldCheck,
  Timer,
  Video,
} from "lucide-react";
import type { LucideIcon } from "lucide-react";

import { useSkillContext } from "@/hooks/use-skill-context";
import { isKiosk } from "@/lib/kiosk";
import {
  armLabel,
  batteryLabel,
  fmtClock,
  gpsLabel,
  linkLabel,
  preflightChecks,
  reachBadge,
  secondsSince,
  videoLabel,
} from "@/lib/safety-band";
import { requestSkill } from "@/lib/skill-runner";
import { CORE_BY_ID, resolveSkillState } from "@/lib/skills";
import { cn } from "@/lib/utils";
import { probePairingInfo, useProfile } from "@/shared/use-profile";
import { useFeedStore } from "@/stores/feed-store";
import { useFlightStore } from "@/stores/flight-store";
import { useStatusStore } from "@/stores/status-store";

type Tone = "ok" | "warn" | "err" | "muted";
const TONE: Record<Tone, string> = {
  ok: "text-ok",
  warn: "text-warn",
  err: "text-err",
  muted: "text-hud-ink-2",
};

function Chip({ icon: Icon, tone = "muted", children, label }: { icon?: LucideIcon; tone?: Tone; children: ReactNode; label: string }) {
  return (
    <span className="flex shrink-0 items-center gap-[0.25rem] whitespace-nowrap font-mono text-[0.75rem]" aria-label={label}>
      {Icon ? <Icon className={cn("h-[0.85rem] w-[0.85rem]", TONE[tone])} aria-hidden /> : null}
      <span className={TONE[tone]}>{children}</span>
    </span>
  );
}

/** Re-renders its children once a second (timers only). */
function useSecondTick(): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const id = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(id);
  }, []);
  return now;
}

function RecTimer() {
  const recording = useStatusStore((s) => s.status?.recording === true || s.status?.video?.recording === true);
  const startedAt = useStatusStore((s) => s.status?.video?.recording_started_at ?? null);
  const now = useSecondTick();
  if (!recording) return null;
  // Elapsed from the agent's own start time, so it survives a remount.
  const elapsed = secondsSince(startedAt, now);
  return (
    <Chip tone="err" label="Recording">
      ● REC{elapsed !== null ? ` ${fmtClock(elapsed)}` : ""}
    </Chip>
  );
}

function FlightTimer() {
  const armedSince = useFlightStore((s) => s.armedSince);
  const now = useSecondTick();
  return (
    <Chip icon={Timer} label="Flight time">
      {armedSince !== null ? fmtClock((now - armedSince) / 1000) : "—"}
    </Chip>
  );
}

function usePreflight() {
  const telemetry = useFlightStore((s) => s.telemetry);
  const live = useFlightStore((s) => s.live);
  const home = useFlightStore((s) => s.home);
  const battery = useStatusStore((s) => s.battery);
  return preflightChecks({ telemetry, battery, live, home });
}

function Preflight({ open, setOpen }: { open: boolean; setOpen: (fn: (o: boolean) => boolean) => void }) {
  const checks = usePreflight();
  const passed = checks.filter((c) => c.ok).length;
  const tone: Tone = passed === checks.length ? "ok" : "warn";
  return (
    <span className="flex shrink-0">
      <button
        type="button"
        onClick={() => setOpen((o) => !o)}
        aria-expanded={open}
        className="flex min-h-[40px] items-center gap-[0.25rem] whitespace-nowrap rounded-md px-[0.3rem] font-mono text-[0.75rem] hover:bg-hud-hair"
      >
        <ClipboardCheck className={cn("h-[0.85rem] w-[0.85rem]", TONE[tone])} aria-hidden />
        <span className={TONE[tone]}>
          PREFLIGHT {passed}/{checks.length}
        </span>
      </button>
    </span>
  );
}

/** The preflight list, drawn below the band (outside its scroll area). */
function PreflightList() {
  const checks = usePreflight();
  return (
    <ul className="pointer-events-auto absolute right-0 top-full z-50 mt-[0.2rem] w-[16rem] rounded-lg border border-hud-hair bg-hud-glass-strong p-[0.4rem] backdrop-blur-hud">
      {checks.map((c) => (
        <li key={c.id} className="flex items-center gap-[0.4rem] py-[0.15rem] text-[0.75rem]">
          <span className={c.ok ? "text-ok" : "text-warn"}>{c.ok ? "PASS" : "FAIL"}</span>
          <span className="text-hud-ink">{c.label}</span>
        </li>
      ))}
    </ul>
  );
}

export function SafetyBand() {
  const profile = useProfile();
  const kiosk = isKiosk();
  const [name, setName] = useState<string | null>(null);
  useEffect(() => {
    let cancelled = false;
    void probePairingInfo().then((i) => {
      if (!cancelled) setName(i.name ?? null);
    });
    return () => {
      cancelled = true;
    };
  }, []);

  const telemetry = useFlightStore((s) => s.telemetry);
  const live = useFlightStore((s) => s.live);
  const status = useStatusStore((s) => s.status);
  const battery = useStatusStore((s) => s.battery);
  const video = useFeedStore((s) => s.video);
  const ctx = useSkillContext();
  const killState = resolveSkillState(CORE_BY_ID.kill, ctx);
  const [preflightOpen, setPreflightOpen] = useState(false);

  const reach = reachBadge(profile, typeof window !== "undefined" ? window.location.hostname : "");
  const arm = armLabel(live, telemetry?.armed);
  const pic = status?.gcs?.pic_id ?? null;
  const vTone: Tone = video.state === "live" ? (video.highLatency ? "warn" : "ok") : video.state === "connecting" ? "muted" : "err";

  return (
    <div className="relative shrink-0">
    <div
      role="region"
      aria-label="Safety band"
      className="pointer-events-auto flex h-[max(1.16rem,40px)] items-center gap-[0.6rem] overflow-x-auto rounded-lg border border-hud-hair bg-hud-glass px-[0.4rem] backdrop-blur-hud"
    >
      {!kiosk ? (
        <button
          type="button"
          onClick={() => window.location.assign("/")}
          aria-label="Exit to dashboard"
          className="flex min-h-[40px] min-w-[40px] shrink-0 items-center justify-center rounded-md text-hud-ink hover:bg-hud-hair"
        >
          <ArrowLeft className="h-[1rem] w-[1rem]" aria-hidden />
        </button>
      ) : null}
      <span className="flex min-w-0 shrink items-center gap-[0.3rem] whitespace-nowrap">
        <span className="max-w-[7rem] truncate text-[0.75rem] font-semibold text-hud-ink">{name ?? "—"}</span>
        {reach ? (
          <span className="shrink-0 rounded border border-hud-hair px-[0.25rem] font-mono text-[0.75rem] text-hud-primary">{reach}</span>
        ) : null}
      </span>
      <Chip tone={arm === "ARMED" ? "err" : arm === "DISARMED" ? "ok" : "muted"} label="Arm state">
        {arm}
      </Chip>
      <Chip label="Flight mode">{live ? (telemetry?.mode ?? "—") : "—"}</Chip>
      <Chip icon={Battery} label="Battery">
        {batteryLabel(battery, telemetry)}
      </Chip>
      <Chip icon={Satellite} label="GPS">
        {gpsLabel(telemetry, live)}
      </Chip>
      <Chip icon={Radio} label="Link">
        {linkLabel(status)}
      </Chip>
      <Chip icon={Video} tone={vTone} label="Video">
        {videoLabel(video)}
      </Chip>
      {pic ? (
        <Chip icon={ShieldCheck} label="Pilot in command">
          PIC {pic}
        </Chip>
      ) : null}
      <Preflight open={preflightOpen} setOpen={setPreflightOpen} />
      <RecTimer />
      <FlightTimer />
      <button
        type="button"
        onClick={() => requestSkill(CORE_BY_ID.kill, ctx)}
        disabled={!killState.enabled}
        title={killState.enabled ? "Kill (guarded)" : killState.reason}
        aria-label={killState.enabled ? "Kill, guarded" : `Kill: ${killState.reason ?? "unavailable"}`}
        className="ml-auto flex min-h-[40px] shrink-0 items-center gap-[0.25rem] whitespace-nowrap rounded-md border border-err/60 bg-err/15 px-[0.5rem] text-[0.75rem] font-semibold text-err disabled:opacity-45"
      >
        <OctagonX className="h-[0.9rem] w-[0.9rem]" aria-hidden />
        KILL
      </button>
    </div>
    {preflightOpen ? <PreflightList /> : null}
    </div>
  );
}
