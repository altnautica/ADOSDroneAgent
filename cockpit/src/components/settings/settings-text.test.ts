// The Settings screens, the shell chrome around them and the Link/Uplink
// screens draw their words from locale keys, never from English literals. The
// scan parses each file and collects JSX text and the text attributes that
// reach the screen or a screen reader.

import { readFileSync } from "node:fs";
import path from "node:path";

import ts from "typescript";
import { describe, expect, it } from "vitest";

const FILES = [
  "components/screens/settings-screen.tsx",
  "components/screens/aircraft-setup-screen.tsx",
  "components/screens/link-screen.tsx",
  "components/screens/uplink-screen.tsx",
  "components/settings/leaf-editor.tsx",
  "components/settings/settings-rows.tsx",
  "components/settings/appearance-settings.tsx",
  "components/settings/camera-source-picker.tsx",
  "components/settings/controls/controls-settings.tsx",
  "components/settings/controls/pad-drawing-view.tsx",
  "components/shell/dirty-guard-dialog.tsx",
  "components/shell/reach-notice.tsx",
  "components/shell/reboot-banner.tsx",
  "components/shell/setup-offer-banner.tsx",
  "components/shell/menu-rail.tsx",
];

const TEXT_ATTRS: Record<string, true> = {
  "aria-label": true, title: true, label: true, placeholder: true, alt: true, summary: true, confirmLabel: true, guardLabel: true,
};
const isWords = (s: string) => /[A-Za-z]{2,}/.test(s);

function literalsIn(source: string): string[] {
  const sf = ts.createSourceFile("scan.tsx", source, ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
  const out: string[] = [];
  const visit = (n: ts.Node) => {
    if (ts.isJsxText(n) && isWords(n.text)) out.push(n.text.trim());
    if (ts.isJsxAttribute(n) && TEXT_ATTRS[n.name.getText()] && n.initializer && ts.isStringLiteral(n.initializer) && isWords(n.initializer.text)) {
      out.push(`${n.name.getText()}="${n.initializer.text}"`);
    }
    if (ts.isJsxExpression(n) && n.expression && ts.isStringLiteral(n.expression) && isWords(n.expression.text)) {
      out.push(n.expression.text);
    }
    n.forEachChild(visit);
  };
  visit(sf);
  return out;
}

describe("settings and shell text", () => {
  it("the scan sees a literal", () => {
    expect(literalsIn('const a = <div title="Theme">Glove mode</div>;').sort()).toEqual(["Glove mode", 'title="Theme"']);
  });

  for (const file of FILES) {
    it(`${file} has no English literals`, () => {
      const src = readFileSync(path.resolve(__dirname, "../..", file), "utf8");
      expect(literalsIn(src)).toEqual([]);
    });
  }
});
