// The status screens show wire enums as words, the radio pair copy names the
// node's real peer, and the System screen carries a pending reboot as a row
// any input can reach.

import { readFileSync } from "node:fs";
import path from "node:path";

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import i18n from "@/shared/i18n";
import type * as ProfileModule from "@/shared/use-profile";
import { parsePeripherals } from "@/lib/peripherals";
import type * as ConfigStoreModule from "@/stores/config-store";

import { answerReads, ok, renderScreen } from "./screen-test-kit";
import { LoraScreen } from "./lora-screen";
import { MeshScreen } from "./mesh-screen";
import { NodeScreen } from "./node-screen";
import { PairScreen } from "./pair-screen";
import { PeripheralsScreen } from "./peripherals-screen";
import { SensorsScreen } from "./sensors-screen";
import { SystemScreen } from "./system-screen";

// vi.mock factories run before the static imports, so the kit is pulled in here.
vi.mock("@/shared/use-resource", async () => ({ useResource: (await import("./screen-test-kit")).fakeUseResource }));
const profile = vi.hoisted(() => ({ now: "drone" }));
vi.mock("@/shared/use-profile", async (orig) => ({
  ...(await orig<typeof ProfileModule>()),
  useProfile: () => profile.now,
}));

// A server render reads a store's initial state, so the pending reboot list is
// fed through the store hook instead.
const reboot = vi.hoisted(() => ({ paths: [] as string[] }));
vi.mock("@/stores/config-store", async (orig) => {
  const real = await orig<typeof ConfigStoreModule>();
  const hook = (sel: (s: ReturnType<typeof real.useConfigStore.getState>) => unknown) =>
    sel({ ...real.useConfigStore.getState(), pendingRebootPaths: reboot.paths });
  return { ...real, useConfigStore: Object.assign(hook, real.useConfigStore) };
});

