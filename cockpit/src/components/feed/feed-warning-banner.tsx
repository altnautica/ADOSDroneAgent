// A centre-top warning shown ON THE FEED only when the operator must act: the RF
// link is not carrying data (deaf / key mismatch / interference), the ground
// station's uplink is offline, or the battery is low. It reads only real fields
// from the shared status + flight telemetry — an unrecognised or absent
// condition shows NOTHING (never a permanent or fabricated banner). At most one
// warning is shown, highest-priority first, so the feed is never cluttered.

import { BatteryLow, WifiOff, type LucideIcon } from "lucide-react";

import { toneClass, type Tone } from "@/components/ui/data";
import { useFlightTelemetryContext } from "@/hooks/flight-telemetry-context";
import { useTelemetryContext } from "@/hooks/telemetry-context";
import { linkDiagView } from "@/lib/link-diag";
import type { GsStatus, VehicleState } from "@/lib/types";
import { cn } from "@/lib/utils";

interface Warning {
  tone: Tone;
  icon: LucideIcon;
  title: string;
  sub?: string;
}

/** The single highest-priority actionable warning, or null when there is none.
 *  Every branch is gated on a real field; nothing is fabricated. */
function computeWarning(status: GsStatus | null, telemetry: VehicleState | null): Warning | null {
  // 1) RF link — the agent's own verdict, only its actionable classes.
  const view = linkDiagView(status?.link?.link_diag);
  if (view?.actionable) {
    return { tone: view.tone, icon: view.icon, title: view.title, sub: view.hint };
  }

  // 2) Uplink offline — a ground station carries an uplink block; a drone's is
  //    empty, so this never fires falsely there.
  const uplinkType = status?.network?.uplink_type;
  if (uplinkType && status?.network?.uplink_reachable === false) {
    return { tone: "warn", icon: WifiOff, title: "Uplink offline", sub: `${uplinkType} is not reachable` };
  }

  // 3) Low battery — the vehicle's own percentage, else the paired-drone block.
  const remaining = telemetry?.battery?.remaining;
  const pct =
    typeof remaining === "number" && remaining >= 0
      ? remaining
      : (status?.paired_drone?.battery_pct ?? null);
  if (pct != null && pct <= 20) {
    return {
      tone: pct <= 10 ? "err" : "warn",
      icon: BatteryLow,
      title: "Battery low",
      sub: `${Math.round(pct)}% remaining`,
    };
  }

  return null;
}

export function FeedWarningBanner() {
  const { status } = useTelemetryContext();
  const { telemetry } = useFlightTelemetryContext();
  const warn = computeWarning(status, telemetry);
  if (!warn) return null;

  const Icon = warn.icon;
  const shell =
    warn.tone === "err" ? "bg-err/20 ring-err/40" : "bg-warn/20 ring-warn/40";

  return (
    <div
      className={cn(
        "pointer-events-none flex max-w-[42rem] items-center gap-[0.6rem] rounded-lg px-[0.9rem] py-[0.4rem] ring-1 backdrop-blur-sm",
        shell,
      )}
      role="alert"
    >
      <Icon className={cn("h-[1.3rem] w-[1.3rem] shrink-0", toneClass(warn.tone))} aria-hidden />
      <div className="min-w-0">
        <div className={cn("text-[0.9rem] font-semibold leading-tight", toneClass(warn.tone))}>
          {warn.title}
        </div>
        {warn.sub ? (
          <div className="truncate text-[0.72rem] text-surface-foreground/80">{warn.sub}</div>
        ) : null}
      </div>
    </div>
  );
}
