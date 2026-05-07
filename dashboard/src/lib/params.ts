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

export function formatParamValue(v: number): string {
  if (Number.isInteger(v)) return v.toString();
  // Show up to 4 decimals, trim trailing zeros
  return parseFloat(v.toFixed(4)).toString();
}
