// Capture exposure fields: typed text is kept as typed and checked against
// the agent's bounds (exposure a whole number of µs ≥ 1 that fits u32, gain
// ≥ 0.01), so "abc" reads as an error, never as a silently disabled Apply.

import type { TFunction } from "i18next";

const U32_MAX = 4_294_967_295;
const GAIN_MIN = 0.01;

export function exposureError(raw: string, t: TFunction): string | null {
  const s = raw.trim();
  if (s === "") return t("settings.shared.enterValue");
  const n = Number(s);
  if (!Number.isFinite(n)) return t("settings.shared.enterNumber");
  if (!Number.isInteger(n)) return t("settings.shared.enterWholeNumber");
  if (n < 1) return t("settings.shared.mustBeAtLeast", { min: 1 });
  if (n > U32_MAX) return t("settings.shared.mustBeAtMost", { max: U32_MAX });
  return null;
}

export function gainError(raw: string, t: TFunction): string | null {
  const s = raw.trim();
  if (s === "") return t("settings.shared.enterValue");
  const n = Number(s);
  if (!Number.isFinite(n)) return t("settings.shared.enterNumber");
  if (n < GAIN_MIN) return t("settings.shared.mustBeAtLeast", { min: GAIN_MIN });
  return null;
}
