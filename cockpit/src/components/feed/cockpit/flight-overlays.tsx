// The flow overlays over the video: a running launch sequence with its
// Abort, the 3 s handover banner, the "Centre sticks to take control" gate
// ring, "Sticks are with <surface>" with its hold-to-take-over, the link-loss
// countdown, and the cancellable low-battery RTH countdown. Voice prompts
// speak handovers, RTH, low battery and mission-state changes. They stack in
// the Feed's alert column, under the alert cards and above the layer chips,
// so none covers another. Everything here reads a fresh pilot frame only: a
// stalled pilot clears them all (the chip turns LINK instead). Sizes are CSS
// px; the column scales them by the class scale.

import { useEffect, useRef, useState } from "react";

import { HoldButton } from "@/components/ui/hold-button";
import { sticksElsewhere, useFreshPilot } from "@/hooks/use-pilot";
import { useNow } from "@/hooks/use-now";
import { holdMs, launchRunning, launchStepText, visibleBanner, voicePrompt } from "@/lib/cockpit-flow";
import { sendPilotAction, takePilotSession } from "@/lib/pilot-link";
import type { PilotState } from "@/shared/hud/cockpit-frames";
import { linkLossLine } from "@/shared/hud/link-loss";
import { specText } from "@/shared/hud/spec-text";
import { abortLaunch } from "@/stores/cockpit-ui-store";
import { speak } from "@/stores/voice-store";

function GateRing({ progress }: { progress: number }) {
  const r = 26;
  const c = 2 * Math.PI * r;
  return (
    <div className="flex items-center gap-[10px] rounded-full border border-hud-hair bg-hud-glass-strong px-[14px] py-[6px] backdrop-blur-hud" role="status" data-testid="gate-ring">
      <svg width="64" height="64" viewBox="0 0 64 64" aria-hidden>
        <circle cx="32" cy="32" r={r} fill="none" stroke="currentColor" strokeOpacity="0.25" strokeWidth="6" />
        <circle
          cx="32"
          cy="32"
          r={r}
          fill="none"
          stroke="currentColor"
          strokeWidth="6"
          strokeDasharray={c}
          strokeDashoffset={c * (1 - Math.max(0, Math.min(1, progress)))}
          transform="rotate(-90 32 32)"
          className="text-primary"
        />
      </svg>
      <span className="text-[15px] font-semibold text-hud-ink">{specText("flow.gate.centre_sticks")}</span>
    </div>
  );
}

export function FlightOverlays() {
  const pilot = useFreshPilot();
  const now = useNow(pilot?.banner || pilot?.rthCountdownS != null ? 250 : null);
  const [bannerSeen, setBannerSeen] = useState<{ key: string; at: number } | null>(null);
  const prev = useRef<PilotState | null>(null);

  // When a banner first appeared (from the ticking clock), adjusted while
  // rendering when the banner changes.
  const bannerKey = pilot?.banner ? `${pilot.banner.textKey}:${JSON.stringify(pilot.banner.args)}` : null;
  if (bannerKey === null ? bannerSeen !== null : bannerSeen?.key !== bannerKey) {
    setBannerSeen(bannerKey === null ? null : { key: bannerKey, at: now });
  }

  useEffect(() => {
    if (!pilot) return;
    const prompt = voicePrompt(prev.current, pilot);
    prev.current = pilot;
    if (prompt) speak(prompt);
  }, [pilot]);

  if (!pilot) return null;
  const banner = visibleBanner(pilot.banner, bannerSeen?.at ?? now, now);
  const elsewhere = sticksElsewhere(pilot);
  const seq = launchRunning(pilot.sequence) ? pilot.sequence : null;

  return (
    <div className="pointer-events-none flex w-full flex-col items-center gap-[8px] px-[8px] empty:hidden" data-testid="flight-overlays">
      {seq ? (
        <div
          role="status"
          data-testid="launch-sequence"
          className="pointer-events-auto flex items-center gap-[12px] rounded-lg border border-hud-hair bg-hud-glass-strong px-[14px] py-[6px] backdrop-blur-hud"
        >
          <span className="text-[15px] font-semibold text-hud-ink">
            {specText("flow.launch.title")} · {launchStepText(seq.step)}
          </span>
          <button
            type="button"
            onClick={abortLaunch}
            data-testid="launch-abort"
            className="min-h-[48px] rounded-lg border border-err bg-err/20 px-[14px] text-[15px] font-semibold text-err"
          >
            {specText("flow.launch.abort")}
          </button>
        </div>
      ) : null}
      {pilot.rthCountdownS !== null ? (
        <div
          role="alert"
          data-testid="low-battery-banner"
          className="pointer-events-auto flex w-full max-w-[704px] items-center gap-[12px] rounded-lg border border-err bg-err/85 px-[14px] py-[8px] text-on-status"
        >
          <div className="flex-1">
            <div className="text-[16px] font-semibold">{specText("flow.low_battery.title", { in_s: Math.ceil(pilot.rthCountdownS) })}</div>
            <div className="text-[12px] opacity-90">{specText("flow.low_battery.cancel_warning")}</div>
          </div>
          <button
            type="button"
            onClick={() => void sendPilotAction("rth_cancel")}
            className="min-h-[48px] rounded-lg border border-on-status px-[14px] font-semibold"
          >
            {specText("flow.low_battery.cancel")}
          </button>
        </div>
      ) : null}
      {pilot.linkLossInS !== null ? (
        <div role="alert" data-testid="link-loss-banner" className="rounded-lg border border-warn bg-warn/85 px-[14px] py-[6px] text-[15px] font-semibold text-on-status">
          {linkLossLine(pilot)}
        </div>
      ) : null}
      {banner ? (
        <div role="status" data-testid="handover-banner" className="w-full max-w-[704px] rounded-lg border border-hud-hair bg-hud-glass-strong px-[14px] py-[8px] text-center text-[16px] font-semibold text-hud-ink backdrop-blur-hud">
          {banner}
        </div>
      ) : null}
      {pilot.gate?.pending ? <GateRing progress={pilot.gate.progress} /> : null}
      {elsewhere ? (
        <div className="pointer-events-auto flex items-center gap-[10px] rounded-lg border border-hud-hair bg-hud-glass-strong px-[12px] py-[6px] backdrop-blur-hud" data-testid="sticks-elsewhere">
          <span className="text-[14px] text-hud-ink">{specText("flow.sticks_with", { label: elsewhere.label })}</span>
          <HoldButton holdMs={holdMs("take_control")} onFired={() => takePilotSession()} className="min-h-[48px]">
            {specText("flow.sticks_take_over")}
          </HoldButton>
        </div>
      ) : null}
    </div>
  );
}
