import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { MemoryRouter } from "react-router-dom";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import "@/i18n";

import { CaptureView } from "@/components/panels/capture-panel";
import { LoraView } from "@/components/panels/lora-panel";
import { SensorsView } from "@/components/panels/sensors-panel";
import { ProfileGateView } from "@/components/profile-gate";
import { captureErrorCode, markCapture, startCapture, type CaptureState } from "@/lib/capture";
import {
  loraChips,
  meterTone,
  type CrsfModule,
  type LoraRegionRow,
  type LoraRule,
  type LoraStatus,
} from "@/lib/lora";
import { rateVerdict, type SensorsStatus } from "@/lib/sensors";
import { ROUTES } from "@/route-table";

const html = (el: Parameters<typeof renderToStaticMarkup>[0]) => renderToStaticMarkup(el);

function lora(over: Partial<LoraStatus> = {}): LoraStatus {
  return {
    present: true,
    transport: "e22",
    module_band: "850-930",
    region: { code: "IN", name: "India", plan_id: "IN865", source: "network.regulatory.region", band_mhz: [865, 867] },
    channel: { freq_mhz: 866, bw_khz: 125, sf: 9, erp_mw: 25, max_erp_mw: 25 },
    node_id: 17,
    tx_allowed: true,
    blocked_reason: null,
    governor: {
      rules: [
        { kind: "duty_cycle", window_s: 3600, limit_pct: 1, used_pct: 0.4, airtime_ms_window: 14400, budget_ms_window: 36000, exhausted: false, sub_band: "865.0-867.0" },
        { kind: "max_power", limit_mw: 25, configured_mw: 25, ok: true },
      ],
      next_tx_allowed_ms: 0,
      last_refusal: null,
    },
    tx: { frames: 1, refused: 2, refused_by_rule: { duty_cycle: 2 } },
    rx: { frames: 3, crc_errors: 0 },
    meshtastic: null,
    ...over,
  };
}

function renderLora(status: LoraStatus, extra: { armed?: boolean } = {}): string {
  return html(
    createElement(
      MemoryRouter,
      null,
      createElement(LoraView, {
        status,
        regions: [],
        beacons: [],
        country: "IN",
        nowMs: 1000,
        fetchedAtMs: 1000,
        onApplyRegion: async () => {},
        ...extra,
      }),
    ),
  );
}

function row(over: Partial<LoraRegionRow> = {}): LoraRegionRow {
  return {
    code: "EU",
    name: "Europe",
    plan_id: "EU868",
    band_mhz: [863, 870],
    rules: ["duty_cycle"],
    supported_by_module: true,
    meshtastic_preset: "EU_868",
    elrs_domains: { "2g4": "CE_LBT", "900": "EU868" },
    ...over,
  };
}

const euStatus = (over: Partial<LoraStatus> = {}) =>
  lora({
    region: { code: "EU", name: "Europe", plan_id: "EU868", source: "network.regulatory.region", band_mhz: [863, 870] },
    ...over,
  });

const elrs = (modules: CrsfModule[]) => loraChips(euStatus(), null, row(), modules).filter((c) => c.kind === "elrs");

describe("route table: node pages", () => {
  it("gates the new pages to their profiles", () => {
    const by = Object.fromEntries(ROUTES.map((r) => [r.path, r.profiles]));
    expect(by["/gnss"]).toEqual(["ground_station", "visor"]);
    expect(by["/lora"]).toEqual(["drone", "ground_station"]);
    expect(by["/capture"]).toEqual(["visor"]);
    expect(by["/sensors"]).toEqual(["drone", "visor"]);
    expect(by["/settings/capture"]).toEqual(["visor"]);
    expect(by["/settings/sensors"]).toEqual(["drone", "visor"]);
  });
});

