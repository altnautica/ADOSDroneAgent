import { readdirSync, readFileSync, statSync } from "node:fs";
import path from "node:path";
import { describe, expect, it } from "vitest";

import en from "./en.json";

const SRC = path.resolve(__dirname, "..");

function sourceFiles(dir: string): string[] {
  const out: string[] = [];
  for (const name of readdirSync(dir)) {
    const full = path.join(dir, name);
    if (statSync(full).isDirectory()) {
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
function isKeyShaped(literal: string): boolean {
  const parts = literal.split(".");
  if (parts.length < 3 || !namespaces.has(parts[0])) return false;
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
  for (const file of sourceFiles(SRC)) {
    const text = readFileSync(file, "utf8");
    const rel = path.relative(SRC, file);
    for (const m of text.matchAll(T_CALL)) refs.set(m[1], rel);
    for (const m of text.matchAll(KEY_LITERAL)) {
      if (isKeyShaped(m[1])) refs.set(m[1], rel);
    }
  }
  return refs;
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
});
