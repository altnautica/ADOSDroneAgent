// This panel's own look: the theme (dark, light, or night-vision green from
// the generated token set) and glove mode (larger touch targets). Both are
// cockpit-local preferences, kept in the browser, never written to the agent.

import { useTranslation } from "react-i18next";

import { SelectSettingRow, SwitchSettingRow } from "@/components/ui/setting-row";
import "@/shared/i18n";
import { COCKPIT_THEMES, useSettingsStore, type CockpitTheme } from "@/stores/settings-store";

const THEME_KEY = {
  dark: "cockpit.settingsSections.appearance.themeDark",
  light: "cockpit.settingsSections.appearance.themeLight",
  nvg: "cockpit.settingsSections.appearance.themeNvg",
} as const satisfies Record<CockpitTheme, string>;

export function AppearanceSettings() {
  const { t } = useTranslation();
  const theme = useSettingsStore((s) => s.theme);
  const setTheme = useSettingsStore((s) => s.setTheme);
  const glove = useSettingsStore((s) => s.glove);
  const setGlove = useSettingsStore((s) => s.setGlove);

  return (
    <div className="flex flex-col gap-[0.3rem]">
      <SelectSettingRow
        title={t("cockpit.settingsSections.appearance.theme")}
        summary={t("cockpit.settingsSections.appearance.themeSummary")}
        options={COCKPIT_THEMES}
        value={theme}
        optionLabel={(th) => t(THEME_KEY[th])}
        onSelect={setTheme}
      />
      <SwitchSettingRow
        title={t("cockpit.settingsSections.appearance.gloveMode")}
        summary={t("cockpit.settingsSections.appearance.gloveSummary")}
        checked={glove}
        onChange={setGlove}
      />
    </div>
  );
}
