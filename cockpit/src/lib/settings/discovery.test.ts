import { describe, expect, it } from "vitest";

import { settleGate } from "@/lib/settings/apply-form";
import { MDNS_KEY, mdnsNeedsGuard, reachFacts } from "@/lib/settings/discovery";
import { stepLeafChange } from "@/lib/settings/guarded-leaf";
import type { SetupStatus } from "@/shared/agent-api.generated";

function status(over: { mdns_host?: string; hostname?: string; lan_host?: string; local_ips?: string[] }): SetupStatus {
  return {
    lan_host: over.lan_host ?? "",
    access_urls: [],
    network: {
      mdns_host: over.mdns_host ?? "",
      hostname: over.hostname ?? "",
      local_ips: over.local_ips ?? [],
    },
  } as unknown as SetupStatus;
}

describe("discovery mDNS guard", () => {
  it("guards disabling only", () => {
    expect(mdnsNeedsGuard(false)).toBe(true);
    expect(mdnsNeedsGuard(true)).toBe(false);
  });

  it("does not write the disable until the guard fires", () => {
    const change = { key: MDNS_KEY, value: false };
    const step = stepLeafChange(change, mdnsNeedsGuard(false));
    expect(step.kind).toBe("hold");
    if (step.kind !== "hold") return;
    expect(settleGate(step.gate, false)).toBeNull();
    expect(settleGate(step.gate, true)).toEqual(change);
  });

  it("writes the enable at once", () => {
    expect(stepLeafChange({ key: MDNS_KEY, value: true }, mdnsNeedsGuard(true)).kind).toBe("write");
  });
});

describe("discovery reach facts", () => {
  it("shows the hostname and LAN host only when they differ from the mDNS name", () => {
    const same = reachFacts(status({ mdns_host: "node.local", hostname: "node.local", lan_host: "node.local" }));
    expect(same).toMatchObject({ mdnsHost: "node.local", hostname: null, lanHost: null });
    const differ = reachFacts(status({ mdns_host: "node.local", hostname: "node", lan_host: "192.168.1.5" }));
    expect(differ).toMatchObject({ hostname: "node", lanHost: "192.168.1.5" });
  });

  it("reports an unpublished mDNS name as empty, never invented", () => {
    expect(reachFacts(status({ mdns_host: "  ", local_ips: ["10.0.0.4"] }))).toMatchObject({ mdnsHost: "", localIps: ["10.0.0.4"] });
    expect(reachFacts(null)).toMatchObject({ mdnsHost: "", hostname: null, lanHost: null, localIps: [], urls: [] });
  });
});
