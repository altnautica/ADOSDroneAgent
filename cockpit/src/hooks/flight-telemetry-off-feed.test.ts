// Off the Feed the arm state comes from the shell's flight-telemetry poll, so
// the setup offer and the draft guard see a known disarmed vehicle on every
// screen, and an unknown one still counts as possibly airborne.

import { readFileSync } from "node:fs";
import path from "node:path";

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { startFlightTelemetryPoll } from "@/hooks/use-flight-telemetry";
import { setupOfferUid } from "@/lib/aircraft-setup";
import { liveArmed, tourAirborne } from "@/lib/controller-tour";
import type { VehicleState } from "@/lib/types";
import { TAB_ORDER } from "@/nav/screen-table";
import { useDirtyStore } from "@/stores/dirty-store";
import { useFlightStore } from "@/stores/flight-store";
import { useNavStore } from "@/stores/nav-store";
import { useNavStreamStore } from "@/stores/nav-stream-store";

let reply: VehicleState | Error = new Error("no answer");
// Hoisted above the imports by vitest.
vi.mock("@/lib/api", () => ({
  getTelemetry: () => (reply instanceof Error ? Promise.reject(reply) : Promise.resolve(reply)),
}));


const read = (rel: string) => readFileSync(path.resolve(__dirname, rel), "utf8");

const DISARMED: VehicleState = { armed: false, attitude: { roll: 0, pitch: 0, yaw: 0 }, last_update: "t1" } as VehicleState;
const NEEDS_SETUP = {
  uid: "fc-1",
  firmware: null,
  firmware_version: null,
  vehicle_class: null,
  running: null,
  wizards: [{ id: "ground", state: "not_started", available: true, blockers: [], completed_at: null, steps: [] }],
  report: null,
} as unknown as Parameters<typeof setupOfferUid>[0];

async function pollOnce(): Promise<() => void> {
  const stop = startFlightTelemetryPoll();
  await vi.waitFor(() => expect(useFlightStore.getState().telemetry !== null || useFlightStore.getState().stale).toBe(true));
  return stop;
}

describe("arm state off the Feed", () => {
  let stop: (() => void) | null = null;
  beforeEach(() => {
    useNavStore.setState({ activeTabId: "video", detailStack: [], tabChosen: true, visibleTabs: [...TAB_ORDER] });
    useDirtyStore.setState({ owners: ["video-tuning"], pending: null });
    useNavStreamStore.setState({ pilot: null });
  });
  afterEach(() => {
    stop?.();
    stop = null;
    useDirtyStore.setState({ owners: [], pending: null });
    useFlightStore.setState({ telemetry: null, live: false, stale: false });
  });

  it("is polled by the shell, not only while the Feed is mounted", () => {
    expect(read("../components/shell/cockpit-shell.tsx")).toContain("useFlightTelemetryPoll()");
    expect(read("../components/screens/feed-screen.tsx")).not.toContain("useFlightTelemetryPoll()");
  });

  it("lets the setup offer show on a known disarmed vehicle", async () => {
    reply = DISARMED;
    stop = await pollOnce();
    expect(setupOfferUid(NEEDS_SETUP, [], tourAirborne(null, liveArmed(useFlightStore.getState())))).toBe("fc-1");
  });

  it("asks before dropping a draft on the way to the Feed when known disarmed", async () => {
    reply = DISARMED;
    stop = await pollOnce();
    useNavStore.getState().goTab("feed");
    expect(useNavStore.getState().activeTabId).toBe("video");
    expect(useDirtyStore.getState().pending).not.toBeNull();
  });

  it("still treats an unanswered poll as possibly airborne", async () => {
    reply = new Error("down");
    stop = await pollOnce();
    expect(setupOfferUid(NEEDS_SETUP, [], tourAirborne(null, liveArmed(useFlightStore.getState())))).toBeNull();
    useNavStore.getState().goTab("feed");
    expect(useNavStore.getState().activeTabId).toBe("feed");
  });

  it("withdraws the setup offer once a later poll goes unanswered", async () => {
    reply = DISARMED;
    stop = await pollOnce();
    reply = new Error("down");
    await vi.waitFor(() => expect(useFlightStore.getState().stale).toBe(true), { timeout: 3000 });
    // The disarmed snapshot is still kept, but it is no longer a reading.
    expect(useFlightStore.getState().telemetry?.armed).toBe(false);
    expect(liveArmed(useFlightStore.getState())).toBeNull();
    expect(setupOfferUid(NEEDS_SETUP, [], tourAirborne(null, liveArmed(useFlightStore.getState())))).toBeNull();
  });
});
