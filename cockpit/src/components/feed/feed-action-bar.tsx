// The Feed's own controls beside the Skill Bar: Stream re-dials the video and,
// on a ground station, Record toggles the recorder. Navigation (back, menu,
// tabs) lives in the menu rail and the utility bar, reachable by the panel
// buttons and gamepad; the pilot-in-command chip lives in the safety band.

import { useState } from "react";
import { Circle, RefreshCw, Square } from "lucide-react";
import type { LucideIcon } from "lucide-react";

import { startRecording, stopRecording } from "@/lib/api";
import { cn } from "@/lib/utils";
import { useProfile } from "@/shared/use-profile";
import { useFeedStore } from "@/stores/feed-store";
import { useStatusStore } from "@/stores/status-store";

function ActionButton({
  icon: Icon,
  label,
  onClick,
  active,
  disabled,
}: {
  icon: LucideIcon;
  label: string;
  onClick: () => void;
  active?: boolean;
  disabled?: boolean;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      disabled={disabled}
      aria-label={label}
      aria-pressed={active}
      className={cn(
        "flex min-h-[max(3.2rem,48px)] min-w-[max(3.6rem,48px)] flex-col items-center justify-center gap-[0.2rem] rounded-lg border px-[0.4rem] backdrop-blur-hud transition-colors duration-quick disabled:opacity-50",
        active
          ? "border-err bg-err/85 text-on-status"
          : "border-hud-hair bg-hud-glass text-hud-ink hover:bg-hud-glass-strong",
      )}
    >
      <Icon className="h-[1.5rem] w-[1.5rem]" aria-hidden />
      <span className="text-[0.75rem] font-medium leading-none">{label}</span>
    </button>
  );
}

export function FeedActionBar() {
  const reconnectStream = useFeedStore((s) => s.reconnectStream);
  const recording = useStatusStore(
    (s) => s.status?.recording === true || s.status?.video?.recording === true,
  );
  // The recorder is the ground station's (`/api/v1/ground-station/recording/*`);
  // a drone has no recording route, so the button is not offered there.
  const hasRecorder = useProfile() === "ground_station";
  const [recordBusy, setRecordBusy] = useState(false);

  const toggleRecord = async () => {
    if (recordBusy) return;
    setRecordBusy(true);
    try {
      await (recording ? stopRecording() : startRecording());
    } catch {
      // The next status poll keeps the button honest (the agent's real state).
    } finally {
      setRecordBusy(false);
    }
  };

  return (
    <div className="pointer-events-auto flex shrink-0 items-end gap-[0.4rem]">
      {hasRecorder ? (
        <ActionButton
          icon={recording ? Square : Circle}
          label={recording ? "Stop" : "Record"}
          onClick={toggleRecord}
          active={recording}
          disabled={recordBusy}
        />
      ) : null}
      <ActionButton icon={RefreshCw} label="Stream" onClick={() => reconnectStream()} />
    </div>
  );
}
