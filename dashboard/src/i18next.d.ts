import "i18next";

import type en from "./locales/en.json";

// Typed keys: a t("...") call with a key missing from en.json fails typecheck.
declare module "i18next" {
  interface CustomTypeOptions {
    defaultNS: "translation";
    resources: { translation: typeof en };
  }
}
