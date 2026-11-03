// This panel's own look: the theme (dark, light, or night-vision green from
// the generated token set) and glove mode (larger touch targets). Both are
// cockpit-local preferences, kept in the browser, never written to the agent.

import { SelectSettingRow, SwitchSettingRow } from "@/components/ui/setting-row";
import { COCKPIT_THEMES, useSettingsStore, type CockpitTheme } from "@/stores/settings-store";

const THEME_LABEL: Record<CockpitTheme, string> = {
  dark: "Dark",
  light: "Light",
  nvg: "Night vision",
};

export function AppearanceSettings() {
  const theme = useSettingsStore((s) => s.theme);
  const setTheme = useSettingsStore((s) => s.setTheme);
  const glove = useSettingsStore((s) => s.glove);
  const setGlove = useSettingsStore((s) => s.setGlove);

  return (
    <div className="flex flex-col gap-[0.3rem]">
      <SelectSettingRow
        title="Theme"
        summary="Night vision keeps the panel dim and green"
        options={COCKPIT_THEMES}
        value={theme}
        optionLabel={(t) => THEME_LABEL[t]}
        onSelect={setTheme}
      />
      <SwitchSettingRow
        title="Glove mode"
        summary="Larger touch targets"
        checked={glove}
        onChange={setGlove}
      />
    </div>
  );
}
