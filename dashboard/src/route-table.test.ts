import { describe, expect, it } from "vitest";

import { ROUTE_COMPONENTS, ROUTES } from "@/route-table";

describe("route table", () => {
  it("maps every key to a component and every component to a key", () => {
    const keys = ROUTES.map((r) => r.key).sort();
    expect(keys).toEqual(Object.keys(ROUTE_COMPONENTS).sort());
    for (const r of ROUTES) {
      expect(ROUTE_COMPONENTS[r.key as keyof typeof ROUTE_COMPONENTS]).toBeTruthy();
    }
  });

  it("has unique keys and paths", () => {
    expect(new Set(ROUTES.map((r) => r.key)).size).toBe(ROUTES.length);
    expect(new Set(ROUTES.map((r) => r.path)).size).toBe(ROUTES.length);
  });

  it("gates roles only behind a profile", () => {
    for (const r of ROUTES) {
      if (r.roles) expect(r.profiles).not.toBeNull();
    }
  });
});
