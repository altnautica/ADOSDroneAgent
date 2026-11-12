import { describe, expect, it } from "vitest";

import {
  catalogView,
  compareVersions,
  installState,
  isInstallUrl,
  normalizeSha256,
  parseInstalls,
} from "@/lib/plugin-catalog";

const entry = { id: "a", name: "A", version: "1.2.0", description: "", download_url: "https://x/a.adosplug" };

describe("catalog view", () => {
  it("shows load and parse failures instead of an empty list", () => {
    expect(catalogView(null, "boom", true)).toEqual({ kind: "error", message: "boom" });
    expect(catalogView(null, null, false)).toEqual({ kind: "loading" });
    expect(catalogView({ schema_version: 1, source: "b", plugins: [], error: "bad" }, null, true)).toEqual({
      kind: "parseError",
      message: "bad",
    });
    expect(catalogView({ schema_version: 1, source: "b", plugins: [entry] }, null, true)).toMatchObject({ kind: "list" });
  });

  it("compares versions and flags updates", () => {
    expect(compareVersions("1.10.0", "1.4.1")).toBe(1);
    expect(compareVersions("1.0.0-rc1", "1.0.0")).toBe(0);
    expect(installState(entry, new Map())).toBe("absent");
    expect(installState(entry, new Map([["a", "1.1.9"]]))).toBe("update");
    expect(installState(entry, new Map([["a", "1.2.0"]]))).toBe("installed");
  });
});

describe("install list", () => {
  it("counts grants and reads enabled and signed", () => {
    const [r] = parseInstalls({
      installs: [
        {
          plugin_id: "a",
          version: "1.0.0",
          status: "running",
          signer_id: "k1",
          permissions: { x: { granted: true }, y: { granted: false } },
        },
        { status: "enabled" },
      ],
    });
    expect(r).toEqual({ pluginId: "a", version: "1.0.0", status: "running", enabled: true, signed: true, granted: 1, declared: 2, failureReason: null });
    expect(parseInstalls({ installs: [{ plugin_id: "b", status: "disabled" }] })[0]).toMatchObject({ enabled: false, signed: false });
  });
});

describe("URL install inputs", () => {
  it("accepts http(s) URLs only and validates the pin", () => {
    expect(isInstallUrl("https://example.com/a.adosplug")).toBe(true);
    expect(isInstallUrl("file:///a.adosplug")).toBe(false);
    expect(isInstallUrl("nope")).toBe(false);
    expect(normalizeSha256("")).toBeNull();
    expect(normalizeSha256(" AB".padEnd(65, "a"))).toBe("ab".padEnd(64, "a"));
    expect(normalizeSha256("abc")).toBe("invalid");
  });
});
