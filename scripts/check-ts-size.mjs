#!/usr/bin/env node
// Ratchet the size of every .ts/.tsx source in the web apps: cockpit/src,
// dashboard/src and web-shared/src.
//
// - A file at or under LIMIT lines passes.
// - A file over LIMIT must be listed in .ts-size-baseline and may not grow
//   past its recorded count. Shrinking is always fine; --update lowers the
//   recorded counts and drops files that fell under the limit.
// - The synced copies of web-shared (cockpit/src/shared, dashboard/src/shared)
//   are counted once, at their source in web-shared/src.
//
//   node scripts/check-ts-size.mjs            # check, exit 1 on a violation
//   node scripts/check-ts-size.mjs --update   # lower the baseline
//
// --update never raises a count or adds a file: growth is fixed by splitting
// the file. --update --accept-growth exists only to seed a baseline.

import { existsSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import { dirname, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const REPO = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const BASELINE = join(REPO, ".ts-size-baseline");
const ROOTS = ["cockpit/src", "dashboard/src", "web-shared/src"];
const LIMIT = 500;
const SKIP_DIRS = new Set(["node_modules", "dist"]);
// Synced copies of web-shared/src: checked once, at the source.
const SKIP_PATHS = new Set(["cockpit/src/shared", "dashboard/src/shared"]);
const EXTS = [".ts", ".tsx"];

function walk(dir, out) {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const full = join(dir, entry.name);
    if (entry.isDirectory()) {
      const rel = relative(REPO, full).split("\\").join("/");
      if (!SKIP_DIRS.has(entry.name) && !SKIP_PATHS.has(rel)) walk(full, out);
    } else if (EXTS.some((e) => entry.name.endsWith(e))) {
      out.push(full);
    }
  }
  return out;
}

function lineCount(file) {
  const text = readFileSync(file, "utf8");
  if (text.length === 0) return 0;
  const n = text.split("\n").length;
  return text.endsWith("\n") ? n - 1 : n;
}

function sources() {
  const counts = new Map();
  for (const root of ROOTS) {
    const dir = join(REPO, root);
    if (!existsSync(dir)) continue;
    for (const file of walk(dir, []).sort()) {
      counts.set(relative(REPO, file).split("\\").join("/"), lineCount(file));
    }
  }
  return counts;
}

function readBaseline() {
  const out = new Map();
  if (!existsSync(BASELINE)) return out;
  for (const raw of readFileSync(BASELINE, "utf8").split("\n")) {
    const line = raw.trim();
    if (!line || line.startsWith("#")) continue;
    const space = line.indexOf(" ");
    out.set(line.slice(space + 1), Number(line.slice(0, space)));
  }
  return out;
}

function writeBaseline(counts) {
  const lines = [
    "# Web app source files over the size limit, with their line count. A listed",
    `# file may shrink but never grow; an unlisted file must stay at or under ${LIMIT}`,
    "# lines. Maintained by scripts/check-ts-size.mjs.",
    ...[...counts.keys()].sort().map((rel) => `${counts.get(rel)} ${rel}`),
  ];
  writeFileSync(BASELINE, lines.join("\n") + "\n");
}

function check(current, baseline) {
  const errors = [];
  for (const [rel, count] of current) {
    if (count <= LIMIT) continue;
    const allowed = baseline.get(rel);
    if (allowed === undefined) errors.push(`${rel}: ${count} lines, limit ${LIMIT} (new or unlisted file)`);
    else if (count > allowed) errors.push(`${rel}: ${count} lines, grew past its baseline of ${allowed}`);
  }
  return errors;
}

const args = new Set(process.argv.slice(2));
const current = sources();
const baseline = readBaseline();

if (args.has("--update")) {
  const kept = new Map();
  for (const [rel, count] of current) {
    if (count <= LIMIT) continue;
    if (args.has("--accept-growth")) kept.set(rel, count);
    else if (baseline.has(rel)) kept.set(rel, Math.min(count, baseline.get(rel)));
  }
  writeBaseline(kept);
  console.log(`baseline: ${kept.size} file(s) over ${LIMIT} lines recorded`);
  const errors = args.has("--accept-growth") ? [] : check(current, kept);
  for (const e of errors) console.log(`  ${e}`);
  process.exit(errors.length ? 1 : 0);
}

const errors = check(current, baseline);
if (errors.length) {
  console.log(`${errors.length} source file(s) over the size ratchet:\n`);
  for (const e of errors) console.log(`  ${e}`);
  console.log(`\nSplit the file. New files stay at or under ${LIMIT} lines.`);
  process.exit(1);
}
console.log(`PASS: ${current.size} source files within the size ratchet (${baseline.size} baselined)`);
