// MAVLink GPS fields as the operator reads them: the numeric GPS_FIX_TYPE as a
// fix word, and the HDOP with the "unknown" sentinel (UINT16_MAX cm → 655.35)
// dropped instead of printed as a precision.

/** The GNSS fix words the cockpit already labels (`cockpit.gnss.fixLabel.*`). */
export type FixWord = "none" | "2d" | "3d" | "dgps" | "rtk_float" | "rtk_fixed";

/** GPS_FIX_TYPE → fix word. 0 no GPS and 1 no fix read as none; static (7)
 *  and PPP (8) are 3D solutions without RTK. Unknown values read as none. */
export function mavFixWord(fixType: number | null | undefined): FixWord | null {
  if (fixType == null || !Number.isFinite(fixType)) return null;
  switch (fixType) {
    case 2:
      return "2d";
    case 3:
    case 7:
    case 8:
      return "3d";
    case 4:
      return "dgps";
    case 5:
      return "rtk_float";
    case 6:
      return "rtk_fixed";
    default:
      return "none";
  }
}

/** HDOP for display, or null when absent or the MAVLink unknown sentinel. */
export function mavHdop(eph: number | null | undefined): number | null {
  if (eph == null || !Number.isFinite(eph) || eph < 0 || eph >= 655.35 - 1e-6) return null;
  return eph;
}

export const FIX_WORDS = ["none", "2d", "3d", "dgps", "rtk_float", "rtk_fixed"] as const;
export const RTK_WORDS = ["none", "float", "fixed"] as const;
export const BASE_STATE_WORDS = ["off", "surveying", "fixed", "failed"] as const;

/** The agent's word when it is one the cockpit labels, else null (the caller
 *  then shows the word as sent rather than guess a label). */
export function knownWord<T extends string>(v: string | null | undefined, words: readonly T[]): T | null {
  return v != null && (words as readonly string[]).includes(v) ? (v as T) : null;
}
