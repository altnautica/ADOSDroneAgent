// Applies the persisted UI-scale knob to the document root so the fluid
// root font-size (styles/globals.css `--ui-scale`) sizes the whole layout up
// or down without changing the layout itself.

import { useEffect } from "react";

import { useSettingsStore } from "@/stores/settings-store";

export function useUiScale(): number {
  const uiScale = useSettingsStore((s) => s.uiScale);

  useEffect(() => {
    document.documentElement.style.setProperty("--ui-scale", String(uiScale));
  }, [uiScale]);

  return uiScale;
}
