// @vitest-environment happy-dom
// The waiting-for-instructions prompt as it renders: shown only while
// follow-me holds for the operator, its words from the spec, each action wired
// to its transport by tap and by key, and bigger touch targets in the kiosk
// layout.

import { createElement } from "react";
import { act } from "react-dom/test-utils";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const { putPluginConfig, sendPilotAction } = vi.hoisted(() => ({
  putPluginConfig: vi.fn<(...args: unknown[]) => Promise<unknown>>(() => Promise.resolve({})),
  sendPilotAction: vi.fn<(...args: unknown[]) => Promise<{ ok: boolean }>>(() => Promise.resolve({ ok: true })),
}));
vi.mock("@/lib/api", () => ({ putPluginConfig }));
vi.mock("@/lib/pilot-link", () => ({ sendPilotAction }));

import { FollowAwaitPrompt } from "@/components/feed/follow-await-prompt";
import { widthClassFor, type WidthClass } from "@/lib/cockpit-layout";
import { parseFollowState } from "@/shared/hud/nav-types";
import { specText } from "@/shared/hud/spec-text";
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

  it("return home, land and stop go to their transports", async () => {
    setFollow(AWAITING);
    await mount("medium");
    await act(async () => button(specText("follow.prompt.action.rth")).click());
    expect(sendPilotAction).toHaveBeenCalledWith("rth");
    await act(async () => button(specText("follow.prompt.action.land")).click());
    expect(sendPilotAction).toHaveBeenCalledWith("land");
    await act(async () => button(specText("follow.prompt.action.stop")).click());
    expect(putPluginConfig).toHaveBeenCalledWith("com.altnautica.follow-me", "active", false);
  });

  it("pick a target drops the designation so a new box can be picked", async () => {
    setFollow(AWAITING);
    await mount("medium");
    await act(async () => button(specText("follow.prompt.action.redesignate")).click());
    expect(useNavStreamStore.getState().designated).toBeNull();
  });

  it("takes focus and answers its keys without passing them on", async () => {
    setFollow(AWAITING);
    const prompt = (await mount("medium"))!;
    expect(document.activeElement).toBe(prompt);
    const onWindow = vi.fn();
    window.addEventListener("keydown", onWindow);
    await act(async () => {
      prompt.dispatchEvent(new KeyboardEvent("keydown", { key: "h", bubbles: true }));
    });
    expect(sendPilotAction).toHaveBeenCalledWith("rth");
    expect(onWindow).not.toHaveBeenCalled();
    // A chord is not the prompt's: Shift+L stays the land built-in's.
    await act(async () => {
      prompt.dispatchEvent(new KeyboardEvent("keydown", { key: "L", shiftKey: true, bubbles: true }));
    });
    expect(sendPilotAction).toHaveBeenCalledTimes(1);
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
