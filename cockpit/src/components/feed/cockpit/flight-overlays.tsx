// The flow overlays over the video: the 3 s handover banner, the
// "Centre sticks to take control" gate ring, "Sticks are with <surface>" with
// its hold-to-take-over, the link-loss countdown, and the cancellable
// low-battery RTH countdown. Voice prompts speak handovers, RTH, low battery
// and mission-state changes. They stack in the Feed's alert column, under the
// alert cards and above the layer chips, so none covers another.

import { useEffect, useRef, useState } from "react";

import { HoldButton } from "@/components/ui/hold-button";
import { sticksElsewhere, usePilot } from "@/hooks/use-pilot";
import { useNow } from "@/hooks/use-now";
import { linkLossLine, visibleBanner, voicePrompt } from "@/lib/cockpit-flow";
import { cockpitText } from "@/lib/cockpit-text";
import { sendPilotAction, takePilotSession } from "@/lib/pilot-link";
import type { PilotState } from "@/shared/hud/cockpit-frames";
import { speak } from "@/stores/voice-store";

function GateRing({ progress }: { progress: number }) {
  const r = 26;
  const c = 2 * Math.PI * r;
  return (
    <div className="flex items-center gap-[0.6rem] rounded-full border border-hud-hair bg-hud-glass-strong px-[0.9rem] py-[0.4rem] backdrop-blur-hud" role="status" data-testid="gate-ring">
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
      <span className="text-[0.95rem] font-semibold text-hud-ink">{cockpitText("flow.gate.centre_sticks")}</span>
    </div>
  );
}

export function FlightOverlays() {
  const pilot = usePilot()?.data ?? null;
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

  return (
    <div className="pointer-events-none flex w-full flex-col items-center gap-[0.5rem] px-[0.5rem] empty:hidden" data-testid="flight-overlays">
      {pilot.rthCountdownS !== null ? (
        <div
          role="alert"
          data-testid="low-battery-banner"
          className="pointer-events-auto flex w-full max-w-[44rem] items-center gap-[0.8rem] rounded-lg border border-err bg-err/85 px-[0.9rem] py-[0.5rem] text-on-status"
        >
          <div className="flex-1">
            <div className="text-[1rem] font-semibold">{cockpitText("flow.low_battery.title", { in_s: Math.ceil(pilot.rthCountdownS) })}</div>
            <div className="text-[0.75rem] opacity-90">{cockpitText("flow.low_battery.cancel_warning")}</div>
          </div>
          <button
            type="button"
            onClick={() => void sendPilotAction("rth_cancel")}
            className="min-h-[48px] rounded-lg border border-on-status px-[0.9rem] font-semibold"
          >
            {cockpitText("flow.low_battery.cancel")}
          </button>
        </div>
      ) : null}
      {pilot.linkLossInS !== null ? (
        <div role="alert" className="rounded-lg border border-warn bg-warn/85 px-[0.9rem] py-[0.4rem] text-[0.95rem] font-semibold text-on-status">
          {linkLossLine(pilot)}
        </div>
      ) : null}
      {banner ? (
        <div role="status" data-testid="handover-banner" className="w-full max-w-[44rem] rounded-lg border border-hud-hair bg-hud-glass-strong px-[0.9rem] py-[0.5rem] text-center text-[1rem] font-semibold text-hud-ink backdrop-blur-hud">
          {banner}
        </div>
      ) : null}
      {pilot.gate?.pending ? <GateRing progress={pilot.gate.progress} /> : null}
      {elsewhere ? (
        <div className="pointer-events-auto flex items-center gap-[0.6rem] rounded-lg border border-hud-hair bg-hud-glass-strong px-[0.8rem] py-[0.35rem] backdrop-blur-hud" data-testid="sticks-elsewhere">
          <span className="text-[0.9rem] text-hud-ink">{cockpitText("flow.sticks_with", { label: elsewhere.label })}</span>
          <HoldButton holdMs={1500} onFired={() => takePilotSession()} className="min-h-[48px]">
            {cockpitText("flow.sticks_take_over")}
          </HoldButton>
        </div>
      ) : null}
    </div>
  );
}
