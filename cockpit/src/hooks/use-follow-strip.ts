// Whether the follow strip under the top bar is up (a target designated) and
// in following mode (follow-me reports a non-idle phase, or a target is
// designated with the follow skill installed). Also toasts each
// `follow.reacquired` once, as it arrives, whatever the mode, naming what hid
// the subject when the search reported it.

import { useEffect, useRef, type RefObject } from "react";

import { useFollowSkill } from "@/hooks/use-follow-skill";
import { useNow } from "@/hooks/use-now";
import { reacquiredToastText } from "@/lib/follow-rail";
import { toast } from "@/shared/api/toast";
import { isFollowingMode } from "@/shared/hud/follow-chrome";
import { isRecoveryPhase, type OccluderKind } from "@/shared/hud/nav-types";
import { useNavStreamStore } from "@/stores/nav-stream-store";

/** The occluder the current recovery last reported: set while the subject is
 *  sought, reset when a new recovery starts or follow goes idle. */
function useRecoveryOccluder(): RefObject<OccluderKind | null> {
  const follow = useNavStreamStore((s) => s.follow);
  const kind = useRef<OccluderKind | null>(null);
  const recovering = useRef(false);
  useEffect(() => {
    if (!follow) return;
    const f = follow.data;
    const now = isRecoveryPhase(f.phase);
    if (now && !recovering.current) kind.current = null;
    if (now && f.occluder) kind.current = f.occluder.kind;
    if (f.phase === "idle") kind.current = null;
    recovering.current = now;
  }, [follow]);
  return kind;
}

function useReacquiredToast(): void {
  const event = useNavStreamStore((s) => s.event);
  const occluder = useRecoveryOccluder();
  const shown = useRef<number | null>(null);
  useEffect(() => {
    if (!event || shown.current === event.at) return;
    shown.current = event.at;
    const text = reacquiredToastText(event.data, occluder.current);
    if (text) toast.info(text);
  }, [event, occluder]);
}

export interface FollowStripState {
  designated: boolean;
  following: boolean;
}

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
