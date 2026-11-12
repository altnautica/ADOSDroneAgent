import { existsSync, readdirSync, readFileSync, statSync } from "node:fs";
import path from "node:path";
import { describe, expect, it } from "vitest";

import specStrings from "../cockpit-spec/setup-strings.json";
import en from "./en.json";

// The locale is shared by every agent web app, so the key check reads every
// app's own source plus the shared code. Each app's `src/shared/` is a byte
// copy of web-shared and is skipped; an app directory that does not exist is
// skipped too.
const REPO = path.resolve(__dirname, "../../..");
const ROOTS = ["web-shared/src", "cockpit/src", "dashboard/src"]
  .map((r) => path.join(REPO, r))
  .filter((r) => existsSync(r));

function sourceFiles(dir: string): string[] {
  const out: string[] = [];
  for (const name of readdirSync(dir)) {
    const full = path.join(dir, name);
    if (statSync(full).isDirectory()) {
      if (name === "shared" || name === "node_modules") continue;
      out.push(...sourceFiles(full));
    } else if (/\.tsx?$/.test(name) && !/\.(test|d)\.tsx?$/.test(name)) {
      out.push(full);
    }
  }
  return out;
}

function lookup(key: string): unknown {
  let node: unknown = en;
  for (const part of key.split(".")) {
    if (typeof node !== "object" || node === null || !(part in node)) return undefined;
    node = (node as Record<string, unknown>)[part];
  }
  return node;
}

// A plural key resolves through its _one/_other forms.
function hasKey(key: string): boolean {
  if (typeof lookup(key) === "string") return true;
  return typeof lookup(`${key}_one`) === "string" && typeof lookup(`${key}_other`) === "string";
}

// Config paths such as "video.camera.rotation" share the dotted shape, so a
// bare literal counts as a key only when its parent group exists in en.json.
// Flight-spec keys (`cockpit-spec/setup-strings.json`, read through
// `specText`) share the shape too and are not locale keys.
function isKeyShaped(literal: string): boolean {
  const parts = literal.split(".");
  if (parts.length < 3 || !namespaces.has(parts[0]) || literal in specStrings.strings) return false;
  const parent = lookup(parts.slice(0, -1).join("."));
  return typeof parent === "object" && parent !== null;
}

// Keys are written as plain literals: t("ns.key"), i18nKey="ns.key", or a
// literal stored for a later t(key) call.
const namespaces = new Set(Object.keys(en));
const KEY_LITERAL = /["'`]([a-z][A-Za-z0-9]*(?:\.[A-Za-z0-9_]+)+)["'`]/g;
const T_CALL = /\bt\(\s*["'`]([^"'`$]+)["'`]/g;

function referencedKeys(): Map<string, string> {
  const refs = new Map<string, string>();
  for (const root of ROOTS) {
    for (const file of sourceFiles(root)) {
      const text = readFileSync(file, "utf8");
      const rel = path.relative(REPO, file);
      for (const m of text.matchAll(T_CALL)) refs.set(m[1], rel);
      for (const m of text.matchAll(KEY_LITERAL)) {
        if (isKeyShaped(m[1])) refs.set(m[1], rel);
      }
    }
  }
  return refs;
}

// A literal t("ns.key") call must pass every {{variable}} its string
// interpolates: i18next leaves a missing one in the text verbatim. A call
// whose options are not an object literal (a spread variable) is not judged.
function argumentText(text: string, from: number): string | null {
  let depth = 1;
  for (let i = from; i < text.length; i++) {
    const c = text[i];
    if (c === "(" || c === "{" || c === "[") depth++;
    else if (c === ")" || c === "}" || c === "]") {
      depth--;
      if (depth === 0) return text.slice(from, i);
    }
  }
  return null;
}

function interpolated(key: string): string[] {
  const forms = [lookup(key), lookup(`${key}_one`), lookup(`${key}_other`)];
  const vars = new Set<string>();
  for (const s of forms) {
    if (typeof s !== "string") continue;
    for (const m of s.matchAll(/\{\{\s*([A-Za-z_][A-Za-z0-9_]*)/g)) vars.add(m[1]);
  }
  return [...vars];
}

function missingVariables(): string[] {
  const out: string[] = [];
  const call = /\bt\(\s*["'`]([^"'`$]+)["'`]\s*([,)])/g;
  for (const root of ROOTS) {
    for (const file of sourceFiles(root)) {
      const text = readFileSync(file, "utf8");
      const rel = path.relative(REPO, file);
      for (const m of text.matchAll(call)) {
        const vars = interpolated(m[1]);
        if (vars.length === 0) continue;
        let args = "";
        if (m[2] === ",") {
          const rest = argumentText(text, (m.index ?? 0) + m[0].length);
          if (rest === null) continue;
          args = rest.trim();
          if (!args.startsWith("{") || /\.\.\./.test(args)) continue;
        }
        for (const v of vars) {
          if (!new RegExp(`\\b${v}\\b`).test(args)) out.push(`${m[1]} needs {{${v}}} (${rel})`);
        }
      }
    }
  }
  return out;
}

describe("locales/en.json", () => {
  it("defines every key the source references", () => {
    const missing = [...referencedKeys()]
      .filter(([key]) => !hasKey(key))
      .map(([key, file]) => `${key} (${file})`);
    expect(missing).toEqual([]);
  });

  it("references at least one key per namespace", () => {
    const used = new Set([...referencedKeys().keys()].map((k) => k.split(".")[0]));
    expect([...namespaces].filter((ns) => !used.has(ns))).toEqual([]);
  });

  it("passes every variable a referenced string interpolates", () => {
    expect(missingVariables()).toEqual([]);
  });
});
