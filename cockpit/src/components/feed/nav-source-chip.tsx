// The nav-source chip (13-hud-v2 §8.3), safety band item 6 in place of the
// GPS chip: what the EKF navigates on (DR, EXT NAV, GPS, FLOW) with a quality
// word, coloured by quality and never by colour alone. Inputs: the FC's
// EKF_STATUS_REPORT flags and GPS fix from the telemetry poll, the EKF source
// sets read once per FC link, and vision-nav's `nav.state` from the nav
// stream. A tap opens the source-set popover (its own lazy chunk).

import { lazy, Suspense, useEffect, useState } from "react";
import { Compass, Satellite, ScanLine, Waves } from "lucide-react";
import type { LucideIcon } from "lucide-react";

import { useNow } from "@/hooks/use-now";
import { useWidthClass } from "@/hooks/use-width-class";
import { loadEkfSourceParams, navSourceInput } from "@/lib/ekf-sources";
import { cn } from "@/lib/utils";
import { navSourceTextEn, resolveNavSource, type NavSourceKind, type NavTone } from "@/shared/hud/nav-source";
import { useFlightStore } from "@/stores/flight-store";
import { useNavStreamStore } from "@/stores/nav-stream-store";
import { useStatusStore } from "@/stores/status-store";

const NavSourcePopover = lazy(() => import("@/components/feed/nav-source-popover"));

/** vision-nav's report counts while younger than this (§3). */
export const NAV_STATE_FRESH_MS = 2000;

const ICON: Record<NavSourceKind, LucideIcon> = {
  dr: Compass,
  ext: ScanLine,
  gps: Satellite,
  flow: Waves,
  stale: Satellite,
};

const TONE: Record<NavTone, string> = {
  good: "text-hud-good",
  warn: "text-hud-warn",
  crit: "text-hud-crit",
  muted: "text-hud-muted",
};

export function NavSourceChip() {
  const { widthClass } = useWidthClass();
  const telemetry = useFlightStore((s) => s.telemetry);
  const live = useFlightStore((s) => s.live);
  const fcConnected = useStatusStore((s) => s.status?.fc_connected === true);
  const nav = useNavStreamStore((s) => s.nav);
  const lastCommandedSet = useNavStreamStore((s) => s.lastCommandedSet);
  const params = useNavStreamStore((s) => s.ekfParams);
  const now = useNow(nav ? 1000 : null);
  const [open, setOpen] = useState(false);

  // The source sets, once per FC link.
  useEffect(() => {
    if (!fcConnected) return;
    const ac = new AbortController();
    void loadEkfSourceParams(ac.signal).catch(() => undefined);
    return () => ac.abort();
  }, [fcConnected]);

  const freshNav = nav && now - nav.at <= NAV_STATE_FRESH_MS ? nav.data : null;
  const result = resolveNavSource(navSourceInput(telemetry, live, freshNav, lastCommandedSet, params));
  const text = navSourceTextEn(result);
  const Icon = ICON[result.kind];
  const showWord = widthClass !== "compact" && text.word !== null;

  return (
    <>
      <button
        type="button"
        onClick={() => setOpen(true)}
        aria-haspopup="dialog"
        aria-label={`Navigation source: ${text.label}${text.word ? `, ${text.word}` : ""}`}
        className="flex min-h-[40px] shrink-0 items-center gap-[0.25rem] whitespace-nowrap rounded-md px-[0.3rem] font-mono text-[0.75rem] hover:bg-hud-hair"
      >
        <Icon className={cn("h-[0.85rem] w-[0.85rem]", TONE[result.tone])} aria-hidden />
        <span className={TONE[result.tone]}>{text.label}</span>
        {showWord ? <span className={cn("font-semibold", TONE[result.tone])}>{text.word}</span> : null}
      </button>
      {open ? (
        <Suspense fallback={null}>
          <NavSourcePopover result={result} nav={freshNav} params={params} onClose={() => setOpen(false)} />
        </Suspense>
      ) : null}
    </>
  );
}
