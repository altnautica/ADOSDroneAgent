// @vitest-environment happy-dom
// The setup offer the banner publishes for the Menu stands only while the
// banner is mounted: the Feed has no banner, so an offer left behind there
// would keep "Set up aircraft" in the Menu after take-off.

import { createElement } from "react";
import { act } from "react-dom/test-utils";
import { createRoot } from "react-dom/client";
import { describe, expect, it, vi } from "vitest";

import { SetupOfferBanner } from "@/components/shell/setup-offer-banner";
import type * as AircraftSetup from "@/lib/aircraft-setup";
import { useSetupOfferStore } from "@/stores/setup-offer-store";

vi.mock("@/shared/use-profile", () => ({ useProfile: () => "drone" }));
vi.mock("@/shared/setup-wizard/client", () => ({ fetchSetupStatus: () => Promise.resolve({ uid: "fc-1" }) }));
vi.mock("@/lib/aircraft-setup", async (orig) => ({
  ...(await orig<typeof AircraftSetup>()),
  setupOfferUid: (status: { uid?: string } | null) => status?.uid ?? null,
}));

Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });

describe("the setup offer banner", () => {
  it("withdraws its offer from the Menu when it unmounts", async () => {
    const host = document.createElement("div");
    document.body.appendChild(host);
    const root = createRoot(host);
    await act(async () => root.render(createElement(SetupOfferBanner)));
    expect(useSetupOfferStore.getState().uid).toBe("fc-1");
    expect(host.querySelector('[data-testid="setup-offer"]')).not.toBeNull();
    await act(async () => root.unmount());
    host.remove();
    expect(useSetupOfferStore.getState().uid).toBeNull();
  });
});
