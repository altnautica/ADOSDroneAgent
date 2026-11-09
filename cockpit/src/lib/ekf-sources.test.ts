import { beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("@/lib/fc-config", () => ({ getParams: vi.fn() }));

import { getParams, type ParamsResponse } from "@/lib/fc-config";
import { EKF_SOURCE_PARAMS, ekfFlagWords, loadEkfSourceParams, navSourceInput, sourceWord } from "@/lib/ekf-sources";
import type { VehicleState } from "@/lib/types";
import { useNavStreamStore } from "@/stores/nav-stream-store";

const telemetry = (t: Record<string, unknown>) => t as unknown as VehicleState;
const cache = (params: Record<string, number>): ParamsResponse => ({ params, count: 3, cached: 3 });

describe("ekf-sources", () => {
  beforeEach(() => {
    useNavStreamStore.getState().clear();
    vi.mocked(getParams).mockReset();
  });

  it("lists the nine parameters set-major", () => {
    expect(EKF_SOURCE_PARAMS).toHaveLength(9);
    expect(EKF_SOURCE_PARAMS.slice(0, 3)).toEqual(["EK3_SRC1_POSXY", "EK3_SRC1_VELXY", "EK3_SRC1_YAW"]);
    expect(EKF_SOURCE_PARAMS[8]).toBe("EK3_SRC3_YAW");
  });

  it("decodes source values per axis", () => {
    expect(sourceWord("POSXY", 6)).toBe("ExternalNav");
    expect(sourceWord("VELXY", 5)).toBe("OpticalFlow");
    expect(sourceWord("YAW", 1)).toBe("Compass");
    expect(sourceWord("YAW", 6)).toBe("ExternalNav");
    expect(sourceWord("POSXY", 99)).toBe("99");
    expect(sourceWord("POSXY", undefined)).toBe("—");
  });

  it("words the EKF flags", () => {
    expect(ekfFlagWords(null)).toBe("—");
    expect(ekfFlagWords(0)).toBe("none");
    expect(ekfFlagWords(2 | 16 | 1024)).toBe("VEL H · POS H ABS · GPS GLITCH");
  });

  it("assembles the chip input; an EKF report counts only while live", () => {
    const t = telemetry({ ekf: { flags: 18, age_ms: 120 }, gps: { fix_type: 3, satellites: 14, eph: 0.9 } });
    expect(navSourceInput(t, true, null, 2, { EK3_SRC2_POSXY: 6 })).toEqual({
      ekf: { flags: 18, ageMs: 120 },
      gps: { fixType: 3, satellites: 14, hdop: 0.9 },
      nav: null,
      lastCommandedSet: 2,
      params: { EK3_SRC2_POSXY: 6 },
    });
    const dead = navSourceInput(t, false, null, null, {});
    expect(dead.ekf).toBeNull();
    expect(dead.params).toBeNull();
    expect(navSourceInput(telemetry({ gps: { fix_type: null } }), true, null, null, null).gps).toBeNull();
  });

  it("reads the nine parameters in one call and keeps the ones the FC reported", async () => {
    vi.mocked(getParams).mockResolvedValue(cache({ EK3_SRC1_POSXY: 3, EK3_SRC2_POSXY: 6, FRAME_CLASS: 1 }));
    await loadEkfSourceParams();
    expect(getParams).toHaveBeenCalledTimes(1);
    expect(useNavStreamStore.getState().ekfParams).toEqual({ EK3_SRC1_POSXY: 3, EK3_SRC2_POSXY: 6 });
  });

  it("a failed read leaves every parameter out", async () => {
    vi.mocked(getParams).mockRejectedValue(new Error("503"));
    await loadEkfSourceParams();
    expect(useNavStreamStore.getState().ekfParams).toEqual({});
  });

  it("writes nothing after an abort", async () => {
    vi.mocked(getParams).mockResolvedValue(cache({ EK3_SRC1_POSXY: 3 }));
    const ac = new AbortController();
    const p = loadEkfSourceParams(ac.signal);
    ac.abort();
    await p;
    expect(useNavStreamStore.getState().ekfParams).toBeNull();
  });
});