describe("ProfileGateView", () => {
  const gate = (profile: Parameters<typeof ProfileGateView>[0]["profile"]) =>
    html(
      createElement(
        MemoryRouter,
        null,
        createElement(ProfileGateView, { profile, role: "direct", allow: ["visor"], children: "PAGE" }),
      ),
    );

  it("renders the page only for an allowed profile", () => {
    expect(gate("visor")).toContain("PAGE");
    for (const p of ["drone", "ground_station", "workstation", "compute"] as const) {
      const out = gate(p);
      expect(out).not.toContain("PAGE");
      expect(out).toContain("Not available on this profile");
    }
  });

  it("shows the neutral state for an unknown profile", () => {
    const out = gate("unknown");
    expect(out).not.toContain("PAGE");
    expect(out).toContain("Not available on this node");
  });
});

describe("/lora", () => {
  it("shows region, TX allowed and one meter per duty rule", () => {
    const out = renderLora(lora());
    expect(out).toContain("IN865");
    expect(out).toContain("TX ALLOWED");
    expect(out).toContain("next TX now");
    expect(out).toContain("14.4/36 s");
    expect(out).toContain("25/25 mW ERP");
    expect(out).toContain("duty 2");
  });

  it("blocks with region_unset", () => {
    const out = renderLora(lora({ region: null, tx_allowed: false, blocked_reason: "region_unset" }));
    expect(out).toContain("TX BLOCKED: region not set");
    expect(out).toContain("--");
  });

  it("offers the country setting when the region is unset", () => {
    const out = renderLora(lora({ region: null, tx_allowed: false, blocked_reason: "region_unset" }));
    expect(out).toContain("Open country setting");
    expect(out).toContain('href="/settings/region"');
  });

  it("shows the not-present state", () => {
    expect(renderLora(lora({ present: false }))).toContain("No LoRa module on this node.");
  });

  it("locks Apply while armed", () => {
    const armed = renderLora(lora(), { armed: true });
    expect(armed).toContain("Region changes are locked while armed.");
    expect(armed).toMatch(/<button[^>]*disabled=""[^>]*>Apply region/);
    expect(renderLora(lora())).not.toContain("locked while armed");
  });

  it("colours the duty meter by fraction of the limit", () => {
    expect(meterTone(0.69)).toBe("neutral");
    expect(meterTone(0.7)).toBe("warn");
    expect(meterTone(0.99)).toBe("warn");
    expect(meterTone(1)).toBe("error");
    const status = lora();
    const duty = status.governor!.rules[0] as Extract<LoraRule, { kind: "duty_cycle" }>;
    duty.airtime_ms_window = 30_000;
    expect(renderLora(status)).toContain('data-tone="warn"');
  });

  it("counts down a duty wait", () => {
    const status = lora({ tx_allowed: false, blocked_reason: "duty_exhausted" });
    status.governor!.next_tx_allowed_ms = 462_000;
    expect(renderLora(status)).toContain("TX PAUSED 07:42 (duty)");
  });
});

describe("loraChips", () => {
  it("flags an ELRS module whose domain differs from the region's", () => {
    expect(elrs([{ id: "m0", domain: "FCC915" }])).toEqual([
      { kind: "elrs", module: "m0", domain: "FCC915", code: "EU", expected: "EU868" },
    ]);
    expect(elrs([{ id: "m1", domain: "ISM2G4" }])[0]?.expected).toBe("CE_LBT");
  });

  it("is silent for a match, a null domain or an unknown domain", () => {
    expect(elrs([{ id: "m0", domain: "EU868" }, { id: "m1", domain: "CE_LBT" }])).toEqual([]);
    expect(elrs([{ id: "m0", domain: null }])).toEqual([]);
    expect(elrs([{ id: "m0", domain: "MYSTERY" }])).toEqual([]);
    expect(loraChips(euStatus(), null, row({ elrs_domains: { "2g4": null, "900": null } }), [
      { id: "m0", domain: "FCC915" },
    ])).toEqual([]);
  });

  it("flags a Meshtastic preset only for an enabled bridge", () => {
    const mesh = (enabled: boolean) =>
      loraChips(
        euStatus({
          region: { ...euStatus().region!, meshtastic_preset: "EU_868" },
          meshtastic: { enabled, connected: true, port: null, region_preset: "US" },
        }),
        null,
      );
    expect(mesh(false)).toEqual([]);
    expect(mesh(true)).toEqual([{ kind: "meshtastic", node: "US", expected: "EU_868" }]);
  });

  it("flags airborne-restricted regions", () => {
    const gb = lora({
      region: { code: "GB", name: "United Kingdom", plan_id: "EU868", source: "lora.region", band_mhz: [863, 870] },
    });
    expect(loraChips(gb, "GB")).toEqual([{ kind: "airborne", code: "GB" }]);
    expect(loraChips(euStatus(), null)).toEqual([]);
  });
});

