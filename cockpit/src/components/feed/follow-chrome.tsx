// The following-mode chrome over the main video (13-hud-v2 §8, §11): the
// what's-locked chip top-centre (L3), the range panel bottom-centre above the
// Skill Bar (L2, read-only) and the right rail (L3). Following mode holds while
// follow-me reports a non-idle phase, or while a target is designated with the
// follow skill installed; outside it the panel and rail step aside. The chip
// shows whenever a target is designated. The re-acquired toast fires from the
// nav stream's `follow.reacquired` events whatever the mode.

import { useEffect, useRef } from "react";

import { FollowRail } from "@/components/feed/follow-rail";
import { FollowRangePanel } from "@/components/feed/follow-range-panel";
import { WhatsLockedChip } from "@/components/feed/whats-locked-chip";
import { useElementSize } from "@/hooks/use-element-size";
import { useFollowSkill } from "@/hooks/use-follow-skill";
import { useNow } from "@/hooks/use-now";
import { Z, type WidthClass } from "@/lib/cockpit-layout";
import { cameraLabel } from "@/lib/feed-legs";
import { railBottomPx, reacquiredToastText } from "@/lib/follow-rail";
import type { RosterCamera } from "@/lib/types";
import { cn } from "@/lib/utils";
import { toast } from "@/shared/api/toast";
import { isFollowingMode } from "@/shared/hud/follow-chrome";
import { useFeedStore } from "@/stores/feed-store";
import { useNavStreamStore } from "@/stores/nav-stream-store";

/** Range panel width per class (§11): content width at compact and medium. */
const PANEL_WIDTH: Partial<Record<WidthClass, number>> = { expanded: 320, tv: 400 };

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

export function FollowChrome({
  widthClass,
  cameras,
  multiStream,
  hasRadar,
  skillClearance,
}: {
  widthClass: WidthClass;
  cameras: readonly RosterCamera[];
  multiStream: boolean;
  hasRadar: boolean;
  /** CSS length the range panel's bottom edge keeps above (the Skill Bar top
   *  plus 12 px, or 12 px where the Skill Bar sits below the video). */
  skillClearance: string;
}) {
  useReacquiredToast();
  const ref = useRef<HTMLDivElement>(null);
  const size = useElementSize(ref);
  const follow = useNavStreamStore((s) => s.follow);
  const designated = useNavStreamStore((s) => s.designated);
  const mainCameraId = useFeedStore((s) => s.mainCameraId);
  const pipCameraId = useFeedStore((s) => s.pipCameraId);
  const followSkill = useFollowSkill();
  const now = useNow(follow ? 1000 : null);

  const followAgeMs = follow ? now - follow.at : Infinity;
  const following = isFollowingMode({
    phase: follow?.data.phase ?? null,
    ageMs: followAgeMs,
    installed: followSkill !== null,
    designated: designated !== null,
  });

  const main = cameras.find((c) => c.id === mainCameraId) ?? cameras[0] ?? null;
  const pip = pipCameraId ? (cameras.find((c) => c.id === pipCameraId) ?? null) : null;
  const swapLabel = main && pip ? `${cameraLabel(main).toUpperCase()} ⇄ ${cameraLabel(pip).toUpperCase()}` : null;

  return (
    <div ref={ref} className="pointer-events-none absolute inset-0">
      {designated ? (
        <div className={cn("absolute inset-x-0 top-0 flex justify-center", Z.zones)}>
          <WhatsLockedChip multiStream={multiStream} mainCameraId={main?.id ?? null} />
        </div>
      ) : null}
      {following ? (
        <>
          <div className={cn("absolute inset-x-0 flex justify-center", Z.hud)} style={{ bottom: skillClearance }}>
            <div style={{ width: PANEL_WIDTH[widthClass] }}>
              <FollowRangePanel follow={follow?.data ?? null} ageMs={followAgeMs} widthClass={widthClass} />
            </div>
          </div>
          <div
            className={cn("absolute right-0 top-0 flex items-center", Z.zones)}
            style={{ bottom: railBottomPx(widthClass, hasRadar) }}
          >
            <FollowRail
              widthClass={widthClass}
              safeHeightPx={size.h}
              follow={followSkill}
              phase={follow?.data.phase ?? null}
              designated={designated !== null}
              swapLabel={swapLabel}
            />
          </div>
        </>
      ) : null}
    </div>
  );
}
