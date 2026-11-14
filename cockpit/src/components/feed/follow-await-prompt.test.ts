// @vitest-environment happy-dom
// The waiting-for-instructions prompt as it renders: shown only while
// follow-me holds for the operator, its words from the spec, each action wired
// to its transport by tap and by key (return home and land through their
// built-in's gate and hold-to-confirm, never straight to the pilot), and
// bigger touch targets in the kiosk layout.

import { createElement } from "react";
import { act } from "react-dom/test-utils";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type * as Skills from "@/lib/skills";

const { putPluginConfig, sendPilotAction, gate } = vi.hoisted(() => ({
  putPluginConfig: vi.fn<(...args: unknown[]) => Promise<unknown>>(() => Promise.resolve({})),
  sendPilotAction: vi.fn<(...args: unknown[]) => Promise<{ ok: boolean }>>(() => Promise.resolve({ ok: true })),
  // Open: every skill gate passes, as on an armed drone with a fresh link.
  // Closed: the real gates read this test's empty stores.
  gate: { open: true },
}));
vi.mock("@/lib/api", async (importOriginal) => ({ ...(await importOriginal<Record<string, unknown>>()), putPluginConfig }));
vi.mock("@/lib/pilot-link", async (importOriginal) => ({ ...(await importOriginal<Record<string, unknown>>()), sendPilotAction }));
vi.mock("@/lib/skills", async (importOriginal) => {
  const real = await importOriginal<typeof Skills>();
  return {
    ...real,
    resolveSkillState: (...args: Parameters<typeof real.resolveSkillState>) =>
      gate.open ? { enabled: true, active: false, reason: null } : real.resolveSkillState(...args),
  };
});
vi.mock("@/stores/voice-store", () => ({ speak: () => {} }));

import { FollowAwaitPrompt } from "@/components/feed/follow-await-prompt";
import { widthClassFor, type WidthClass } from "@/lib/cockpit-layout";
import { parseFollowState } from "@/shared/hud/nav-types";
import { specText } from "@/shared/hud/spec-text";
import { useConfirmStore } from "@/stores/confirm-store";
import { useNavStreamStore } from "@/stores/nav-stream-store";

Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });

const DESIGNATED = { cameraId: "main", trackId: 12, bbox: { x: 0, y: 0, width: 10, height: 20 } };

let host: HTMLDivElement;
let root: Root;

function setFollow(raw: Record<string, unknown>): void {
  useNavStreamStore.getState().ingest({ kind: "follow", data: parseFollowState(raw)! }, Date.now());
}

async function mount(widthClass: WidthClass): Promise<HTMLElement | null> {
  host = document.createElement("div");
  document.body.appendChild(host);
  root = createRoot(host);
  await act(async () => root.render(createElement(FollowAwaitPrompt, { widthClass })));
  return host.querySelector<HTMLElement>("[data-zone='follow_prompt']");
}

function button(label: string): HTMLButtonElement {
  const b = [...host.querySelectorAll("button")].find((el) => el.textContent?.includes(label));
  if (!b) throw new Error(`no button ${label}: ${host.textContent}`);
  return b;
}

const AWAITING = {
  phase: "holding-await-operator",
  hold_reason: "search-exhausted",
  reacquire_mode: "smart",
  operator_resume_seq: 2,
};

beforeEach(() => {
  useNavStreamStore.getState().clear();
  useNavStreamStore.getState().designate(DESIGNATED);
  putPluginConfig.mockClear();
  sendPilotAction.mockClear();
  useConfirmStore.getState().cancel();
  gate.open = true;
});

afterEach(async () => {
  await act(async () => root.unmount());
  host.remove();
});

