// The maps, the instruments, the target overlay and the vision panel draw
// their words from the cockpit spec (`specText`), never from literals of their
// own. The scan parses each file and collects every literal that reaches the
// screen or a screen reader: JSX text, a text attribute (aria-label, title,
// label, placeholder, alt), a JSX child expression, a canvas `fillText`, an
// object's `label`/`text`/`title`/`warn`/`scale` field and the values of a
// word table (a const named *WORD*, *LABEL* or *TEXT*). Spec keys and the
// arguments of `specText` are not words.

import { readFileSync } from "node:fs";
import path from "node:path";

import ts from "typescript";
import { describe, expect, it } from "vitest";

import { detectorNotice } from "@/lib/detection-lease";

const TEXT_ATTRS: Record<string, true> = { "aria-label": true, title: true, label: true, placeholder: true, alt: true };
const TEXT_FIELDS: Record<string, true> = { label: true, text: true, title: true, warn: true, scale: true };
const KEY_CALLS: Record<string, true> = { specText: true, hasSpecText: true, setupText: true, cn: true };
/** A literal holding a word: neither a spec key ("minimap.pin.auto") nor a
 *  class list ("bg-hud-glass text-hud-warn"). */
const isWords = (t: string) => /[A-Za-z]{2,}/.test(t) && !/^[a-z_]+(\.[A-Za-z0-9_]+)+$/.test(t) && !t.split(" ").every((tok) => tok.includes("-"));
const COMPARISONS: readonly ts.SyntaxKind[] = [
  ts.SyntaxKind.EqualsEqualsEqualsToken,
  ts.SyntaxKind.ExclamationEqualsEqualsToken,
  ts.SyntaxKind.EqualsEqualsToken,
  ts.SyntaxKind.ExclamationEqualsToken,
];

/** Whether a literal reaches the screen, judged by its nearest context. */
function drawn(node: ts.Node): boolean {
  for (let child: ts.Node = node, p = node.parent; p; child = p, p = p.parent) {
    if (ts.isCallExpression(p)) {
      const callee = p.expression;
      const name = ts.isIdentifier(callee) ? callee.text : ts.isPropertyAccessExpression(callee) ? callee.name.text : null;
      if (name && KEY_CALLS[name]) return false;
      if (name === "fillText" || name === "strokeText") return p.arguments[0] === child;
    }
    if (ts.isBinaryExpression(p) && COMPARISONS.includes(p.operatorToken.kind)) return false;
    if (ts.isCaseClause(p) || ts.isImportDeclaration(p) || ts.isTypeNode(p) || ts.isElementAccessExpression(p)) return false;
    if (ts.isJsxAttribute(p)) return TEXT_ATTRS[p.name.getText()] === true;
    if (ts.isJsxExpression(p) && (ts.isJsxElement(p.parent) || ts.isJsxFragment(p.parent))) return true;
    if (ts.isPropertyAssignment(p)) {
      if (TEXT_FIELDS[p.name.getText()]) return true;
      let decl: ts.Node = p.parent.parent;
      if (ts.isAsExpression(decl)) decl = decl.parent;
      return ts.isVariableDeclaration(decl) && /WORD|LABEL|TEXT/.test(decl.name.getText());
    }
    if (ts.isFunctionLike(p) || ts.isStatement(p)) return false;
  }
  return false;
}

/** Every drawn literal in a source text that holds a word. */
function drawnWordsIn(source: string): string[] {
  const sf = ts.createSourceFile("scan.tsx", source, ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
  const out: string[] = [];
  const visit = (n: ts.Node) => {
    if (ts.isJsxText(n)) {
      const t = n.text.trim();
      if (isWords(t)) out.push(t);
    } else if (ts.isStringLiteral(n) || ts.isNoSubstitutionTemplateLiteral(n)) {
      if (isWords(n.text) && drawn(n)) out.push(n.text);
    } else if (ts.isTemplateExpression(n)) {
      const parts = [n.head.text, ...n.templateSpans.map((s) => s.literal.text)];
      if (parts.some(isWords) && drawn(n)) out.push(parts.join("{}"));
    }
    ts.forEachChild(n, visit);
  };
  visit(sf);
  return out;
}

const drawnWords = (rel: string) => drawnWordsIn(readFileSync(path.resolve(__dirname, rel), "utf8"));

describe("map, instrument, target and vision text", () => {
  it("finds a literal in each drawn position", () => {
    const probe = [
      'const PIN_WORD = { auto: "AUTO" } as const;',
      'const k = specText("minimap.pin.auto", { pin: "word" });',
      'if (kind === "LOST") draw();',
      'ctx.fillText("Canvas words", 0, 0);',
      'const row = { label: "Field words", id: "memory" };',
      'const el = <div className="flex gap-[4px]" aria-label="Aria words" data-testid="probe">Text node{ok ? "Child words" : null}</div>;',
    ].join("\n");
    expect(drawnWordsIn(probe).sort()).toEqual(["AUTO", "Aria words", "Canvas words", "Child words", "Field words", "Text node"]);
  });

  it.each([
    "mini-map.tsx",
    "local-mini-map.tsx",
    "minimap-readout.tsx",
    "map-panel.tsx",
    "fly-here.tsx",
    "../../hooks/use-fly-here.tsx",
    "hud-canvas.tsx",
    "target-layer.tsx",
    "whats-locked-chip.tsx",
    "../../lib/detection-lease.ts",
  ])("%s draws no hard-coded words", (rel) => {
    expect(drawnWords(rel)).toEqual([]);
  });

  // Words the spec has no key for yet; each leaves this list when its key lands.
  it.each([
    ["layers/vision-panel.tsx", ["On while the", "view is open", "no backend", "{} fps", "idle"]],
    ["proximity-radar.tsx", ["no heading", "Radar", "Radar range {}; change range"]],
    ["reacquire-toggle.tsx", ["Re-acquire mode"]],
  ] as const)("%s draws no hard-coded words but the ones awaiting a spec key", (rel, awaiting) => {
    expect(drawnWords(rel)).toEqual(awaiting);
  });

  it("reads the detector notices from the spec", () => {
    expect(detectorNotice("idle")).toBe("Detector idle");
    expect(detectorNotice("unavailable")).toBe("Detector unavailable");
    expect(detectorNotice("detecting")).toBeNull();
  });
});
