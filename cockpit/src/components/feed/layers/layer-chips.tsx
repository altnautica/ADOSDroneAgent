// The data layers' status badges ("Depth unavailable", "AR unavailable"),
// stacked in the Feed's alert column under the banners and alert cards
// instead of on the layer canvas, where a banner would cover them.
//
// Each badge is a button. It opens a small menu that says why the badge shows
// (the vision engine is off, the video has failed, or the layer sent nothing
// for 2 s) and offers two fixes: Turn off switches the layer off (or, when the
// open view forces it on, closes that view instead), and Set up opens
// Settings › Vision on a node that has that section. The menu reads in the
// alert sheet's bounded type scale, so it stays chrome-sized on a large panel.

import { useRef, useState } from "react";
import { useTranslation } from "react-i18next";

import { Overlay } from "@/components/ui/overlay";
import type { CockpitView } from "@/lib/cockpit-view";
import { MENU_MAX_WIDTH_PX, MENU_MIN_WIDTH_PX, placeMenu, type AnchorRect } from "@/lib/menu-placement";
import { useChromeScale } from "@/lib/ui-scale";
import { cn } from "@/lib/utils";
import { ALERT_TEXT } from "@/shared/hud/alert-centre";
import { layerChips, UNAVAILABLE_CHIP, VISION_CHIP_MASK, type LayerChip } from "@/shared/hud/layer-engine";
import { useNodeIdentity } from "@/shared/use-profile";
import { useCockpitViewStore } from "@/stores/cockpit-view-store";
import { useFeedStore } from "@/stores/feed-store";
import { useLayerChipsStore, visionEngineUp } from "@/stores/layer-chips-store";
import { useNavStore } from "@/stores/nav-store";

import { badgeTurnOff, turnOffBadge, VISION_SETTINGS_SCREEN } from "./layer-badge-actions";
import { useVisionEngine } from "./use-vision-engine";

export interface BadgeMenuProps {
  chip: LayerChip;
  view: CockpitView;
  /** Settings › Vision exists on this node (an aircraft profile). */
  canSetUp: boolean;
  /** The Feed's video has failed: a layer drawn from it gets no frames. */
  videoFailed: boolean;
  onClose: () => void;
}

const MENU_ITEM = cn(
  "flex min-h-[48px] w-full items-center rounded-md px-[12px] text-left text-surface-foreground hover:bg-muted active:bg-muted",
  ALERT_TEXT.body,
);

/** The badge menu's content: the reason line, then Turn off (or Close Vision
 *  view) and Set up. */
export function BadgeMenuBody({ chip, view, canSetUp, videoFailed, onClose }: BadgeMenuProps) {
  const { t } = useTranslation();
  const layer = t(`vision.layerName.${chip.layer}` as const);
  const reason =
    chip.reason === "engine_off"
      ? t("vision.badge.engineOff")
      : chip.reason === "degraded"
        ? chip.layer === "ar"
          ? t("vision.badge.approx")
          : t("vision.badge.unscaled")
        : videoFailed
          ? t("vision.badge.noDataVideoFailed", { layer })
          : t("vision.badge.noData", { layer });
  const off = badgeTurnOff(chip, view);
  const run = (fn: () => void) => () => {
    onClose();
    fn();
  };
  return (
    <>
      <p className={cn("px-[12px] py-[6px] text-muted-foreground", ALERT_TEXT.small)} data-testid="layer-chip-reason">
        {reason}
      </p>
      <button type="button" className={MENU_ITEM} onClick={run(() => turnOffBadge(chip, view))} data-testid="layer-chip-turn-off">
        {off === "close_view" ? t("vision.badge.closeView") : t("vision.badge.turnOff")}
      </button>
      {canSetUp ? (
        <button
          type="button"
          className={MENU_ITEM}
          onClick={run(() => useNavStore.getState().openDetail(VISION_SETTINGS_SCREEN))}
          data-testid="layer-chip-set-up"
        >
          {t("vision.badge.setUp")}
        </button>
      ) : null}
    </>
  );
}

function BadgeMenu({ anchor, ...props }: BadgeMenuProps & { anchor: AnchorRect | null }) {
  // Drawn at the chrome scale like the bar's popovers (a CSS zoom), so it is
  // placed in the zoomed space: the anchor and the window divided by it.
  const s = useChromeScale();
  const place = placeMenu(anchor && { left: anchor.left / s, top: anchor.top / s, bottom: anchor.bottom / s }, window.innerWidth / s, window.innerHeight / s);
  return (
    <Overlay
      onDismiss={props.onClose}
      scrim="clear"
      panelClassName="absolute overflow-y-auto rounded-lg border border-border bg-surface p-[4px] text-surface-foreground shadow-lg"
      panelStyle={{
        zoom: s === 1 ? undefined : s,
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
  const videoFailed = useFeedStore((s) => s.video.state === "failed");
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
      {open ? (
        <BadgeMenu chip={open.chip} anchor={open.anchor} view={view} canSetUp={canSetUp} videoFailed={videoFailed} onClose={() => setOpen(null)} />
      ) : null}
    </div>
  );
}
