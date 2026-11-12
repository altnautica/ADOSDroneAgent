// The flight-state chip at the head of the top bar: who flies the aircraft
// now (cockpit spec `flight-state.json`), its colour, the secondary line, and
// the inline button a state offers (Take control, Resume, Abort, Set up
// aircraft). The chip never takes more than `top_bar.chip.max_frac` of the
// bar; past it the text ends in an ellipsis and the full text shows on hover
// (its tooltip) or on a tap, which opens it in a small panel whose press
// opens the Mode sheet. A tap on a chip that fits opens the Mode sheet.

import { useRef, useState } from "react";

import { Overlay } from "@/components/ui/overlay";
import { useFlightChip } from "@/hooks/use-pilot";
import { chipButton, chipFullText, chipLines, chipLineTextColor } from "@/lib/chip-view";
import { chromeStyle } from "@/lib/chrome-tokens";
import { CHIP_MAX_FRAC, HIT_AREA } from "@/lib/flight-layout";
import { cn } from "@/lib/utils";
import { specText } from "@/shared/hud/spec-text";
import { useCockpitUiStore } from "@/stores/cockpit-ui-store";
import { useNavStreamStore } from "@/stores/nav-stream-store";

/** `fit`: the bar has nothing left to move into the Menu, so the chip
 *  truncates its text rather than push the battery or STOP off the bar. */
export function FlightStateChip({ compact = false, fit = false }: { compact?: boolean; fit?: boolean }) {
  const chip = useFlightChip();
  const radioFallback = useNavStreamStore((s) => s.radioFallback);
  const openSheet = useCockpitUiStore((s) => s.openSheet);
  const textRef = useRef<HTMLSpanElement>(null);
  const [fullAt, setFullAt] = useState<DOMRect | null>(null);
  const maxWidth = `${CHIP_MAX_FRAC * 100}cqw`;

  if (!chip) {
    return (
      <span
        className="chrome-chip-text chrome-dim h-[var(--fs-control)] min-w-0 shrink truncate whitespace-nowrap rounded-[var(--fs-radius-sm)] border border-[var(--fs-border)] px-[10px] leading-[var(--fs-control)]"
        style={{ maxWidth }}
        data-testid="flight-state-chip"
      >
        {specText("chip.no_pilot")}
      </span>
    );
  }

  const button = chipButton(chip.button);
  const lines = chipLines(chip, radioFallback);
  const full = chipFullText(chip.text, lines);
  const truncated = () => textRef.current !== null && textRef.current.scrollWidth > textRef.current.clientWidth + 1;

  return (
    <span
      // Until the bar is full the chip keeps its width (up to its cap) and the
      // bar moves items into the Menu instead; only then does it truncate.
      className={cn("flex min-w-0 items-center gap-[6px]", fit ? "min-w-[96px] shrink" : "shrink-0", chip.dim && "opacity-55")}
      style={{ maxWidth }}
      data-testid="flight-state-chip-box"
    >
      <button
        type="button"
        onClick={(e) => (truncated() ? setFullAt(e.currentTarget.getBoundingClientRect()) : openSheet("mode"))}
        aria-label={specText("chip.aria", { text: chip.text })}
        title={full}
        data-testid="flight-state-chip"
        className={cn(HIT_AREA, "flex h-[var(--fs-control)] min-w-0 shrink items-center gap-[6px] rounded-[var(--fs-radius-sm)] border bg-transparent px-[10px] text-left transition-transform duration-150 ease-out active:scale-[0.97]")}
        // An outline in the state colour, a dot in it, and the words in the
        // panel's text colours: a state hue is too dark as small text.
        style={{ borderColor: `${chip.color}99` }}
      >
        <span aria-hidden data-testid="flight-state-chip-dot" className="h-[6px] w-[6px] shrink-0 rounded-full" style={{ background: chip.color }} />
        <span ref={textRef} className="min-w-0 truncate whitespace-nowrap tracking-[0.02em]" style={{ color: "var(--fs-text)" }}>
          <span className="chrome-chip-text">{chip.text}</span>
          {!compact && lines.length > 0 ? (
            <span className="font-normal" data-testid="flight-state-chip-lines">
              {lines.map((l, i) => (
                <span key={i} style={{ color: chipLineTextColor(l.color) }}>
                  {" · "}
                  {l.text}
                </span>
              ))}
            </span>
          ) : null}
        </span>
      </button>
      {button ? (
        <button
          type="button"
          onClick={button.run}
          data-testid="flight-state-chip-button"
          data-button={chip.button}
          className={cn(HIT_AREA, "h-[var(--fs-control)] shrink-0 whitespace-nowrap rounded-[var(--fs-radius-sm)] border border-[var(--fs-border)] bg-white/10 px-[10px] font-semibold hover:bg-white/15")}
        >
          {button.label}
        </button>
      ) : null}
      {fullAt ? (
        <Overlay
          onDismiss={() => setFullAt(null)}
          scrim="clear"
          panelClassName="chrome-panel chrome-bar-text w-[min(360px,90vw)]"
          panelStyle={{ ...chromeStyle(), position: "absolute", left: Math.max(8, Math.min(fullAt.left, window.innerWidth - 368)), top: fullAt.bottom + 8 }}
        >
          <button
            type="button"
            onClick={() => {
              setFullAt(null);
              openSheet("mode");
            }}
            aria-label={specText("chip.aria", { text: chip.text })}
            data-testid="flight-state-chip-full"
            className="min-h-[48px] w-full p-[10px] text-left"
          >
            {full}
          </button>
        </Overlay>
      ) : null}
    </span>
  );
}
