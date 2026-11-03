import i18n from "i18next";
import { initReactI18next } from "react-i18next";

import en from "./locales/en.json";

// Single bundled locale today. Keys are namespaced by surface
// ("common.*", "home.*", "settings.network.*", ...) inside the default
// "translation" namespace so every string lives in one reviewable file.
export const resources = { en: { translation: en } } as const;

void i18n.use(initReactI18next).init({
  resources,
  lng: "en",
  fallbackLng: "en",
  interpolation: {
    // React already escapes rendered values.
    escapeValue: false,
  },
});

export default i18n;
