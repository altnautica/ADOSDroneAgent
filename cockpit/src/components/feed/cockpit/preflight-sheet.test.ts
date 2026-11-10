import { createElement, type ReactNode } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { afterEach, describe, expect, it, vi } from "vitest";

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
