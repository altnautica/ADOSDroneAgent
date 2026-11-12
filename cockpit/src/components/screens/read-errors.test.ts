// A failed read says it failed and offers Retry; it never renders as an empty
// list, a definite "none", or made-up current values.

import { beforeEach, describe, expect, it, vi } from "vitest";

import i18n from "@/shared/i18n";

import { FAILED, answerReads, ok, renderScreen } from "./screen-test-kit";
import { PairScreen } from "./pair-screen";
import { SystemScreen } from "./system-screen";
import { UplinkScreen } from "./uplink-screen";
import { VideoScreen } from "./video-screen";

// vi.mock factories run before the static imports, so the kit is pulled in here.
vi.mock("@/shared/use-resource", async () => ({ useResource: (await import("./screen-test-kit")).fakeUseResource }));

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
    expect(html).toContain(en("cockpit.readFailed.wfbPair"));
    expect(html).not.toContain(en("cockpit.pair.noDrone"));
    expect(html).not.toContain(en("cockpit.pair.pairDrone"));
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
