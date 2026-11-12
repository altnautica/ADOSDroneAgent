// The nav-source chip (13-hud-v2 §8.3), safety band item 6 in place of the
// GPS chip: what the EKF navigates on (DR, EXT NAV, GPS, FLOW) with a quality
// word, coloured by quality and never by colour alone. Inputs: the FC's
// EKF_STATUS_REPORT flags and GPS fix from the telemetry poll, the EKF source
// sets read once per FC link, and vision-nav's `nav.state` from the nav
// stream. A tap opens the source-set popover (its own lazy chunk).

import { lazy, Suspense, useState } from "react";
import { Compass, Satellite, ScanLine, Waves } from "lucide-react";
import type { LucideIcon } from "lucide-react";

import { useNavSource } from "@/hooks/use-nav-source";
import { useWidthClass } from "@/hooks/use-width-class";
import { CHROME_TOKENS } from "@/lib/chrome-tokens";
import { HIT_AREA } from "@/lib/flight-layout";
import { cn } from "@/lib/utils";
import { navSourceTextEn, type NavSourceKind, type NavTone } from "@/shared/hud/nav-source";
import { specText } from "@/shared/hud/spec-text";
import { severityTextColor } from "@/shared/hud/status-pill";

const NavSourcePopover = lazy(() => import("@/components/feed/nav-source-popover"));

const ICON: Record<NavSourceKind, LucideIcon> = {
  dr: Compass,
  ext: ScanLine,
  gps: Satellite,
  flow: Waves,
  stale: Satellite,
};

/** The icon in the tone's colour; the words in the panel's text colours (a
 *  severity hue is too dark as small text). */
const TONE: Record<NavTone, { icon: string; text: string }> = {
  good: { icon: CHROME_TOKENS.severity_ok, text: "var(--fs-text)" },
  warn: { icon: CHROME_TOKENS.severity_warning, text: severityTextColor("warning") },
  crit: { icon: CHROME_TOKENS.severity_critical, text: severityTextColor("critical") },
  muted: { icon: CHROME_TOKENS.text_dim, text: "var(--fs-text-dim)" },
};

export function NavSourceChip() {
  const { widthClass } = useWidthClass();
  const { result, freshNav, params } = useNavSource(false);
  const [open, setOpen] = useState(false);
  const text = navSourceTextEn(result);
  const Icon = ICON[result.kind];
  const tone = TONE[result.tone];
  const showWord = widthClass !== "compact" && text.word !== null;

  return (
    <>
      <button
        type="button"
        onClick={() => setOpen(true)}
        aria-haspopup="dialog"
        aria-label={`${specText("cockpit.status.source.nav_source")}: ${text.label}${text.word ? `, ${text.word}` : ""}`}
        className={cn(
          HIT_AREA,
          "flex h-[var(--fs-control)] shrink-0 items-center gap-[4px] whitespace-nowrap rounded-[var(--fs-radius-sm)] px-[4px] font-mono hover:bg-white/10",
        )}
        style={{ color: tone.text }}
      >
        <Icon className="chrome-icon" style={{ color: tone.icon }} aria-hidden />
        <span>{text.label}</span>
        {showWord ? <span className="font-semibold">{text.word}</span> : null}
      </button>
      {open ? (
        <Suspense fallback={null}>
          <NavSourcePopover result={result} nav={freshNav} params={params} onClose={() => setOpen(false)} />
        </Suspense>
      ) : null}
    </>
  );
}
