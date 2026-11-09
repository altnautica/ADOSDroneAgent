// Whether the follow strip under the top bar is up (a target designated) and
// in following mode (follow-me reports a non-idle phase, or a target is
// designated with the follow skill installed). Also toasts each
// `follow.reacquired` once, as it arrives, whatever the mode.

import { useEffect, useRef } from "react";

import { useFollowSkill } from "@/hooks/use-follow-skill";
import { useNow } from "@/hooks/use-now";
import { reacquiredToastText } from "@/lib/follow-rail";
import { toast } from "@/shared/api/toast";
import { isFollowingMode } from "@/shared/hud/follow-chrome";
import { useNavStreamStore } from "@/stores/nav-stream-store";

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
