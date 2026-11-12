// Params screen rules that sit on top of the metadata catalog: search matches
// the readable name and description as well as the parameter id, and an entry
// outside the catalog's range is refused before it becomes a draft.

interface MetaText {
  humanName?: string | null;
  description?: string | null;
  range?: { min: number; max: number } | null;
}

/** Whether `name` (with its catalog entry) matches the search text. */
export function paramMatches(name: string, meta: MetaText | undefined, search: string): boolean {
  const q = search.trim().toLowerCase();
  if (!q) return true;
  return [name, meta?.humanName, meta?.description].some((s) => typeof s === "string" && s.toLowerCase().includes(q));
}

/** The catalog range `value` falls outside, or null when it is in range or the
 *  catalog gives none. */
export function outOfRange(value: number, meta: MetaText | undefined): { min: number; max: number } | null {
  const r = meta?.range;
  if (!r || !Number.isFinite(r.min) || !Number.isFinite(r.max)) return null;
  return value < r.min || value > r.max ? r : null;
}

/** The catalog default when `value` differs from it, else null (no default
 *  in the catalog, or equal within float noise). */
export function nonDefault(value: number, meta: { defaultValue?: number } | undefined): number | null {
  const d = meta?.defaultValue;
  if (d == null || !Number.isFinite(d)) return null;
  return Math.abs(value - d) > 1e-6 * Math.max(1, Math.abs(d)) ? d : null;
}
