// The flight screen's clock format (battery time to reserve, flight timer).
// Unknown is always a dash, never a fabricated value.

import { DASH } from "@/shared/format";

/** `m:ss`, or `h:mm:ss` past an hour. */
export function fmtClock(seconds: number | null): string {
  if (seconds === null || !Number.isFinite(seconds) || seconds < 0) return DASH;
  const s = Math.floor(seconds);
  const h = Math.floor(s / 3600);
  const mm = Math.floor((s % 3600) / 60);
  const ss = String(s % 60).padStart(2, "0");
  return h > 0 ? `${h}:${String(mm).padStart(2, "0")}:${ss}` : `${mm}:${ss}`;
}
