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
import { chipButton, chipFullText, chipLines } from "@/lib/chip-view";
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
  const lineRef = useRef<HTMLSpanElement>(null);
  const [fullAt, setFullAt] = useState<DOMRect | null>(null);
  const maxWidth = `${CHIP_MAX_FRAC * 100}cqw`;

  if (!chip) {
    return (
      <span
        className="chrome-chip-text chrome-dim min-w-0 shrink truncate whitespace-nowrap rounded-[var(--fs-radius-sm)] border border-[var(--fs-border)] px-[10px] py-[6px]"
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
  const truncated = () =>
    [textRef.current, lineRef.current].some((el) => el !== null && el.scrollWidth > el.clientWidth + 1);

  return (
    <span
      className={cn("flex min-w-0 items-center gap-[6px]", fit ? "min-w-[96px] shrink" : "shrink", chip.dim && "opacity-55")}
      style={{ maxWidth }}
      data-testid="flight-state-chip-box"
    >
      <button
        type="button"
        onClick={(e) => (truncated() ? setFullAt(e.currentTarget.getBoundingClientRect()) : openSheet("mode"))}
        aria-label={specText("chip.aria", { text: chip.text })}
        title={full}
        data-testid="flight-state-chip"
        className={cn(HIT_AREA, "flex h-[40px] min-w-0 shrink flex-col items-start justify-center rounded-[var(--fs-radius-sm)] border px-[10px] text-left transition-transform duration-150 ease-out active:scale-[0.97]")}
        style={{ borderColor: `${chip.color}B3`, background: `${chip.color}2E` }}
      >
        <span ref={textRef} className="chrome-chip-text max-w-full truncate whitespace-nowrap leading-[16px] tracking-[0.02em]" style={{ color: chip.color }}>
          {chip.text}
        </span>
        {!compact && lines.length > 0 ? (
          <span ref={lineRef} className="max-w-full truncate whitespace-nowrap text-[11px] leading-[13px]" data-testid="flight-state-chip-lines">
            {lines.map((l, i) => (
              <span key={i} style={{ color: l.color }}>
                {i > 0 ? " · " : ""}
                {l.text}
              </span>
            ))}
          </span>
        ) : null}
      </button>
      {button ? (
        <button
          type="button"
          onClick={button.run}
          data-testid="flight-state-chip-button"
          data-button={chip.button}
          className={cn(HIT_AREA, "h-[40px] shrink-0 whitespace-nowrap rounded-[var(--fs-radius-sm)] border border-[var(--fs-border)] bg-white/10 px-[10px] font-semibold hover:bg-white/15")}
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
