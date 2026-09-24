// Pure helpers for the parameter editor. Kept side-effect-free so the
// component can stay focused on layout and the helpers can be tested
// without React.

export function categoryFromName(name: string): string {
  // ArduPilot/iNav/Betaflight all use prefix-underscore namespacing
  // (BATT_LOW_VOLT, GPS_TYPE, ARMING_REQUIRE, MOT_PWM_TYPE, etc.).
  // First underscore segment is the category.
  const idx = name.indexOf("_");
  if (idx <= 0) return "OTHER";
  return name.slice(0, idx).toUpperCase();
}

export interface ParamRow {
  name: string;
  value: number;
  category: string;
}

export function buildRows(params: Record<string, number>): ParamRow[] {
  const rows: ParamRow[] = [];
  for (const [name, value] of Object.entries(params)) {
    rows.push({ name, value, category: categoryFromName(name) });
  }
  rows.sort((a, b) => {
    const c = a.category.localeCompare(b.category);
    if (c !== 0) return c;
    return a.name.localeCompare(b.name);
  });
  return rows;
}

export function categoryCounts(rows: ParamRow[]): Record<string, number> {
  const counts: Record<string, number> = {};
  for (const row of rows) {
    counts[row.category] = (counts[row.category] || 0) + 1;
  }
  return counts;
}

export interface FilterState {
  category: string | null; // null = all
  search: string; // case-insensitive substring on name
  modifiedOnly: boolean;
  modified: Set<string>;
}

export function filterRows(rows: ParamRow[], filter: FilterState): ParamRow[] {
  const q = filter.search.trim().toLowerCase();
  return rows.filter((row) => {
    if (filter.category && row.category !== filter.category) return false;
    if (q && !row.name.toLowerCase().includes(q)) return false;
    if (filter.modifiedOnly && !filter.modified.has(row.name)) return false;
    return true;
  });
}

/**
 * Why a parameter write is blocked, or null when it may go out. Only an FC that
 * reports disarmed is written to: `null`/absent means its heartbeat has not
 * reported the armed state yet, and unknown is not disarmed.
 */
export function paramWriteBlock(armed: boolean | null | undefined): string | null {
  if (armed === false) return null;
  return armed
    ? "Vehicle is armed — parameter writes are blocked. Disarm to save changes."
    : "Armed state not reported yet — parameter writes are blocked until the flight controller reports it disarmed.";
}

export function formatParamValue(v: number): string {
  if (Number.isInteger(v)) return v.toString();
  // Show up to 4 decimals, trim trailing zeros
  return parseFloat(v.toFixed(4)).toString();
}
