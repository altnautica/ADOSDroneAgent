// Setup wizard text and pictures from the cockpit spec: `setup-strings.json`
// (`{v, locale, strings:{"<dotted.key>": "English {arg}"}}`) and
// `setup-images/<key>.svg`, where a step's `image` is the file stem. Every
// surface resolves the engine's text keys through the same file, so the wizard
// reads the same on all of them. The pictures are looked up by glob, so the
// bundle carries exactly the files the spec ships.

import setupStrings from "../cockpit-spec/setup-strings.json";

import type { SetupValue } from "./types";

const STRINGS: Readonly<Record<string, string>> = setupStrings.strings;

const IMAGE_URLS = import.meta.glob<string>("../cockpit-spec/setup-images/*.svg", {
  eager: true,
  query: "?url",
  import: "default",
});

/** Fill `{arg}` placeholders from `args`; an unknown placeholder stays as written. */
export function interpolate(template: string, args: Record<string, SetupValue>): string {
  return template.replace(/\{([A-Za-z0-9_]+)\}/g, (whole, name: string) => {
    const v = args[name];
    return v === undefined || v === null ? whole : formatValue(v);
  });
}

/** The English text for `key` with `args` filled in. A key the spec does not
 *  carry renders as the key itself, so a gap is visible rather than blank. */
export function setupText(
  key: string,
  args: Record<string, SetupValue> = {},
  table: Readonly<Record<string, string>> = STRINGS,
): string {
  const template = table[key];
  return template === undefined ? key : interpolate(template, args);
}

/** A live-value or result key as a label: the spec's `setup.<group>.<key>`
 *  when it names one, else the key itself resolved as a text key (the engine
 *  may send a full key such as `setup.live.compass_progress`). */
export function setupLabel(group: "live" | "result", key: string, args: Record<string, SetupValue> = {}): string {
  const named = `setup.${group}.${key}`;
  return setupText(named in STRINGS ? named : key, args);
}

/** The bundled URL of `setup-images/<key>.svg`, or null when the spec has none. */
export function setupImageUrl(key: string | null): string | null {
  if (!key) return null;
  return IMAGE_URLS[`../cockpit-spec/setup-images/${key}.svg`] ?? null;
}

/** A value as the wizard shows it: numbers to at most three decimals,
 *  booleans as ✓/✗, nested objects flattened to `k v` pairs. */
export function formatValue(v: unknown): string {
  if (v === null || v === undefined) return "—";
  if (typeof v === "number") return Number.isInteger(v) ? String(v) : String(Number(v.toFixed(3)));
  if (typeof v === "boolean") return v ? "✓" : "✗";
  if (typeof v === "string") return v;
  if (Array.isArray(v)) return v.map(formatValue).join(", ");
  if (typeof v === "object") {
    return Object.entries(v)
      .map(([k, val]) => `${k} ${formatValue(val)}`)
      .join(" · ");
  }
  return String(v);
}
