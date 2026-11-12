// Forgetting a paired Bluetooth controller can drop the RC input, so the
// Bluetooth tab holds Forget back while the vehicle is armed or its arm state
// is unknown, as it does Make primary.

import { describe, expect, it, vi } from "vitest";

import i18n from "@/shared/i18n";
import type { Resource } from "@/shared/use-resource";

import { FAILED, answerReads, ok, renderScreen } from "./screen-test-kit";
import { IoScreen } from "./io-screen";

// vi.mock factories run before the static imports, so the kit is pulled in here.
vi.mock("@/shared/use-resource", async () => ({ useResource: (await import("./screen-test-kit")).fakeUseResource }));

const en = (key: string) => i18n.t(key as never) as unknown as string;

const PAIRED = [{ mac: "AA:BB:CC:DD:EE:FF", name: "Pad", type: "gamepad", connected: true }];

/** The Forget button's opening tag in the Bluetooth tab. */
function forgetButton(armed: Partial<Resource<unknown>>): string {
  // gamepads, display, paired, armed
  answerReads([ok(null), ok(null), ok(PAIRED), armed]);
  const html = renderScreen(IoScreen, { initialTab: 2 });
  const button = html.split("<button").find((b) => b.includes(en("cockpit.io.forget")));
  expect(button).toBeDefined();
  return button!.slice(0, button!.indexOf(">"));
}

describe("IO Bluetooth Forget", () => {
  it("is held back while the vehicle is armed", () => {
    expect(forgetButton(ok({ connected: true, armed: true }))).toContain("disabled=\"\"");
  });

  it("is held back while the arm state is unknown or unread", () => {
    expect(forgetButton(ok({ connected: true, armed: null }))).toContain("disabled=\"\"");
    expect(forgetButton(FAILED)).toContain("disabled=\"\"");
  });

  it("may fire on a known disarmed vehicle", () => {
    expect(forgetButton(ok({ connected: true, armed: false }))).not.toContain("disabled=\"\"");
  });

  it("says why it is held back", () => {
    answerReads([ok(null), ok(null), ok(PAIRED), ok({ connected: true, armed: true })]);
    expect(renderScreen(IoScreen, { initialTab: 2 })).toContain(en("cockpit.radio.armed"));
  });
});