describe("FollowAwaitPrompt", () => {
  it("shows nothing while follow-me is still searching", async () => {
    setFollow({ phase: "searching", reacquire_mode: "smart" });
    expect(await mount("medium")).toBeNull();
  });

  it("shows the title, the reason and every action while waiting", async () => {
    setFollow(AWAITING);
    const prompt = await mount("medium");
    expect(prompt?.getAttribute("role")).toBe("alert");
    const text = prompt?.textContent ?? "";
    expect(text).toContain(specText("follow.prompt.await_title"));
    expect(text).toContain(specText("follow.prompt.await_body.search_exhausted"));
    for (const a of ["resume", "redesignate", "rth", "land", "stop"]) expect(text).toContain(specText(`follow.prompt.action.${a}`));
  });

  it("offers no search-again in strict re-acquire", async () => {
    setFollow({ ...AWAITING, reacquire_mode: "strict" });
    const text = (await mount("medium"))?.textContent ?? "";
    expect(text).not.toContain(specText("follow.prompt.action.resume"));
    expect(text).toContain(specText("follow.prompt.action.redesignate"));
  });

  it("hides a stale report", async () => {
    useNavStreamStore.getState().ingest({ kind: "follow", data: parseFollowState(AWAITING)! }, Date.now() - 10_000);
    expect(await mount("medium")).toBeNull();
  });

  it("search again writes the next resume sequence", async () => {
    setFollow(AWAITING);
    await mount("medium");
    await act(async () => button(specText("follow.prompt.action.resume")).click());
    expect(putPluginConfig).toHaveBeenCalledWith("com.altnautica.follow-me", "operator_resume_seq", 3);
  });

  it("return home and land open their built-in's hold-to-confirm, never the bare pilot socket", async () => {
    setFollow(AWAITING);
    await mount("medium");
    await act(async () => button(specText("follow.prompt.action.rth")).click());
    expect(useConfirmStore.getState().pending?.skill.id).toBe("rth");
    expect(useConfirmStore.getState().pending?.skill.confirm).toEqual({ kind: "hold", ms: 1500 });
    await act(async () => useConfirmStore.getState().cancel());
    await act(async () => button(specText("follow.prompt.action.land")).click());
    expect(useConfirmStore.getState().pending?.skill.id).toBe("land");
    expect(sendPilotAction).not.toHaveBeenCalled();
  });

  it("return home and land stay behind their built-in's gate", async () => {
    // No telemetry and no pilot link: the built-ins' own gate refuses them.
    gate.open = false;
    setFollow(AWAITING);
    await mount("medium");
    await act(async () => button(specText("follow.prompt.action.rth")).click());
    await act(async () => button(specText("follow.prompt.action.land")).click());
    expect(useConfirmStore.getState().pending).toBeNull();
    expect(sendPilotAction).not.toHaveBeenCalled();
  });

  it("stop turns follow off", async () => {
    setFollow(AWAITING);
    await mount("medium");
    await act(async () => button(specText("follow.prompt.action.stop")).click());
    expect(putPluginConfig).toHaveBeenCalledWith("com.altnautica.follow-me", "active", false);
  });

  it("pick a target drops the designation so a new box can be picked", async () => {
    setFollow(AWAITING);
    await mount("medium");
    await act(async () => button(specText("follow.prompt.action.redesignate")).click());
    expect(useNavStreamStore.getState().designated).toBeNull();
  });

  it("a tapped button never keeps focus, so Space stays the Pause key", async () => {
    setFollow(AWAITING);
    await mount("medium");
    for (const a of ["resume", "redesignate", "rth", "land", "stop"]) {
      const down = new MouseEvent("mousedown", { bubbles: true, cancelable: true });
      button(specText(`follow.prompt.action.${a}`)).dispatchEvent(down);
      // The default action of a mousedown is to focus the button.
      expect(down.defaultPrevented).toBe(true);
    }
  });

  it("a button pressed from the keyboard hands focus to the prompt, so Space stays the Pause key", async () => {
    setFollow(AWAITING);
    const prompt = (await mount("medium"))!;
    const pick = button(specText("follow.prompt.action.redesignate"));
    await act(async () => pick.focus());
    await act(async () => pick.click());
    expect(useNavStreamStore.getState().designated).toBeNull();
    expect(document.activeElement).toBe(prompt);
    expect(document.activeElement?.tagName).not.toBe("BUTTON");
  });

  it("never takes focus on its own: with focus elsewhere its keys reach the window", async () => {
    const outside = document.createElement("button");
    document.body.appendChild(outside);
    outside.focus();
    setFollow(AWAITING);
    const prompt = (await mount("medium"))!;
    expect(document.activeElement).toBe(outside);
    expect(prompt.contains(document.activeElement)).toBe(false);
    const onWindow = vi.fn();
    window.addEventListener("keydown", onWindow);
    for (const key of ["r", "h", "x"]) {
      await act(async () => {
        (document.activeElement ?? document.body).dispatchEvent(new KeyboardEvent("keydown", { key, bubbles: true, cancelable: true }));
      });
    }
    expect(onWindow).toHaveBeenCalledTimes(3);
    expect(onWindow.mock.calls.every(([e]) => !(e as KeyboardEvent).defaultPrevented)).toBe(true);
    expect(putPluginConfig).not.toHaveBeenCalled();
    expect(useConfirmStore.getState().pending).toBeNull();
    window.removeEventListener("keydown", onWindow);
    outside.remove();
  });

  it("never takes focus on its own, even with nothing else focused", async () => {
    setFollow(AWAITING);
    const prompt = (await mount("medium"))!;
    expect(prompt.contains(document.activeElement)).toBe(false);
    const onWindow = vi.fn();
    window.addEventListener("keydown", onWindow);
    await act(async () => {
      (document.activeElement ?? document.body).dispatchEvent(new KeyboardEvent("keydown", { key: "r", bubbles: true }));
    });
    expect(onWindow).toHaveBeenCalledTimes(1);
    expect(putPluginConfig).not.toHaveBeenCalled();
    window.removeEventListener("keydown", onWindow);
  });

  it("once focused, H and L open the confirm and never fire at once", async () => {
    setFollow(AWAITING);
    const prompt = (await mount("medium"))!;
    await act(async () => prompt.focus());
    const onWindow = vi.fn();
    window.addEventListener("keydown", onWindow);
    await act(async () => {
      prompt.dispatchEvent(new KeyboardEvent("keydown", { key: "h", bubbles: true }));
    });
    expect(useConfirmStore.getState().pending?.skill.id).toBe("rth");
    // The confirm sheet takes Enter from the page: the prompt hands back the keyboard.
    expect(document.activeElement).not.toBe(prompt);
    await act(async () => useConfirmStore.getState().cancel());
    await act(async () => {
      prompt.dispatchEvent(new KeyboardEvent("keydown", { key: "l", bubbles: true }));
    });
    expect(useConfirmStore.getState().pending?.skill.id).toBe("land");
    expect(sendPilotAction).not.toHaveBeenCalled();
    expect(onWindow).not.toHaveBeenCalled();
    // A chord is not the prompt's: Shift+L stays the land built-in's.
    await act(async () => {
      prompt.dispatchEvent(new KeyboardEvent("keydown", { key: "L", shiftKey: true, bubbles: true }));
    });
    expect(onWindow).toHaveBeenCalledTimes(1);
    window.removeEventListener("keydown", onWindow);
  });

  it("renders in the kiosk layout with larger touch targets", async () => {
    setFollow(AWAITING);
    await mount(widthClassFor(800, false));
    const desk = parseFloat(button(specText("follow.prompt.action.land")).style.minHeight);
    await act(async () => root.unmount());
    host.remove();
    const kiosk = await mount(widthClassFor(800, true));
    expect(kiosk?.textContent).toContain(specText("follow.prompt.await_title"));
    const big = parseFloat(button(specText("follow.prompt.action.land")).style.minHeight);
    expect(desk).toBe(48);
    expect(big).toBe(64);
  });
});
