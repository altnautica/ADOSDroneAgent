import { beforeEach, describe, expect, it } from "vitest";

import {
  REFUSED_POLL_INTERVAL_MS,
  isRefusal,
  pollIntervalFor,
  useReachStore,
} from "@/stores/reach-store";

function reset() {
  useReachStore.setState({ refusal: "none", pairingCode: null });
}

describe("isRefusal", () => {
  it("treats only the credential codes as a refusal", () => {
    expect(isRefusal(401)).toBe(true);
    expect(isRefusal(403)).toBe(true);
  });

  it("does not treat a missing route or a server fault as a refusal", () => {
    // A 404 is a route this profile does not serve — three screens already
    // branch on it and it must not raise a pairing prompt. A 5xx is the
    // agent's own fault and says nothing about credentials.
    for (const s of [404, 500, 502, 503, null]) {
      expect(isRefusal(s)).toBe(false);
    }
  });
});

describe("reach store", () => {
  beforeEach(reset);

  it("names an unpaired node from the code it actually answers with", () => {
    // 403 is what an unpaired node returns a peer it will not talk to.
    useReachStore.getState().report(403, false);
    expect(useReachStore.getState().refusal).toBe("unpaired");
  });

  it("distinguishes a paired node we hold no credential for", () => {
    // 401 is a different problem with a different next step, so it must not
    // collapse into the pairing prompt.
    useReachStore.getState().report(401, false);
    expect(useReachStore.getState().refusal).toBe("unauthorized");
  });

  it("stays quiet for failures that are not about credentials", () => {
    for (const s of [404, 500, null]) {
      useReachStore.getState().report(s, false);
      expect(useReachStore.getState().refusal).toBe("none");
    }
  });

  it("clears on the next success so pairing recovers without a reload", () => {
    // The panel is open while the operator pairs the node from elsewhere. The
    // next poll succeeds and the notice must go away by itself.
    useReachStore.getState().report(403, false);
    expect(useReachStore.getState().refusal).toBe("unpaired");
    useReachStore.getState().report(null, true);
    expect(useReachStore.getState().refusal).toBe("none");
  });

  it("keeps the pairing code so the notice can offer a way out", () => {
    // The identity route is the one call an unpaired node still answers, so it
    // is the only place this can come from.
    useReachStore.getState().setPairingCode("RXW6XR");
    expect(useReachStore.getState().pairingCode).toBe("RXW6XR");
  });

  it("does not churn state when the same verdict repeats", () => {
    // Every poll of every screen reports, several times a second. Re-setting an
    // unchanged value would re-render every subscriber on each one.
    useReachStore.getState().report(403, false);
    const first = useReachStore.getState();
    useReachStore.getState().report(403, false);
    expect(useReachStore.getState()).toBe(first);

    useReachStore.getState().report(null, true);
    const cleared = useReachStore.getState();
    useReachStore.getState().report(null, true);
    expect(useReachStore.getState()).toBe(cleared);
  });
});

describe("polling while refused", () => {
  it("slows a fast poll down", () => {
    // Measured on a refusing node: 512 rejected requests in two minutes, each
    // one also a warning written on the box the panel cannot read.
    expect(pollIntervalFor(400, "unpaired")).toBe(REFUSED_POLL_INTERVAL_MS);
    expect(pollIntervalFor(1500, "unauthorized")).toBe(REFUSED_POLL_INTERVAL_MS);
  });

  it("never speeds a slow poll up", () => {
    // A screen that deliberately polls slowly keeps its own pace; the backoff
    // is a floor, not a rate.
    expect(pollIntervalFor(30_000, "unpaired")).toBe(30_000);
  });

  it("returns to the normal cadence once the refusal clears", () => {
    expect(pollIntervalFor(400, "none")).toBe(400);
  });
});
