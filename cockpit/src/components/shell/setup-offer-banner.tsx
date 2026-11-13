// The one-time aircraft setup offer: a non-modal banner in the shell's
// banner slot (outside the flight screen's zones) when the agent's setup
// engine reports a flight controller whose ground wizard is not done and
// whose uid this device never offered. On the ground only. Open goes to
// Settings › Aircraft setup; Open and Later both mark the uid offered. The
// offer also stands as the Menu's "Set up aircraft" row, which panel and
// controller input can reach. The buttons keep the 48 px touch floor.

import { Wrench } from "lucide-react";
import { useEffect, useState } from "react";

import { useFreshPilot } from "@/hooks/use-pilot";
import { useWidthClass } from "@/hooks/use-width-class";
import { setupOfferUid } from "@/lib/aircraft-setup";
import { gridMeasures } from "@/lib/cockpit-layout";
import { bannerTextStyle } from "@/lib/shell-banners";
import { liveArmed, tourAirborne } from "@/lib/controller-tour";
import { specText } from "@/shared/hud/spec-text";
import { fetchSetupStatus } from "@/shared/setup-wizard/client";
import type { SetupStatus } from "@/shared/setup-wizard/types";
import { useProfile } from "@/shared/use-profile";
import { useFlightStore } from "@/stores/flight-store";
import { activeScreenId, useNavStore } from "@/stores/nav-store";
import { useSetupOfferStore } from "@/stores/setup-offer-store";

/** How often the setup engine is asked, ms. */
const SETUP_POLL_MS = 30_000;

export function SetupOfferBanner() {
  const profile = useProfile();
  const [status, setStatus] = useState<SetupStatus | null>(null);
  const offered = useSetupOfferStore((s) => s.offered);
  const setOfferUid = useSetupOfferStore((s) => s.setUid);
  const close = useSetupOfferStore((s) => s.close);
  const pilot = useFreshPilot();
  const telemetryArmed = useFlightStore(liveArmed);
  const { widthClass } = useWidthClass();
  const scale = gridMeasures(widthClass, false).scale;
  // The offer is pointless over the setup screen it opens.
  const onSetup = useNavStore((s) => activeScreenId(s) === "aircraft-setup");

  useEffect(() => {
    if (profile !== "drone") return;
    const ctl = new AbortController();
    const read = () =>
      fetchSetupStatus(ctl.signal).then(setStatus, () => {
        // no setup engine on this node, or it did not answer: no offer
      });
    void read();
    const id = window.setInterval(() => void read(), SETUP_POLL_MS);
    return () => {
      ctl.abort();
      window.clearInterval(id);
    };
  }, [profile]);

  const uid = setupOfferUid(status, offered, tourAirborne(pilot, telemetryArmed));
  // The Menu offers the same setup while this offer stands, and only while
  // the banner is mounted to keep it current (the Feed has no banner).
  useEffect(() => {
    setOfferUid(uid);
    return () => setOfferUid(null);
  }, [uid, setOfferUid]);
  if (!uid || onSetup) return null;
  const button = "touch-target shrink-0 rounded-[var(--fs-radius-sm)] border font-semibold";
  return (
    <div
      role="status"
      data-testid="setup-offer"
      className="pointer-events-auto flex items-center bg-primary/15 text-foreground"
      style={{ gap: 8 * scale, padding: `${5 * scale}px ${12 * scale}px` }}
    >
      <Wrench className="shrink-0 text-hud-primary" style={{ width: 16 * scale, height: 16 * scale }} aria-hidden />
      <span className="min-w-0 flex-1 font-medium" style={bannerTextStyle(scale)}>
        {specText("setup.banner.title")}
      </span>
      <button
        type="button"
        onClick={() => close(true)}
        className={`${button} border-primary bg-primary/25`}
        style={{ padding: `0 ${10 * scale}px`, fontSize: 13 * scale }}
      >
        {specText("setup.banner.open")}
      </button>
      <button
        type="button"
        onClick={() => close(false)}
        className={`${button} border-border`}
        style={{ padding: `0 ${10 * scale}px`, fontSize: 13 * scale }}
      >
        {specText("setup.banner.later")}
      </button>
    </div>
  );
}
