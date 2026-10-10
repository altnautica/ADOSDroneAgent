import { describe, expect, it } from "vitest";

import { catalogView, compareVersions, installState, type CatalogResponse } from "./plugin-catalog";

const entry = {
  id: "com.example.cam",
  name: "Cam",
  version: "1.4.1",
  description: "d",
  download_url: "https://cloud.example.com/cam.adosplug",
};

describe("catalogView", () => {
  it("renders a fetch error instead of vanishing", () => {
    const v = catalogView(undefined, new Error("HTTP 401"), false);
    expect(v).toEqual({ kind: "error", message: "Could not load the extension catalog: HTTP 401" });
  });

  it("renders the bundled-catalog parse error", () => {
    const data: CatalogResponse = {
      schema_version: 1,
      source: "first-party-bundled",
      plugins: [],
      error: "expected value at line 1",
    };
    const v = catalogView(data, null, false);
    expect(v.kind).toBe("error");
    if (v.kind === "error") expect(v.message).toContain("expected value at line 1");
  });

  it("lists entries", () => {
    const v = catalogView({ schema_version: 1, source: "x", plugins: [entry] }, null, false);
    expect(v).toEqual({ kind: "list", entries: [entry], parseError: null });
  });

  it("is loading while pending", () => {
    expect(catalogView(undefined, null, true)).toEqual({ kind: "loading" });
  });
});

describe("installState", () => {
  it("offers an update only when the catalog is newer", () => {
    expect(installState(entry, new Map())).toBe("absent");
    expect(installState(entry, new Map([["com.example.cam", "1.4.1"]]))).toBe("installed");
    expect(installState(entry, new Map([["com.example.cam", "1.0.2"]]))).toBe("update");
    expect(installState(entry, new Map([["com.example.cam", "1.10.0"]]))).toBe("installed");
  });

  it("compares numerically", () => {
    expect(compareVersions("0.4.1", "0.10.0")).toBe(-1);
    expect(compareVersions("1.0", "1.0.0")).toBe(0);
    expect(compareVersions("2.0.0-rc1", "1.9.9")).toBe(1);
  });
});
