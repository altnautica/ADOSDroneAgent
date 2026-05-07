import { Moon, Sun, MonitorCog } from "lucide-react";

import { Button } from "@/components/ui/button";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import { useUiStore, type Theme } from "@/stores/ui-store";

const ORDER: Theme[] = ["dark", "light", "system"];
const ICON: Record<Theme, typeof Sun> = {
  dark: Moon,
  light: Sun,
  system: MonitorCog,
};
const LABEL: Record<Theme, string> = {
  dark: "dark theme",
  light: "light theme",
  system: "follow system",
};

export function ThemeToggle() {
  const theme = useUiStore((s) => s.theme);
  const setTheme = useUiStore((s) => s.setTheme);
  const Icon = ICON[theme];

  const next = ORDER[(ORDER.indexOf(theme) + 1) % ORDER.length] as Theme;

  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <Button
          variant="ghost"
          size="icon"
          aria-label={`switch to ${LABEL[next]}`}
          onClick={() => setTheme(next)}
        >
          <Icon />
        </Button>
      </TooltipTrigger>
      <TooltipContent side="bottom">{LABEL[theme]}</TooltipContent>
    </Tooltip>
  );
}