const en = (key: string, opts?: Record<string, unknown>) => i18n.t(key as never, opts as never) as unknown as string;
/** Text content of the markup, entities decoded. */
const text = (html: string) => html.replace(/<[^>]+>/g, " ").replace(/&#x27;/g, "'").replace(/&amp;/g, "&");

beforeEach(() => {
  profile.now = "drone";
  answerReads([]);
});

describe("Pair on a drone", () => {
  it("offers to pair with a ground station, never to bind a drone", () => {
    // info, rf (unpaired), pending, bind
    answerReads([ok(null), ok({ paired: false }), ok(null), ok(null)]);
    const html = text(renderScreen(PairScreen, {}));
    expect(html).toContain("Pair with a ground station");
    expect(html).toContain("No ground station is bound");
    expect(html).not.toMatch(/Bind a drone|No drone is bound/);
  });

  it("still binds a drone from a ground station", () => {
    profile.now = "ground_station";
    answerReads([ok(null), ok({ paired: false }), ok({ open: false }), ok(null)]);
    expect(text(renderScreen(PairScreen, {}))).toContain("Pair with a drone");
  });
});

describe("Pair bind cancel", () => {
  it("offers Cancel while the agent reports a waiting bind this tab did not start", () => {
    // info, rf (unpaired), pending, bind
    answerReads([ok(null), ok({ paired: false }), ok(null), ok({ state: "waiting_peer", phase_age_s: 12 })]);
    expect(text(renderScreen(PairScreen, {}))).toContain(en("cockpit.pair.bindCancel"));
  });

  it("offers no Cancel when no bind is waiting", () => {
    answerReads([ok(null), ok({ paired: false }), ok(null), ok({ state: "failed", error: "timeout" })]);
    expect(text(renderScreen(PairScreen, {}))).not.toContain(en("cockpit.pair.bindCancel"));
  });
});

describe("Sensors", () => {
  const status = {
    present: true,
    rig_id: null,
    calib_version: null,
    hub: { connected: true, transport: "serial:/dev/ttyACM0", fw: null, board: null, proto_ver: null, uptime_s: null, crc_errors: 0, dropped: 0 },
    fc: null,
    sensors: [{ id: "cam0", kind: "camera", role: "front_left", source: "vision", rate_hz: 30, expected_hz: null, last_t_ns: 0, age_ms: 5, sigma_ns: -1 }],
    gnss: { fix: "3d", sats: 12, hdop: 0.8, rtk: "none" },
  };

  it("names the stream kind, source, hub link and GNSS fix as words", () => {
    answerReads([ok(status), ok(null)]);
    const html = text(renderScreen(SensorsScreen, {}));
    expect(html).toContain(`${en("cockpit.sensors.kind.camera")} · cam0`);
    expect(html).toContain(en("cockpit.sensors.source.vision"));
    expect(html).toContain(en("cockpit.sensors.hubSerial", { target: "/dev/ttyACM0" }));
    expect(html).toContain(en("cockpit.gnss.fixLabel.3d"));
    expect(html).not.toContain("cam0 · camera");
    expect(html).not.toContain("serial:/dev");
  });

  it("reads a kind, source or link outside the set as unrecognised, never raw", () => {
    const odd = {
      ...status,
      hub: { ...status.hub, transport: "spi:0" },
      sensors: [{ ...status.sensors[0], kind: "radar", source: "ether" }],
    };
    answerReads([ok(odd), ok(null)]);
    const html = text(renderScreen(SensorsScreen, {}));
    expect(html).toContain(en("panels.state.unrecognised"));
    expect(html).not.toMatch(/radar|ether|spi:0/);
  });

  it("points to Aircraft setup for calibration on a drone", () => {
    answerReads([ok(status), ok(null)]);
    expect(text(renderScreen(SensorsScreen, {}))).toContain(en("cockpit.sensors.calibrateOpen"));
  });
});

describe("Peripherals", () => {
  it("shows the registry state as a word and no manifest id", () => {
    const rows = parsePeripherals({ peripherals: [{ id: "com.example.cam", name: "Gimbal camera", state: "NOT_DETECTED" }] });
    answerReads([ok(rows)]);
    const html = text(renderScreen(PeripheralsScreen, {}));
    expect(html).toContain(en("cockpit.peripherals.state.notDetected"));
    expect(html).not.toContain("NOT_DETECTED");
    expect(html).not.toContain("com.example.cam");
  });
});

describe("Node", () => {
  it("names the job state", () => {
    // status, jobs, sessions
    answerReads([
      ok({ role: null, queueDepth: 0, activeJobs: 0, workersIdle: 1, activeSessions: 0, clusterSlaves: 0, sessionStates: null }),
      ok([{ id: "j1", kind: "splat", state: "failed", progress: null, error: null, sessionId: null, updatedMs: null }]),
      ok([]),
    ]);
    const html = text(renderScreen(NodeScreen, {}));
    expect(html).toContain(en("cockpit.node.jobState.failed"));
    expect(html).not.toMatch(/\bfailed\b/);
  });
});

describe("Mesh", () => {
  it("names the mesh role", () => {
    answerReads([ok({ up: true, role: "relay", neighbors: [], gateways: [] })]);
    const html = text(renderScreen(MeshScreen, {}));
    expect(html).toContain(en("access.profileGate.roleRelay"));
    expect(html).not.toMatch(/\brelay\b/);
  });

  it("reads a role outside the set as Unrecognised", () => {
    answerReads([ok({ up: true, role: "satellite", neighbors: [], gateways: [] })]);
    const html = text(renderScreen(MeshScreen, {}));
    expect(html).toContain("Unrecognised");
    expect(html).not.toMatch(/satellite|\bunrecognised\b/);
  });
});

describe("LoRa", () => {
  it("heads the region by name and names its source and radio", () => {
    const region = { code: "EU868", name: "Europe 868 MHz", plan_id: "eu868-g1", source: "lora.region", band_mhz: [863, 870] };
    const status = { present: true, region, transport: "e22", module_band: "868", channel: null, chips: [], rules: [], tx: null };
    // lora, regions, beacons, crsf, config, armed
    answerReads([ok({ status, at: Date.now() }), ok({ regions: [] }), ok({ beacons: [] }), ok({ modules: [] }), ok(null), ok(null)]);
    const html = text(renderScreen(LoraScreen, {}));
    expect(html).toContain("Europe 868 MHz");
    expect(html).toContain(en("cockpit.lora.source.lora"));
    expect(html).toContain(en("cockpit.lora.transport.e22"));
    expect(html).not.toContain("(lora.region)");
  });
});

describe("System", () => {
  afterEach(() => {
    reboot.paths = [];
  });

  it("names each service state and never shows a raw systemd word", () => {
    // sys, diag, services, recording
    answerReads([
      ok(null),
      ok(null),
      ok({
        systemdAvailable: true,
        services: [
          { name: "ados-video", state: "active", sub_state: "running" },
          { name: "ados-mesh", state: "warp", sub_state: "dead" },
        ],
      }),
      ok(null),
    ]);
    const html = text(renderScreen(SystemScreen, { dispatch: () => {} }));
    expect(html).toContain(en("panels.state.service.active"));
    expect(html).toContain(en("panels.state.unrecognised"));
    expect(html).not.toMatch(/\b(warp|running|dead|active)\b/);
  });

  it("carries a pending reboot as a Reboot to apply row", () => {
    reboot.paths = ["video.camera.source"];
    answerReads([ok(null), ok(null), ok(null), ok(null)]);
    const html = text(renderScreen(SystemScreen, { dispatch: () => {} }));
    expect(html).toContain(en("cockpit.system.rebootToApply"));
    expect(html).toContain(en("cockpit.system.rebootPending", { paths: "Source" }));
  });
});

describe("cloud unpair refusal words", () => {
  it("names the unpair, not a radio change", () => {
    expect(i18n.t("cockpit.pair.unpairArmed")).toContain("Unpairing waits");
    expect(i18n.t("cockpit.pair.unpairArmedUnknown")).toContain("Unpairing waits");
    const src = readFileSync(path.resolve(__dirname, "pair-screen.tsx"), "utf8");
    const cloud = src.slice(src.indexOf("const unpairOwner"), src.indexOf("return (", src.indexOf("const unpairOwner")));
    expect(cloud).toContain("cockpit.pair.unpairArmed");
    expect(cloud).not.toContain("cockpit.radio.");
  });
});
