// The data layers' status chips, stacked in the Feed's alert column under the
// banners and alert cards instead of on the layer canvas, where a banner
// would cover them.

import { chipTexts, UNAVAILABLE_CHIP } from "@/shared/hud/layer-engine";
import { useLayerChipsStore } from "@/stores/layer-chips-store";

export function LayerChips() {
  const mask = useLayerChipsStore((s) => s.mask);
  if (mask === 0) return null;
  return (
    <div role="status" aria-live="polite" className="flex flex-col items-center gap-[4px]" data-testid="layer-chips">
      {chipTexts(mask).map((text) => (
        <div
          key={text}
          className="rounded-[6px] bg-hud-glass-strong px-[0.6rem] py-[0.15rem] text-[0.8rem] font-semibold backdrop-blur-hud"
          style={{ color: UNAVAILABLE_CHIP }}
          data-testid="layer-chip"
        >
          {text}
        </div>
      ))}
    </div>
  );
}
