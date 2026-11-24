// The data layers' status badges ("Depth unavailable", "AR unavailable"),
// stacked in the Feed's alert column under the banners and alert cards
// instead of on the layer canvas, where a banner would cover them.
//
// Each badge is a button. It opens a small menu that says why the badge shows
// (the vision engine is off, or the layer sent nothing for 2 s) and offers two
// fixes: Turn off switches the layer off (or, when the open view forces it on,
// closes that view instead), and Set up opens Settings › Vision on a node that
// has that section.

import { useRef, useState } from "react";
import { useTranslation } from "react-i18next";

import { ListItem } from "@/components/ui/list-item";
import { Overlay } from "@/components/ui/overlay";
import type { CockpitView } from "@/lib/cockpit-view";
import { MENU_MAX_WIDTH_PX, MENU_MIN_WIDTH_PX, placeMenu, type AnchorRect } from "@/lib/menu-placement";
import { layerChips, UNAVAILABLE_CHIP, VISION_CHIP_MASK, type LayerChip } from "@/shared/hud/layer-engine";
import { useNodeIdentity } from "@/shared/use-profile";
import { useCockpitViewStore } from "@/stores/cockpit-view-store";
import { useLayerChipsStore, visionEngineUp } from "@/stores/layer-chips-store";
import { useNavStore } from "@/stores/nav-store";

import { badgeTurnOff, turnOffBadge, VISION_SETTINGS_SCREEN } from "./layer-badge-actions";
import { useVisionEngine } from "./use-vision-engine";

export interface BadgeMenuProps {
  chip: LayerChip;
  view: CockpitView;
  /** Settings › Vision exists on this node (an aircraft profile). */
  canSetUp: boolean;
  onClose: () => void;
}

/** The badge menu's content: the reason line, then Turn off (or Close Vision
 *  view) and Set up. */
export function BadgeMenuBody({ chip, view, canSetUp, onClose }: BadgeMenuProps) {
  const { t } = useTranslation();
  const layer = t(`vision.layerName.${chip.layer}` as const);
  const reason =
    chip.reason === "engine_off"
      ? t("vision.badge.engineOff")
      : chip.reason === "degraded"
        ? chip.layer === "ar"
          ? t("vision.badge.approx")
          : t("vision.badge.unscaled")
        : t("vision.badge.noData", { layer });
  const off = badgeTurnOff(chip, view);
  const run = (fn: () => void) => () => {
    onClose();
    fn();
  };
  return (
    <>
      <p className="px-[0.75rem] py-[0.4rem] text-[0.8rem] text-muted-foreground" data-testid="layer-chip-reason">
        {reason}
      </p>
      <ListItem
        headline={off === "close_view" ? t("vision.badge.closeView") : t("vision.badge.turnOff")}
        onClick={run(() => turnOffBadge(chip, view))}
      />
      {canSetUp ? (
        <ListItem headline={t("vision.badge.setUp")} onClick={run(() => useNavStore.getState().openDetail(VISION_SETTINGS_SCREEN))} />
      ) : null}
    </>
  );
}

function BadgeMenu({ anchor, ...props }: BadgeMenuProps & { anchor: AnchorRect | null }) {
  const place = placeMenu(anchor, window.innerWidth, window.innerHeight);
  return (
    <Overlay
      onDismiss={props.onClose}
      scrim="clear"
      panelClassName="absolute overflow-y-auto rounded-lg border border-border bg-surface py-[0.25rem] text-surface-foreground shadow-lg"
      panelStyle={{
        left: place.left,
        top: place.top,
        bottom: place.bottom,
        maxHeight: place.maxHeight,
        minWidth: MENU_MIN_WIDTH_PX,
        maxWidth: MENU_MAX_WIDTH_PX,
      }}
    >
      <BadgeMenuBody {...props} />
    </Overlay>
  );
}

export function LayerChips() {
  const { t } = useTranslation();
  const mask = useLayerChipsStore((s) => s.mask);
  const engineUp = useLayerChipsStore(visionEngineUp);
  const view = useCockpitViewStore((s) => s.view);
  const { profile } = useNodeIdentity();
  const [open, setOpen] = useState<{ chip: LayerChip; anchor: AnchorRect } | null>(null);
  const refs = useRef(new Map<string, HTMLButtonElement>());
  useVisionEngine((mask & VISION_CHIP_MASK) !== 0);
  if (mask === 0) return null;
  const chips = layerChips(mask, engineUp);
  // The Vision settings section exists on an aircraft only.
  const canSetUp = profile === "drone" || profile === "visor";
  return (
    <div role="status" aria-live="polite" className="flex flex-col items-center gap-[4px]" data-testid="layer-chips">
      {chips.map((chip) => (
        <button
          key={chip.id}
          ref={(el) => {
            if (el) refs.current.set(chip.id, el);
            else refs.current.delete(chip.id);
          }}
          type="button"
          aria-haspopup="menu"
          aria-label={t("vision.badge.options", { layer: chip.text })}
          onClick={() => {
            const r = refs.current.get(chip.id)?.getBoundingClientRect();
            setOpen({ chip, anchor: r ? { left: r.left, top: r.top, bottom: r.bottom } : { left: 8, top: 8, bottom: 8 } });
          }}
          className="pointer-events-auto min-h-[48px] rounded-[6px] bg-hud-glass-strong px-[0.8rem] text-[0.8rem] font-semibold backdrop-blur-hud"
          style={{ color: UNAVAILABLE_CHIP }}
          data-testid="layer-chip"
          data-reason={chip.reason}
        >
          {chip.text}
        </button>
      ))}
      {open ? <BadgeMenu chip={open.chip} anchor={open.anchor} view={view} canSetUp={canSetUp} onClose={() => setOpen(null)} /> : null}
    </div>
  );
}
