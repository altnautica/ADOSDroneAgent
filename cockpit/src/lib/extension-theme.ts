// The `theme.changed` event an extension frame receives: the generated token
// set matching the cockpit's own theme. Posted when a frame loads and again
// whenever the operator switches the theme, so an open frame never keeps
// drawing in the old palette.

import { PROTOCOL_VERSION, type RpcEnvelope } from "@/lib/plugin-host";
import { EXTENSION_THEMES, type ExtensionThemeId } from "@/shared/extension-theme.generated";
import { useSettingsStore, type CockpitTheme } from "@/stores/settings-store";

const EXTENSION_THEME_FOR: Record<CockpitTheme, ExtensionThemeId> = {
  dark: "brand-dark",
  light: "brand-light",
  nvg: "nvg",
};

export function themeChangedEvent(theme: CockpitTheme): RpcEnvelope {
  return {
    type: "event",
    method: "theme.changed",
    capability: "theme.useTheme",
    args: EXTENSION_THEMES[EXTENSION_THEME_FOR[theme]],
    version: PROTOCOL_VERSION,
  };
}

/** Post `theme.changed` through `post` on every theme switch while `ready()`
 *  holds (the frame has loaded). Returns the unsubscribe. */
export function followTheme(post: (env: RpcEnvelope) => void, ready: () => boolean): () => void {
  return useSettingsStore.subscribe((s, prev) => {
    if (ready() && s.theme !== prev.theme) post(themeChangedEvent(s.theme));
  });
}
