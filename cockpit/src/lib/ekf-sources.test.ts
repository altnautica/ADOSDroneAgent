import { beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("@/lib/api", () => ({ getParam: vi.fn() }));

import { getParam } from "@/lib/api";
import { EKF_SOURCE_PARAMS, ekfFlagWords, loadEkfSourceParams, navSourceInput, sourceWord } from "@/lib/ekf-sources";
import type { VehicleState } from "@/lib/types";
import { useNavStreamStore } from "@/stores/nav-stream-store";

const telemetry = (t: Record<string, unknown>) => t as unknown as VehicleState;

describe("ekf-sources", () => {
  beforeEach(() => {
    useNavStreamStore.getState().clear();
    vi.mocked(getParam).mockReset();
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

  it("stores only the parameters the FC reported", async () => {
    vi.mocked(getParam).mockImplementation(async (name: string) => {
      if (name === "EK3_SRC1_POSXY") return 3;
      if (name === "EK3_SRC2_POSXY") return 6;
      if (name === "EK3_SRC1_VELXY") throw new Error("404");
      return null;
    });
    await loadEkfSourceParams();
    expect(useNavStreamStore.getState().ekfParams).toEqual({ EK3_SRC1_POSXY: 3, EK3_SRC2_POSXY: 6 });
  });

  it("writes nothing after an abort", async () => {
    vi.mocked(getParam).mockResolvedValue(3);
    const ac = new AbortController();
    const p = loadEkfSourceParams(ac.signal);
    ac.abort();
    await p;
    expect(useNavStreamStore.getState().ekfParams).toBeNull();
  });
});
