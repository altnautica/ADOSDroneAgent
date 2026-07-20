// L2 — the flight-instrument HUD drawn over the video: a centred artificial
// horizon, a speed tape (left) and altitude tape (right), and the bottom
// telemetry strip. Purely informational and non-interactive (pointer-events
// pass through to the feed and the action bar). It insets its instruments to
// clear the shell chrome that floats over the feed: the top status strip, the
// left menu rail (when shown), and the bottom action dock. Flight values come
// from the shared flight telemetry and are gated on `live`, so tapes read a
// dash rather than a stale number when the link is silent.

import { AttitudeIndicator } from "@/components/feed/attitude-indicator";
import { FeedTelemetryStrip } from "@/components/feed/feed-telemetry-strip";
import { VerticalTape } from "@/components/feed/vertical-tape";
import { useFlightTelemetryContext } from "@/hooks/flight-telemetry-context";
import { useNavStore } from "@/stores/nav-store";
import { cn } from "@/lib/utils";

export function FeedHud() {
  const { telemetry, live } = useFlightTelemetryContext();
  const menuCollapsed = useNavStore((s) => s.menuCollapsed);

  const speed = live
    ? (telemetry?.velocity?.groundspeed ?? telemetry?.velocity?.airspeed ?? null)
    : null;
  const altitude = live ? (telemetry?.position?.alt_rel ?? null) : null;

  // The left tape and top-left overlays clear the menu rail only while it is
  // shown (landscape); when the menu is collapsed they sit at the edge.
  const leftInset = menuCollapsed
    ? "left-[0.5rem]"
    : "left-[0.5rem] landscape:left-[6.9rem]";

  return (
    <div className="pointer-events-none absolute inset-0 z-10">
      {/* artificial horizon, centred in the region between the top strip and
          the bottom dock */}
      <div className="absolute inset-x-0 bottom-[9.4rem] top-[2.6rem] flex items-center justify-center">
        <div className="aspect-square h-[min(58vmin,100%)]">
          <AttitudeIndicator />
        </div>
      </div>

      {/* speed tape (left) */}
      <div
        className={cn(
          "absolute bottom-[9.8rem] top-[3rem] w-[3.6rem]",
          leftInset,
        )}
      >
        <VerticalTape
          side="left"
          label="Spd"
          unit="m/s"
          value={speed}
          windowSpan={20}
          step={5}
        />
      </div>

      {/* altitude tape (right) */}
      <div className="absolute bottom-[9.8rem] right-[0.5rem] top-[3rem] w-[3.8rem]">
        <VerticalTape
          side="right"
          label="Alt"
          unit="m"
          value={altitude}
          windowSpan={60}
          step={10}
        />
      </div>

      {/* bottom telemetry strip, above the action dock */}
      <div className="absolute inset-x-0 bottom-[8rem] flex justify-center">
        <FeedTelemetryStrip />
      </div>
    </div>
  );
}
