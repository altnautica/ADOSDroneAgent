import { describe, expect, it } from "vitest";

import { resolveWhepResource, whepUrlFor } from "./whep";

describe("resolveWhepResource", () => {
  it("resolves the agent's path-only Location against the page URL", () => {
    expect(resolveWhepResource("/whep/session-1", "http://192.168.1.50:8080/cockpit/")).toBe(
      "http://192.168.1.50:8080/whep/session-1",
    );
  });

  it("keeps an absolute Location as-is", () => {
    expect(
      resolveWhepResource("http://192.168.1.50:8889/main/whep/abc", "http://192.168.1.50:8080/"),
    ).toBe("http://192.168.1.50:8889/main/whep/abc");
  });

  it("resolves a relative Location against the page directory", () => {
    expect(resolveWhepResource("whep/xyz", "http://192.168.1.50:8080/cockpit/")).toBe(
      "http://192.168.1.50:8080/cockpit/whep/xyz",
    );
  });

  it("returns null when there is no valid base", () => {
    expect(resolveWhepResource("/whep/x", "not a url")).toBeNull();
  });
});

describe("whepUrlFor", () => {
  it("addresses a camera leg with ?camera=", () => {
    expect(whepUrlFor("/whep", "cam 2")).toBe("/whep?camera=cam%202");
    expect(whepUrlFor("/whep?x=1", "front")).toBe("/whep?x=1&camera=front");
  });

  it("leaves the primary leg untouched", () => {
    expect(whepUrlFor("/whep", null)).toBe("/whep");
  });
});
