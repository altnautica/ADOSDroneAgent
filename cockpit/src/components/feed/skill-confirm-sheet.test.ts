// @vitest-environment happy-dom
// The skill confirm sheet as it renders: a builtin's sheet carries its spec
// body (`confirm.<id>.message`), and every word on it (hold label, hints,
// input hint, altitude stepper) comes from the spec.

import { createElement } from "react";
import { act } from "react-dom/test-utils";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, describe, expect, it, vi } from "vitest";

vi.mock("@/lib/skills", async (importOriginal) => ({
  ...(await importOriginal<Record<string, unknown>>()),
  resolveSkillState: () => ({ enabled: true, active: false, reason: null }),
}));
vi.mock("@/stores/voice-store", () => ({ speak: () => {} }));

import { SkillConfirmSheet } from "@/components/feed/skill-confirm-sheet";
import { BUILTIN_SKILL_BY_ID } from "@/lib/skills";
import { specText } from "@/shared/hud/spec-text";
import { useConfirmStore } from "@/stores/confirm-store";

Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });

let host: HTMLDivElement;
let root: Root;

async function open(id: string): Promise<string> {
  useConfirmStore.getState().open(BUILTIN_SKILL_BY_ID[id]);
  host = document.createElement("div");
  document.body.appendChild(host);
  root = createRoot(host);
  await act(async () => root.render(createElement(SkillConfirmSheet)));
  return document.body.textContent ?? "";
}

afterEach(async () => {
  await act(async () => root.unmount());
  host.remove();
  useConfirmStore.getState().cancel();
});

describe("the skill confirm sheet", () => {
  it.each(["rth", "land"])("%s draws its spec body", async (id) => {
    const text = await open(id);
    expect(document.querySelector('[data-testid="confirm-sheet-body"]')?.textContent).toBe(specText(`confirm.${id}.message`));
    expect(text).toContain(specText("confirm.hold_to_confirm", { s: "1.5" }));
    expect(text).toContain(specText("confirm.input_hint_panel"));
  });

  it("take-off names its altitude stepper with the launch sheet's words", async () => {
    await open("takeoff");
    expect(document.body.textContent).toContain(specText("flow.launch.alt_m"));
    expect(document.querySelector(`[aria-label="${specText("confirm.altitude_up")}"]`)).not.toBeNull();
    expect(document.querySelector(`[aria-label="${specText("confirm.altitude_down")}"]`)).not.toBeNull();
  });

  it("the STOP sheet draws its own body and the twice-long-press hint, never a skill body", async () => {
    const text = await open("kill");
    expect(document.querySelector('[data-testid="confirm-sheet-body"]')).toBeNull();
    expect(document.querySelector('[data-testid="stop-sheet-body"]')).not.toBeNull();
    expect(text).toContain(specText("stop.input_hint"));
  });
});
