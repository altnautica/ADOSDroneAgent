import { beforeEach, describe, expect, it } from "vitest";

import { useTrackStore } from "@/stores/track-store";

// The store is a module singleton; reset it between cases so each test starts
// from an empty track with no vehicle.
beforeEach(() => {
  useTrackStore.setState({ start: null, samples: [], vehicleId: null });
});

// A pair of Bangalore fixes ~5 km apart — both pass the jitter filter (> 1.5 m),
// so recording them yields a real two-point trail with a "start".
const A1: [number, number] = [12.9, 77.6];
const A2: [number, number] = [12.95, 77.65];

describe("track-store vehicle identity", () => {
  it("adopts the first vehicle without clearing the in-progress track", () => {
    const s = useTrackStore.getState();
    s.record(...A1);
    s.syncVehicle("drone-a");

    const st = useTrackStore.getState();
    expect(st.vehicleId).toBe("drone-a");
    expect(st.start).not.toBeNull();
    expect(st.samples).toHaveLength(1);
  });

  it("clears the breadcrumb + start when the paired vehicle changes", () => {
    const s = useTrackStore.getState();
    s.syncVehicle("drone-a");
    s.record(...A1);
    s.record(...A2);
    expect(useTrackStore.getState().samples).toHaveLength(2);
    expect(useTrackStore.getState().start).not.toBeNull();

    // A mid-session re-pair to a different drone.
    useTrackStore.getState().syncVehicle("drone-b");

    const st = useTrackStore.getState();
    expect(st.vehicleId).toBe("drone-b");
    expect(st.start).toBeNull();
    expect(st.samples).toEqual([]);
  });

  it("keeps the track through a transient dropout and a re-pair to the same vehicle", () => {
    const s = useTrackStore.getState();
    s.syncVehicle("drone-a");
    s.record(...A1);
    s.syncVehicle(null); // link drop / unpaired — must not wipe
    s.syncVehicle("drone-a"); // same vehicle back — must not wipe

    const st = useTrackStore.getState();
    expect(st.samples).toHaveLength(1);
    expect(st.start).not.toBeNull();
    expect(st.vehicleId).toBe("drone-a");
  });

  it("resets when a different vehicle appears after a dropout (A then null then B)", () => {
    const s = useTrackStore.getState();
    s.syncVehicle("drone-a");
    s.record(...A1);
    s.syncVehicle(null);
    s.syncVehicle("drone-b");

    const st = useTrackStore.getState();
    expect(st.vehicleId).toBe("drone-b");
    expect(st.start).toBeNull();
    expect(st.samples).toEqual([]);
  });
});
