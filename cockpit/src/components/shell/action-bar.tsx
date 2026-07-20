// L3 — the persistent bottom action bar. Global quick actions (collapse the
// menu, UI-scale, quick menu), a compact input-hint legend, and live input
// indicators (button stream / gamepad / wake lock). Buttons are >=48px touch
// targets. When `floating` (over the Feed video) it is translucent.

import {
  Gamepad2,
  Menu as MenuIcon,
  Minus,
  Monitor,
  MonitorOff,
  PanelLeftClose,
  PanelLeftOpen,
  Plus,
  Radio,
} from "lucide-react";

import { useNavStore } from "@/stores/nav-store";
import { useSettingsStore, UI_SCALE_STEP } from "@/stores/settings-store";
import { cn } from "@/lib/utils";

export interface InputStatus {
  buttonsConnected: boolean;
  gamepadConnected: boolean;
  wakeHeld: boolean;
}

function BarButton({
  onClick,
  label,
  children,
}: {
  onClick: () => void;
  label: string;
  children: React.ReactNode;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      aria-label={label}
      className="touch-target flex items-center justify-center rounded-md px-[0.5rem] text-surface-foreground hover:bg-muted"
    >
      {children}
    </button>
  );
}

function Indicator({
  on,
  title,
  children,
}: {
  on: boolean;
  title: string;
  children: React.ReactNode;
}) {
  return (
    <span
      title={title}
      className={cn("flex items-center", on ? "text-ok" : "text-muted-foreground/50")}
    >
      {children}
    </span>
  );
}

export function ActionBar({
  floating = false,
  input,
}: {
  floating?: boolean;
  input: InputStatus;
}) {
  const menuCollapsed = useNavStore((s) => s.menuCollapsed);
  const toggleMenuCollapsed = useNavStore((s) => s.toggleMenuCollapsed);
  const command = useNavStore((s) => s.command);
  const uiScale = useSettingsStore((s) => s.uiScale);
  const nudgeUiScale = useSettingsStore((s) => s.nudgeUiScale);

  return (
    <div
      className={cn(
        "flex items-center gap-[0.4rem] px-[0.4rem]",
        floating
          ? "pointer-events-auto bg-background/60 backdrop-blur-sm"
          : "border-t border-border bg-surface/70",
      )}
    >
      <BarButton
        onClick={toggleMenuCollapsed}
        label={menuCollapsed ? "Show menu" : "Hide menu"}
      >
        {menuCollapsed ? (
          <PanelLeftOpen className="h-[1.2rem] w-[1.2rem]" />
        ) : (
          <PanelLeftClose className="h-[1.2rem] w-[1.2rem]" />
        )}
      </BarButton>
      <BarButton onClick={() => command("quick-menu")} label="Quick menu">
        <MenuIcon className="h-[1.2rem] w-[1.2rem]" />
      </BarButton>

      <div className="mx-[0.3rem] flex items-center gap-[0.15rem]">
        <BarButton onClick={() => nudgeUiScale(-UI_SCALE_STEP)} label="Smaller UI">
          <Minus className="h-[1.1rem] w-[1.1rem]" />
        </BarButton>
        <span className="w-[2.4rem] text-center font-mono text-[0.72rem] text-muted-foreground">
          {Math.round(uiScale * 100)}%
        </span>
        <BarButton onClick={() => nudgeUiScale(UI_SCALE_STEP)} label="Larger UI">
          <Plus className="h-[1.1rem] w-[1.1rem]" />
        </BarButton>
      </div>

      <div className="hidden flex-1 items-center justify-center gap-[0.6rem] font-mono text-[0.68rem] text-muted-foreground landscape:flex">
        <span>◀ ▶ move</span>
        <span>● select</span>
        <span>↩ back</span>
        <span>≡ menu</span>
      </div>

      <div className="ml-auto flex items-center gap-[0.55rem] pr-[0.3rem]">
        <Indicator on={input.buttonsConnected} title="Front-panel buttons">
          <Radio className="h-[1.05rem] w-[1.05rem]" />
        </Indicator>
        <Indicator on={input.gamepadConnected} title="Gamepad">
          <Gamepad2 className="h-[1.05rem] w-[1.05rem]" />
        </Indicator>
        <Indicator on={input.wakeHeld} title="Screen wake lock">
          {input.wakeHeld ? (
            <Monitor className="h-[1.05rem] w-[1.05rem]" />
          ) : (
            <MonitorOff className="h-[1.05rem] w-[1.05rem]" />
          )}
        </Indicator>
      </div>
    </div>
  );
}
