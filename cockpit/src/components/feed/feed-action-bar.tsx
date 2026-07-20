// The Feed's primary control surface — a row of large (>=64px) touch buttons the
// pilot uses without leaving the flying view, sitting just above the shell's
// thin utility bar. Each button fires a navigator action or an agent call:
// Back and Menu drive the navigator; Record toggles the ground-station
// recorder; Stream re-establishes the video; Settings and Pair jump to those
// screens. A PIC chip shows who holds pilot-in-command (the "what's locked"
// state). The buttons are also reachable by the physical panel buttons through
// the shared focus ring, so the bar is operable by touch or by button.

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

import { useTelemetryContext } from "@/hooks/telemetry-context";
import { startRecording, stopRecording } from "@/lib/api";
import { useFeedStore } from "@/stores/feed-store";
import { useNavStore } from "@/stores/nav-store";
import { cn } from "@/lib/utils";

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
        "flex min-h-[max(4rem,64px)] min-w-[max(4rem,64px)] flex-col items-center justify-center gap-[0.2rem] rounded-lg px-[0.4rem] backdrop-blur-sm transition-colors disabled:opacity-50",
        active
          ? "bg-err/85 text-white"
          : "bg-background/55 text-surface-foreground hover:bg-muted active:bg-muted",
      )}
    >
      <Icon className="h-[1.5rem] w-[1.5rem]" aria-hidden />
      <span className="text-[0.62rem] font-medium leading-none">{label}</span>
    </button>
  );
}

export function FeedActionBar() {
  const command = useNavStore((s) => s.command);
  const goTab = useNavStore((s) => s.goTab);
  const reconnectStream = useFeedStore((s) => s.reconnectStream);
  const { status } = useTelemetryContext();
  const [recordBusy, setRecordBusy] = useState(false);

  const recording =
    status?.recording === true || status?.video?.recording === true;
  const pic = status?.gcs?.pic_id ?? null;

  const toggleRecord = async () => {
    if (recordBusy) return;
    setRecordBusy(true);
    try {
      await (recording ? stopRecording() : startRecording());
    } catch {
      // The recorder call failed; the next status poll keeps the button honest
      // (it reflects the agent's real recording state, not an optimistic flip).
    } finally {
      setRecordBusy(false);
    }
  };

  return (
    <div className="pointer-events-none absolute inset-x-0 bottom-[3rem]">
      <div className="pointer-events-auto mx-auto flex w-fit items-end gap-[0.4rem] px-[0.4rem]">
        <ActionButton icon={ArrowLeft} label="Back" onClick={() => command("back")} />
        <ActionButton
          icon={MenuIcon}
          label="Menu"
          onClick={() => command("quick-menu")}
        />
        <ActionButton
          icon={recording ? Square : Circle}
          label={recording ? "Stop" : "Record"}
          onClick={toggleRecord}
          active={recording}
          disabled={recordBusy}
        />
        <ActionButton
          icon={RefreshCw}
          label="Stream"
          onClick={() => reconnectStream()}
        />
        <ActionButton
          icon={SettingsIcon}
          label="Settings"
          onClick={() => goTab("settings")}
        />
        <ActionButton icon={Link2} label="Pair" onClick={() => goTab("pair")} />
      </div>

      {/* PIC / what's-locked chip */}
      <div className="pointer-events-none absolute bottom-0 right-[0.6rem] flex items-center gap-[0.3rem] rounded-md bg-background/60 px-[0.5rem] py-[0.3rem] backdrop-blur-sm">
        <ShieldCheck
          className={cn("h-[0.9rem] w-[0.9rem]", pic ? "text-amber" : "text-muted-foreground")}
          aria-hidden
        />
        <span className="text-[0.6rem] uppercase tracking-wide text-muted-foreground">
          PIC
        </span>
        <span
          className={cn(
            "font-mono text-[0.72rem]",
            pic ? "text-surface-foreground" : "text-muted-foreground",
          )}
        >
          {pic ?? "unassigned"}
        </span>
      </div>
    </div>
  );
}
