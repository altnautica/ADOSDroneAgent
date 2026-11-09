// The following-mode chrome (13-hud-v2 §8, §11), docked in the follow strip
// under the top bar, between the columns, so it never covers the centre or a
// column: the what's-locked chip, the range line (tap for the full range
// panel, which drops from the strip) and the follow controls. Following mode
// holds while follow-me reports a non-idle phase, or while a target is
// designated with the follow skill installed; outside it only the chip shows,
// and only while a target is designated. The re-acquired toast fires from the
// nav stream's `follow.reacquired` events whatever the mode.

import { useEffect, useRef, useState } from "react";
import { ChevronDown } from "lucide-react";

import { FollowRail } from "@/components/feed/follow-rail";
import { FollowRangePanel } from "@/components/feed/follow-range-panel";
import { WhatsLockedChip } from "@/components/feed/whats-locked-chip";
import { useFollowSkill } from "@/hooks/use-follow-skill";
import { useNow } from "@/hooks/use-now";
import type { WidthClass } from "@/lib/cockpit-layout";
import { cameraLabel } from "@/lib/feed-legs";
import { compactFollowLine, followPanelModel } from "@/lib/follow-panel";
import { reacquiredToastText } from "@/lib/follow-rail";
import type { RosterCamera } from "@/lib/types";
import { cn } from "@/lib/utils";
import { toast } from "@/shared/api/toast";
import { isFollowingMode } from "@/shared/hud/follow-chrome";
import { useFeedStore } from "@/stores/feed-store";
import { useNavStreamStore } from "@/stores/nav-stream-store";

/** Toast each `follow.reacquired` once, as it arrives. */
function useReacquiredToast(): void {
  const event = useNavStreamStore((s) => s.event);
  const shown = useRef<number | null>(null);
  useEffect(() => {
    if (!event || shown.current === event.at) return;
    shown.current = event.at;
    const text = reacquiredToastText(event.data);
    if (text) toast.info(text);
  }, [event]);
}

export interface FollowStripState {
  designated: boolean;
  following: boolean;
}

/** Whether the follow strip is up, and in following mode. */
export function useFollowStripState(): FollowStripState {
  useReacquiredToast();
  const follow = useNavStreamStore((s) => s.follow);
  const designated = useNavStreamStore((s) => s.designated) !== null;
  const followSkill = useFollowSkill();
  const now = useNow(follow ? 1000 : null);
  const following = isFollowingMode({
    phase: follow?.data.phase ?? null,
    ageMs: follow ? now - follow.at : Infinity,
    installed: followSkill !== null,
    designated,
  });
  return { designated, following };
}

export function FollowStrip({
  widthClass,
  cameras,
  multiStream,
  following,
  designated,
  bandHeightPx,
}: {
  widthClass: WidthClass;
  cameras: readonly RosterCamera[];
  multiStream: boolean;
  following: boolean;
  designated: boolean;
  /** The height between the top bar and the bottom zones; the rail folds its
   *  extras on a short compact panel. */
  bandHeightPx: number;
}) {
  const follow = useNavStreamStore((s) => s.follow);
  const mainCameraId = useFeedStore((s) => s.mainCameraId);
  const pipCameraId = useFeedStore((s) => s.pipCameraId);
  const followSkill = useFollowSkill();
  const now = useNow(follow ? 1000 : null);
  const [panelOpen, setPanelOpen] = useState(false);

  const followAgeMs = follow ? now - follow.at : Infinity;
  const main = cameras.find((c) => c.id === mainCameraId) ?? cameras[0] ?? null;
  const pip = pipCameraId ? (cameras.find((c) => c.id === pipCameraId) ?? null) : null;
  const swapLabel = main && pip ? `${cameraLabel(main).toUpperCase()} ⇄ ${cameraLabel(pip).toUpperCase()}` : null;
  const m = followPanelModel(follow?.data ?? null, followAgeMs);

  return (
    <div className="pointer-events-none relative flex h-full w-full items-center justify-center gap-[8px]" data-zone="follow_strip">
      {designated ? (
        <div className="pointer-events-auto min-w-0 shrink">
          <WhatsLockedChip multiStream={multiStream} mainCameraId={main?.id ?? null} />
        </div>
      ) : null}
      {following ? (
        <div className="chrome-panel chrome-bar-text pointer-events-auto flex h-full min-w-0 items-center gap-[8px] px-[6px]">
          <button
            type="button"
            onClick={() => setPanelOpen((o) => !o)}
            aria-expanded={panelOpen}
            aria-label="Follow range"
            data-zone="follow_range"
            className={cn("flex h-[44px] min-w-0 shrink items-center gap-[6px] rounded-[var(--fs-radius-sm)] px-[8px] font-mono hover:bg-white/10", m.stale && "opacity-50")}
          >
            <span className="truncate">{compactFollowLine(m)}</span>
            <ChevronDown className={cn("h-[14px] w-[14px] shrink-0 transition-transform duration-150", panelOpen && "rotate-180")} aria-hidden />
          </button>
          <FollowRail
            widthClass={widthClass}
            safeHeightPx={bandHeightPx}
            follow={followSkill}
            phase={follow?.data.phase ?? null}
            designated={designated}
            swapLabel={swapLabel}
          />
        </div>
      ) : null}
      {following && panelOpen ? (
        <div className="pointer-events-auto absolute left-1/2 top-[calc(100%+8px)] -translate-x-1/2">
          <FollowRangePanel follow={follow?.data ?? null} ageMs={followAgeMs} widthClass={widthClass === "compact" ? "medium" : widthClass} />
        </div>
      ) : null}
    </div>
  );
}
