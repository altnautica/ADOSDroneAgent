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

/** The motor-order pictures as markup, so the outputs step can mark the motor
 *  under test. */
const MOTOR_SVGS = import.meta.glob<string>("../cockpit-spec/setup-images/motor_order_*.svg", {
  eager: true,
  query: "?raw",
  import: "default",
});

/** Fill `{arg}` placeholders from `args`; an unknown placeholder stays as written. */
export function interpolate(template: string, args: Record<string, SetupValue>): string {
  return template.replace(/\{([A-Za-z0-9_]+)\}/g, (whole, name: string) => {
    const v = args[name];
    return v === undefined || v === null ? whole : formatValue(v);
  });
}

/** The English text for `key` with `args` filled in. Per the spec's lookup
 *  rule, a missing `setup.<step>.body.<variant|phase>` falls back to
 *  `setup.<step>.body`; any other key the spec does not carry renders as the
 *  key itself, so a gap is visible rather than blank. */
export function setupText(
  key: string,
  args: Record<string, SetupValue> = {},
  table: Readonly<Record<string, string>> = STRINGS,
): string {
  const bodyVariant = /^(setup\.[^.]+\.body)\.[^.]+$/.exec(key);
  const template = table[key] ?? (bodyVariant ? table[bodyVariant[1]] : undefined);
  return template === undefined ? key : interpolate(template, args);
}

/** Whether the spec carries `key`. */
export function hasSetupText(key: string): boolean {
  return key in STRINGS;
}

/** The words for an engine refusal code (`{ok:false, err:"armed"}`): its
 *  `setup.err.<code>`, else `setup.blocker.<code>`, else the code. */
export function setupErrorText(code: string): string {
  if (`setup.err.${code}` in STRINGS) return setupText(`setup.err.${code}`);
  if (`setup.blocker.${code}` in STRINGS) return setupText(`setup.blocker.${code}`);
  return setupText(code);
}

/** An enum option's text: its own `label_key`, else the engine's convention
 *  `<input label_key>.<value>` when the spec carries it, else the value. */
export function optionText(inputLabelKey: string, value: SetupValue, labelKey: string | null): string {
  if (labelKey) return setupText(labelKey);
  const named = `${inputLabelKey}.${String(value)}`;
  return named in STRINGS ? setupText(named) : formatValue(value);
}

/** A live-value or result key as a label: the spec's `setup.<group>.<key>`
 *  when it names one, else the key itself resolved as a text key (the engine
 *  may send a full key such as `setup.live.compass_progress`). */
export function setupLabel(group: "live" | "result", key: string, args: Record<string, SetupValue> = {}): string {
  const named = `setup.${group}.${key}`;
  return setupText(named in STRINGS ? named : key, args);
}

/** What a parameter a step writes is for (`setup.<step>.param.<NAME>`, e.g.
 *  the flight controller's obstacle backstop rows), or null when the spec
 *  says nothing about it. */
export function paramNote(step: string, name: string): string | null {
  return STRINGS[`setup.${step}.param.${name}`] ?? null;
}

/** The square around a motor-order picture's motors and spin arrows (every
 *  shipped layout keeps them within x 66..254, y 12..200 of its 320×200
 *  canvas); the corner title and CW/CCW legend fall outside it. */
const MOTOR_TILE_VIEWBOX = "66 12 188 188";

/** A motor-order picture as picker-tile markup: cropped square to its
 *  motors, sized by its container, the motor numbers enlarged so they read
 *  on a small tile. Null when `key` is not a motor-order picture. */
export function motorOrderTile(key: string | null, sources: Readonly<Record<string, string>> = MOTOR_SVGS): string | null {
  if (!key) return null;
  const svg = sources[`../cockpit-spec/setup-images/${key}.svg`];
  if (!svg) return null;
  return svg
    .replace(/viewBox="[^"]*"\s+width="[^"]*"\s+height="[^"]*"/, `viewBox="${MOTOR_TILE_VIEWBOX}" width="100%" height="100%"`)
    .replace(/font-size="13"( font-weight="600" text-anchor="middle")/g, 'font-size="18"$1');
}

/** The bundled URL of `setup-images/<key>.svg`, or null when the spec has none. */
export function setupImageUrl(key: string | null): string | null {
  if (!key) return null;
  return IMAGE_URLS[`../cockpit-spec/setup-images/${key}.svg`] ?? null;
}

/** A motor-order picture as markup with motor `n` (the number drawn on it)
 *  filled in the warning colour, or null when `key` is not a motor-order
 *  picture or has no motor `n`. */
export function motorOrderMarkup(key: string | null, n: number, sources: Readonly<Record<string, string>> = MOTOR_SVGS): string | null {
  if (!key) return null;
  const svg = sources[`../cockpit-spec/setup-images/${key}.svg`];
  if (!svg) return null;
  const motor = new RegExp(
    `<circle cx="([\\d.]+)" cy="([\\d.]+)" r="([\\d.]+)"/>\\s*(<text x="\\1" y="\\2" )fill="#EDEDED"([^>]*>${n}</text>)`,
  );
  if (!motor.test(svg)) return null;
  return svg.replace(
    motor,
    (_all, x: string, y: string, r: string, open: string, rest: string) =>
      `<circle cx="${x}" cy="${y}" r="${Number(r) + 2}" style="fill:hsl(var(--warn));stroke:hsl(var(--warn))"/>${open}fill="#111111"${rest}`,
  );
}

/** A value as the wizard shows it: numbers to at most three decimals,
 *  booleans as ✓/✗, a parameter row (`{name, value}`) as `NAME = value`,
 *  other nested objects flattened to `k v` pairs. */
export function formatValue(v: unknown): string {
  if (v === null || v === undefined) return "—";
  if (typeof v === "number") return Number.isInteger(v) ? String(v) : String(Number(v.toFixed(3)));
  if (typeof v === "boolean") return v ? "✓" : "✗";
  if (typeof v === "string") return v;
  if (Array.isArray(v)) return v.map(formatValue).join(", ");
  if (typeof v === "object") {
    const o = v as Record<string, unknown>;
    const keys = Object.keys(o);
    if (keys.length === 2 && typeof o.name === "string" && "value" in o) return `${o.name} = ${formatValue(o.value)}`;
    return Object.entries(o)
      .map(([k, val]) => `${k} ${formatValue(val)}`)
      .join(" · ");
  }
  return String(v);
}
