// Small shell and screen rules checked against the source: each was a bug
// where the rendered result needs a browser to observe.

import { readdirSync, readFileSync } from "node:fs";
import path from "node:path";

import { describe, expect, it } from "vitest";

const src = (file: string) => readFileSync(path.resolve(__dirname, "../..", file), "utf8");

describe("shell and screen rules", () => {
  it("the discard dialog starts on Keep editing and uses the keyed words", () => {
    const s = src("components/shell/dirty-guard-dialog.tsx");
    expect(s).toContain('initialFocus="dismiss"');
    expect(s).toContain("cockpit.settingsSections.dirtyTitle");
    expect(s).not.toContain("Discard unsaved changes?");
    expect(src("components/ui/dialog.tsx")).toContain('initialFocus === "dismiss" && dismiss ? dismissRef : confirmRef');
  });

  it("the scaffold's tab pills are full touch targets", () => {
    const s = src("components/ui/console-scaffold.tsx");
    expect(s).not.toContain("min-h-[max(2.25rem,36px)]");
    expect(s).toMatch(/"touch-target flex shrink-0 items-center rounded-xl/);
  });

  it("the setup offer hides over the setup screen it opens", () => {
    expect(src("components/shell/setup-offer-banner.tsx")).toContain('activeScreenId(s) === "aircraft-setup"');
  });

  it("the menu keeps the focused entry in view", () => {
    expect(src("components/shell/menu-rail.tsx")).toContain("scrollIntoView");
  });

  it("a failed Wi-Fi scan does not also claim no networks were found", () => {
    expect(src("components/settings/sections/wifi-section.tsx")).toContain("!scan.scanning && !scan.failed");
  });

  it("stale reads dim the screen body in one place, and every stale badge also dims", () => {
    expect(src("components/ui/console-scaffold.tsx")).toMatch(/stale && "opacity-60 saturate-50"/);
    expect(src("components/ui/data.tsx")).toContain('t("cockpit.stale")');
    const dir = path.resolve(__dirname, "../screens");
    for (const f of readdirSync(dir).filter((n) => n.endsWith("-screen.tsx"))) {
      const s = readFileSync(path.join(dir, f), "utf8");
      const badges = /<StaleBadge/.test(s);
      const dims = /<ConsoleScaffold[^>]*\bstale=\{/.test(s);
      expect(dims || !badges, f).toBe(true);
    }
  });
});
