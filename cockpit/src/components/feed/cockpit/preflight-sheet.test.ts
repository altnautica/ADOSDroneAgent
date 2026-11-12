import { readFileSync } from "node:fs";
import path from "node:path";

import { createElement, type ReactNode } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { afterEach, describe, expect, it, vi } from "vitest";

import { preflightTally } from "@/hooks/use-preflight-items";
import { checklistPassable } from "@/lib/cockpit-flow";
import { demoPilotData } from "@/lib/mock-layers";
import { parsePilotState } from "@/shared/hud/cockpit-frames";
import { specText } from "@/shared/hud/spec-text";
import { useNavStreamStore, type NavStreamState } from "@/stores/nav-stream-store";

import { pilotReadinessItems } from "./preflight-items";
import { PreflightSheet } from "./preflight-sheet";

vi.mock("@/components/ui/sheet", () => ({
  Sheet: ({ children, footer }: { children: ReactNode; footer?: ReactNode }) => createElement("div", null, children, footer),
}));

const NOW = 1_000_000;
const ready = parsePilotState({
  ...demoPilotData(0),
  readiness: [{ id: "sticks", ok: true, severity: "block", msg: "Sticks centred" }],
})!;
const quietReady = parsePilotState({ ...demoPilotData(0), readiness: [] })!;

describe("the pilot's pre-flight readiness", () => {
  it("passes on a fresh frame that reports ready", () => {
    const items = pilotReadinessItems({ data: ready, at: NOW - 200 }, NOW);
    expect(items).toEqual([{ id: "pilot:sticks", label: "Sticks centred", state: "pass" }]);
    expect(checklistPassable(items)).toBe(true);
  });

  it("never passes on a 2.5 s old frame: every item reads not reported and the first arm stays blocked", () => {
    const items = pilotReadinessItems({ data: ready, at: NOW - 2500 }, NOW);
    expect(items).toEqual([
      { id: "pilot:sticks", label: specText("flow.preflight.not_reported", { item: "Sticks centred" }), state: "fail" },
    ]);
    expect(checklistPassable(items)).toBe(false);
    // A stale frame that listed nothing still blocks.
    expect(checklistPassable(pilotReadinessItems({ data: quietReady, at: NOW - 2500 }, NOW))).toBe(false);
  });

  it("adds nothing on a node with no pilot", () => {
    expect(pilotReadinessItems(null, NOW)).toEqual([]);
  });
});

/** A server render reads a store's initial state, so the pilot goes there. */
function serverPilot(pilot: NavStreamState["pilot"]): void {
  Object.assign(useNavStreamStore.getInitialState(), { pilot });
}

describe("the pre-flight sheet on a stale pilot frame", () => {
  afterEach(() => serverPilot(null));

  const render = (ageMs: number) => {
    serverPilot({ data: ready, at: Date.now() - ageMs });
    return renderToStaticMarkup(createElement(PreflightSheet));
  };

  it("reads the pilot item as passing while fresh and as not reported, failing, on a 2.5 s old frame", () => {
    /** The icon label of the row that reads `label`. */
    const rowState = (html: string, label: string) =>
      html.match(new RegExp(`aria-label="(\\w+)">(?:(?!</li>).)*<span>${label.replace(/[()]/g, "\\$&")}</span>`))?.[1];
    expect(rowState(render(200), "Sticks centred")).toBe("Pass");
    const stale = render(2500);
    expect(rowState(stale, specText("flow.preflight.not_reported", { item: "Sticks centred" }))).toBe("Fail");
    expect(stale).not.toContain("<span>Sticks centred</span>");
  });
});

describe("the pre-flight sheet's words", () => {
  it("names each row's icon with the spec's result words", () => {
    const src = readFileSync(path.resolve(__dirname, "preflight-sheet.tsx"), "utf8");
    for (const w of ['aria-label="Pass"', 'aria-label="Warning"', 'aria-label="Fail"']) expect(src, w).not.toContain(w);
    for (const k of ["setup.result.pass", "setup.result.warn", "setup.result.fail"]) expect(src, k).toContain(`specText("${k}")`);
  });

  it("a failing check reads as its failure, never as the passed wording", () => {
    const html = renderToStaticMarkup(createElement(PreflightSheet));
    // No telemetry: link, GNSS, battery and home all fail.
    expect(html).toContain("<span>Home position not set</span>");
    expect(html).not.toContain("<span>Home position set</span>");
    expect(html).toContain(`aria-label="${specText("setup.result.fail")}"`);
  });
});

describe("the Menu's pre-flight tally", () => {
  it("counts the passed items and carries the worst state", () => {
    expect(preflightTally([{ id: "a", label: "", state: "pass" }, { id: "b", label: "", state: "warn" }])).toEqual({ passed: 1, total: 2, state: "warn" });
    expect(preflightTally([{ id: "a", label: "", state: "fail" }, { id: "b", label: "", state: "warn" }])).toEqual({ passed: 0, total: 2, state: "fail" });
    expect(preflightTally([{ id: "a", label: "", state: "pass" }])).toEqual({ passed: 1, total: 1, state: "pass" });
  });
});
