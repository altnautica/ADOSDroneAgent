// The minimap's footer readout as one row: the scale bottom-left and a
// warning (such as "No fix") bottom-right, sized in chrome px times the
// class scale. In the bottom-left card it sits above the card's dot strip.

import { CHROME_TOKENS } from "@/lib/chrome-tokens";

export interface MiniMapReadout {
  scale: string;
  warn: string | null;
}

/** Clearance above the bottom-left card's foot, where its dots sit. */
const CARD_FOOT_CLEAR_PX = 14;

export function MiniMapReadoutRow({ readout, scale, inCard }: { readout: MiniMapReadout | null; scale: number; inCard: boolean }) {
  if (!readout) return null;
  return (
    <div
      className="pointer-events-none absolute left-0 right-0 flex items-center justify-between gap-[4px] px-[6px] font-mono text-muted-foreground"
      style={{ fontSize: CHROME_TOKENS.type.label_px * scale, bottom: inCard ? CARD_FOOT_CLEAR_PX : 2 }}
      data-testid="minimap-readout"
    >
      <span>{readout.scale}</span>
      {readout.warn ? <span className="text-warn">{readout.warn}</span> : null}
    </div>
  );
}
