// The Feed's utility controls — large (>=64px) touch buttons the pilot uses
// without leaving the flying view: Back and Menu drive the navigator; Record
// toggles the ground-station recorder; Stream re-establishes the video;
// Settings and Pair jump to those screens. A PIC chip names the
// pilot-in-command only when the node actually reports one. The buttons are
// also reachable by the physical panel buttons through the shared focus ring.

import { useState } from "react";
import {
  ArrowLeft,
  Circle,
  Link2,
  Menu as MenuIcon,
  RefreshCw,
  Settings as SettingsIcon,
  ShieldCheck,
  Square,
} from "lucide-react";
import type { LucideIcon } from "lucide-react";

import { startRecording, stopRecording } from "@/lib/api";
import { cn } from "@/lib/utils";
import { useProfile } from "@/shared/use-profile";
import { useFeedStore } from "@/stores/feed-store";
import { useNavStore } from "@/stores/nav-store";
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
        "flex min-h-[max(4rem,64px)] min-w-[max(4rem,64px)] flex-col items-center justify-center gap-[0.2rem] rounded-lg border px-[0.4rem] backdrop-blur-hud transition-colors duration-quick disabled:opacity-50",
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
  const command = useNavStore((s) => s.command);
  const goTab = useNavStore((s) => s.goTab);
  const reconnectStream = useFeedStore((s) => s.reconnectStream);
  const recording = useStatusStore(
    (s) => s.status?.recording === true || s.status?.video?.recording === true,
  );
  const pic = useStatusStore((s) => s.status?.gcs?.pic_id ?? null);
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
      <ActionButton icon={ArrowLeft} label="Back" onClick={() => command("back")} />
      <ActionButton icon={MenuIcon} label="Menu" onClick={() => command("quick-menu")} />
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
      <ActionButton icon={SettingsIcon} label="Settings" onClick={() => goTab("settings")} />
      <ActionButton icon={Link2} label="Pair" onClick={() => goTab("pair")} />
      {pic ? (
        <div className="flex items-center gap-[0.3rem] self-center rounded-md border border-hud-hair bg-hud-glass px-[0.5rem] py-[0.3rem] backdrop-blur-hud">
          <ShieldCheck className="h-[0.9rem] w-[0.9rem] text-primary" aria-hidden />
          <span className="text-[0.75rem] uppercase tracking-wide text-hud-ink-2">PIC</span>
          <span className="font-mono text-[0.75rem] text-hud-ink">{pic}</span>
        </div>
      ) : null}
    </div>
  );
}
