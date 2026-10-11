import type { ParseKeys } from "i18next";
import { useTranslation } from "react-i18next";
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
const LABEL_KEY: Record<Theme, ParseKeys> = {
  dark: "layout.theme.dark",
  light: "layout.theme.light",
  system: "layout.theme.system",
};

export function ThemeToggle() {
  const { t } = useTranslation();
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
          aria-label={t("layout.theme.switchTo", { theme: t(LABEL_KEY[next]) })}
          onClick={() => setTheme(next)}
        >
          <Icon />
        </Button>
      </TooltipTrigger>
      <TooltipContent side="bottom">{t(LABEL_KEY[theme])}</TooltipContent>
    </Tooltip>
  );
}
