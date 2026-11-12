// A failed read says it failed and offers Retry; it never renders as an empty
// list, a definite "none", or made-up current values.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import i18n from "@/shared/i18n";
import type * as ProfileModule from "@/shared/use-profile";

import { FAILED, answerReads, ok, renderScreen } from "./screen-test-kit";
import { MeshScreen } from "./mesh-screen";
import { PairScreen } from "./pair-screen";
import { SensorsScreen } from "./sensors-screen";
import { SystemScreen } from "./system-screen";
import { UplinkScreen } from "./uplink-screen";
import { VideoScreen } from "./video-screen";

// vi.mock factories run before the static imports, so the kit is pulled in here.
vi.mock("@/shared/use-resource", async () => ({ useResource: (await import("./screen-test-kit")).fakeUseResource }));

const profile = vi.hoisted(() => ({ now: null as string | null }));
vi.mock("@/shared/use-profile", async (orig) => ({
  ...(await orig<typeof ProfileModule>()),
  useProfile: () => profile.now,
}));

const en = (key: string, opts?: Record<string, unknown>) => i18n.t(key as never, opts as never) as unknown as string;

describe("Video", () => {
  beforeEach(() => answerReads([]));

  it("shows a read error instead of made-up tuning values when the link config read fails", () => {
    // cameras, config, latency, armed
    answerReads([ok([]), FAILED, ok(null), ok(null)]);
    const html = renderScreen(VideoScreen, { initialTab: 1 });
    expect(html).toContain(en("cockpit.readFailed.videoConfig"));
    expect(html).toContain(en("common.retry"));
    expect(html).not.toContain(en("wfb.tuning.presetBalanced"));
    expect(html).not.toContain(en("cockpit.video.slideApply"));
  });

  it("never says no cameras were detected when the camera read fails", () => {
    answerReads([FAILED, ok({}), ok(null), ok(null)]);
    const html = renderScreen(VideoScreen, {});
    expect(html).not.toContain(en("video.route.noCameras"));
    expect(html).toContain(en("cockpit.readFailed.cameras"));
  });
});

describe("Pair", () => {
  it("never reads a failed radio pairing read as unpaired, and hides the bind button", () => {
    // info, rf, pending, bind
    answerReads([ok(null), FAILED, ok(null), ok(null)]);
    const html = renderScreen(PairScreen, {});
    // This node is a drone, so its radio peer is a ground station.
    const peer = en("pairing.subject.groundStation");
    expect(html).toContain(en("cockpit.readFailed.wfbPair"));
    expect(html).not.toContain(en("cockpit.pair.noDrone", { peer }));
    expect(html).not.toContain(en("cockpit.pair.pairDrone", { peer }));
  });

  it("never reads a failed owner read as loading", () => {
    answerReads([FAILED, ok({ paired: false }), ok(null), ok(null)]);
    const html = renderScreen(PairScreen, {});
    expect(html).toContain(en("cockpit.readFailed.pairing"));
    expect(html).not.toContain(en("pairing.page.loading"));
  });
});

describe("Uplink", () => {
  it("never reads a failed lane read as no uplink", () => {
    answerReads([FAILED, ok(null)]);
    const html = renderScreen(UplinkScreen, {});
    expect(html).toContain(en("uplink.matrix.loadFailed"));
    expect(html).not.toContain(en("uplink.active.none"));
    expect(html).not.toContain("no uplink");
  });
});

describe("System", () => {
  it("never says no services were reported when the services read fails", () => {
    // sys, diag, services, recording
    answerReads([ok(null), ok(null), FAILED, ok(null)]);
    const html = renderScreen(SystemScreen, { dispatch: () => {} });
    expect(html).toContain(en("cockpit.readFailed.services"));
    expect(html).not.toContain(en("cockpit.system.noServices"));
  });
});

/** Text content of the markup. */
const text = (html: string) => html.replace(/<[^>]+>/g, " ").replace(/\s+/g, " ");

describe("Video partial config", () => {
  const partial = {
    encoder: { codec: "h264", width: null, height: null, fps: null, bitrateKbps: null },
    radio: { channel: 149, band: null, mcs: null, fecK: null, fecN: null, txPowerDbm: null, preset: null },
    adaptive: null,
  };

  it("never offers made-up current tuning values for fields the agent did not report", () => {
    // cameras, config, latency, armed
    answerReads([ok([]), ok(partial), ok(null), ok(null)]);
    const html = text(renderScreen(VideoScreen, { initialTab: 1 }));
    expect(html).not.toContain(en("wfb.tuning.presetBalanced"));
    expect(html).not.toContain(en("wfb.tuning.fecOption", { k: 8, n: 12, redundancy: 50 }));
    expect(html).toContain(en("cockpit.video.notReported"));
  });

  it("labels the adaptive switch as adaptive bitrate on the status tab", () => {
    answerReads([ok([]), ok({ ...partial, adaptive: true }), ok(null), ok(null)]);
    const html = text(renderScreen(VideoScreen, {}));
    expect(html).toContain(en("video.fields.adaptiveBitrate"));
    expect(html).not.toContain(en("video.fields.adaptiveFec"));
  });
});

describe("Sensors", () => {
  it("says the clock read failed rather than not configured", () => {
    const status = { present: true, rig_id: null, calib_version: null, hub: null, fc: null, sensors: [], gnss: null };
    // sensors, clock
    answerReads([ok(status), FAILED]);
    const html = text(renderScreen(SensorsScreen, {}));
    expect(html).toContain(en("cockpit.readFailed.clock"));
    expect(html).not.toContain(`${en("sensors.clock")} ${en("sensors.notConfigured")}`);
  });
});

describe("Mesh", () => {
  it("shows a failed mesh read as an error with Retry, and no definite mesh capability", () => {
    answerReads([FAILED]);
    const html = text(renderScreen(MeshScreen, {}));
    expect(html).toContain(en("common.retry"));
    expect(html).not.toContain(`${en("cockpit.mesh.capable")} ${en("cockpit.mesh.no")}`);
  });
});

describe("System recorder", () => {
  afterEach(() => {
    profile.now = null;
  });

  it("never reads a failed recorder read as idle", () => {
    profile.now = "ground_station";
    // sys, diag, services, recording
    answerReads([ok(null), ok(null), ok(null), FAILED]);
    const html = text(renderScreen(SystemScreen, { dispatch: () => {} }));
    expect(html).toContain(en("cockpit.readFailed.recording"));
    expect(html).not.toContain(en("cockpit.system.recordingIdle"));
    expect(html).not.toMatch(new RegExp(`\\b${en("cockpit.system.record")}\\b`));
  });
});