describe("/sensors", () => {
  it("gives a rate verdict against the expected rate", () => {
    expect(rateVerdict({ rate_hz: 1000, expected_hz: 1000 })).toBe("ok");
    expect(rateVerdict({ rate_hz: 600, expected_hz: 1000 })).toBe("degraded");
    expect(rateVerdict({ rate_hz: 100, expected_hz: 1000 })).toBe("low");
    expect(rateVerdict({ rate_hz: 10, expected_hz: null })).toBe("none");
    const s: SensorsStatus = {
      present: true,
      rig_id: "r",
      calib_version: "c",
      hub: null,
      fc: null,
      gnss: null,
      sensors: [
        { id: "imu0", kind: "imu", role: "nav", source: "hub", rate_hz: 400, expected_hz: 1000, last_t_ns: 1, age_ms: 1, sigma_ns: 420000 },
      ],
    };
    const out = html(createElement(SensorsView, { sensors: s, clock: null }));
    expect(out).toContain("400.0/1000.0 Hz");
    expect(out).toContain("low");
  });
});

describe("/capture", () => {
  const fetchMock = vi.fn<(input: RequestInfo | URL, init?: RequestInit) => Promise<Response>>();
  const reply = (status: number, body: unknown) =>
    new Response(JSON.stringify(body), { status, headers: { "Content-Type": "application/json" } });

  beforeEach(() => {
    fetchMock.mockReset();
    vi.stubGlobal("fetch", fetchMock);
  });
  afterEach(() => vi.unstubAllGlobals());

  it("starts and marks through the plugin route", async () => {
    fetchMock.mockResolvedValueOnce(reply(200, { ok: true, session_id: "s1" }));
    fetchMock.mockResolvedValueOnce(reply(200, { ok: true, segment: 2 }));
    await startCapture({ room: "Dock", floor: 0 });
    await markCapture({ room: "Kitchen", floor: 1 });
    expect(fetchMock.mock.calls[0]![0]).toBe("/api/plugins/com.altnautica.world-engine/x/atlas/capture/start");
    expect(fetchMock.mock.calls[0]![1]!.method).toBe("POST");
    expect(fetchMock.mock.calls[1]![0]).toBe("/api/plugins/com.altnautica.world-engine/x/atlas/capture/mark");
    expect(JSON.parse(fetchMock.mock.calls[1]![1]!.body as string)).toEqual({ room: "Kitchen", floor: 1 });
  });

  it("shows E_STORAGE_FULL", async () => {
    fetchMock.mockResolvedValueOnce(reply(409, { error: "E_STORAGE_FULL" }));
    const code = await startCapture({ floor: 0 }).then(
      () => null,
      (err: unknown) => captureErrorCode(err),
    );
    expect(code).toBe("E_STORAGE_FULL");
    const state: CaptureState = {
      state: "idle",
      session_id: null,
      duration_s: 0,
      site: null,
      floor: 0,
      room: null,
      segment: 0,
      frames: null,
      health: [],
      exposure: null,
      storage: null,
    };
    const noop = () => {};
    const out = html(createElement(CaptureView, { state, errorCode: code, busy: false, onStart: noop, onStop: noop, onMark: noop }));
    expect(out).toContain("Storage is full");
  });
});
