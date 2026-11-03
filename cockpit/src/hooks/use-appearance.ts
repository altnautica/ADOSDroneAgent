// Applies the persisted appearance settings to the document root: the UI-scale
// knob (the fluid root font-size, styles/globals.css `--ui-scale`), the theme
// (`data-theme`, which selects the generated token set) and glove mode (the
// `--touch-scale` every touch target multiplies its floor by).

import { useEffect } from "react";

import { GLOVE_TOUCH_SCALE } from "@/lib/cockpit-layout";
import { useSettingsStore } from "@/stores/settings-store";

export function useAppearance(): void {
  const uiScale = useSettingsStore((s) => s.uiScale);
  const theme = useSettingsStore((s) => s.theme);
  const glove = useSettingsStore((s) => s.glove);

  useEffect(() => {
    document.documentElement.style.setProperty("--ui-scale", String(uiScale));
  }, [uiScale]);

  useEffect(() => {
    document.documentElement.dataset.theme = theme;
  }, [theme]);

  useEffect(() => {
    document.documentElement.style.setProperty("--touch-scale", String(glove ? GLOVE_TOUCH_SCALE : 1));
  }, [glove]);
}
