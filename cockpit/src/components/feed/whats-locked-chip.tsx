// The what's-locked chip (13-hud-v2 §8.4, L3): who is designated, the one
// state word that matters now and the live confidence, with an × that
// releases the target (and stops a running follow). Shown only while a target
// is designated; the words come from `lib/whats-locked`.

import { Crosshair, X } from "lucide-react";

import { useNow } from "@/hooks/use-now";
import { useSkillContext } from "@/hooks/use-skill-context";
import { releaseTarget } from "@/lib/target-actions";
import { isDesignated, isVideoLost } from "@/lib/target-overlay";
import { cn, isDemoMode } from "@/lib/utils";
import { whatsLocked, type ChipTone } from "@/lib/whats-locked";
import { DETECTION_STALE_MS, useDetectionsStore } from "@/stores/detections-store";
import { useFeedStore } from "@/stores/feed-store";
import { useNavStreamStore } from "@/stores/nav-stream-store";

const TONE: Record<ChipTone, string> = {
  track: "text-hud-track",
  coast: "text-hud-coast",
  warn: "text-hud-warn",
  crit: "text-hud-crit",
  muted: "text-hud-muted",
};

export function WhatsLockedChip({ multiStream, mainCameraId }: { multiStream: boolean; mainCameraId: string | null }) {
  const designated = useNavStreamStore((s) => s.designated);
  const follow = useNavStreamStore((s) => s.follow);
  const event = useNavStreamStore((s) => s.event);
  const latest = useDetectionsStore((s) => s.latest);
  const byCamera = useDetectionsStore((s) => s.byCamera);
  const videoState = useFeedStore((s) => s.video.state);
  const ctx = useSkillContext();
  const now = useNow(designated ? 250 : null);
  if (!designated) return null;

  const batch = multiStream ? (byCamera[designated.cameraId] ?? null) : latest;
  const fresh = batch && now - batch.receivedAt <= DETECTION_STALE_MS ? batch : null;
  const camera = multiStream ? designated.cameraId : (fresh?.cameraId ?? designated.cameraId);
  const detection = fresh?.detections.find((d) => isDesignated(designated, camera, d)) ?? null;
  const onMain = !multiStream || designated.cameraId === mainCameraId;
  const staleReason = onMain && isVideoLost(videoState, isDemoMode()) ? "No video" : fresh ? null : "Waiting for feed";

  const chip = whatsLocked({
    designated,
    detection,
    staleReason,
    follow: follow?.data ?? null,
    followAgeMs: follow ? now - follow.at : Infinity,
    event: event?.data ?? null,
    eventAgeMs: event ? now - event.at : Infinity,
  });

  return (
    <div
      role="status"
      aria-label={`Target ${chip.who}: ${chip.word}`}
      className={cn(
        "pointer-events-auto flex items-center gap-[8px] rounded-lg border bg-hud-glass-strong py-[3px] pl-[8px] pr-[3px] font-mono text-[13px] backdrop-blur-hud",
        chip.tone === "track" ? "border-hud-track" : "border-hud-hair",
      )}
    >
      <Crosshair className={cn("h-[16px] w-[16px] shrink-0", TONE[chip.tone])} aria-hidden />
      <div className="flex min-w-0 flex-col leading-tight">
        <span className="truncate text-hud-ink">{chip.who}</span>
        <span className={cn("truncate font-semibold", TONE[chip.tone])}>{chip.word}</span>
      </div>
      <span className="text-hud-ink-2">{chip.confidence}</span>
      <button
        type="button"
        onClick={() => void releaseTarget(ctx)}
        aria-label="Release target"
        className="flex h-[32px] w-[32px] items-center justify-center rounded-md text-hud-ink hover:bg-hud-hair"
      >
        <X className="h-[16px] w-[16px]" aria-hidden />
      </button>
    </div>
  );
}
