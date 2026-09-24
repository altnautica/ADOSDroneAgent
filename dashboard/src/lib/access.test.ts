import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { verifyAccess } from "./access";
import { ApiError, apiFetch, setAuthRequiredHandler } from "./api";
import {
  groupPermissions,
  installPlugin,
  requiresCoolOff,
  type PluginManifestSummary,
} from "./plugin-install";
import { clearSession, getSession, setSession } from "./session";

function reply(status: number, body: unknown): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

const fetchMock = vi.fn<(input: RequestInfo | URL, init?: RequestInit) => Promise<Response>>();

beforeEach(() => {
  fetchMock.mockReset();
  vi.stubGlobal("fetch", fetchMock);
  setSession("session-token", 0);
});

afterEach(() => {
  vi.unstubAllGlobals();
  setAuthRequiredHandler(null);
  clearSession();
});

describe("a refused request", () => {
  it("keeps the session and asks the gate to re-verify", async () => {
    // A 403 that is about the request, not the credential (a plugin capability
    // the plugin was never granted). Dropping the session here logged a valid
    // operator out to the PIN splash.
    fetchMock.mockResolvedValueOnce(reply(403, { detail: "capability_denied: mavlink.write" }));
    const reverify = vi.fn();
    setAuthRequiredHandler(reverify);

    await expect(apiFetch("/api/plugins/demo/tools/x/invoke", { method: "POST" })).rejects.toBeInstanceOf(
      ApiError,
    );
    expect(getSession()).toBe("session-token");
    expect(reverify).toHaveBeenCalledTimes(1);
  });
});

describe("verifyAccess", () => {
  it("drops the session only when the agent refuses the probe itself", async () => {
    fetchMock.mockResolvedValueOnce(reply(200, { version: "x" }));
    expect(await verifyAccess()).toBe("ok");
    expect(getSession()).toBe("session-token");

    fetchMock.mockResolvedValueOnce(reply(401, { detail: "Missing X-ADOS-Key header." }));
    expect(await verifyAccess()).toBe("locked");
    expect(getSession()).toBeNull();
  });
});

describe("plugin install", () => {
  it("sends the dashboard credential and the approved permissions", async () => {
    fetchMock.mockResolvedValueOnce(reply(200, { ok: true, plugin_id: "demo", granted: ["mavlink.read"] }));
    const res = await installPlugin(
      { kind: "catalog", url: "https://example.com/demo.adosplug", sha256: "ab" },
      ["mavlink.read"],
    );
    expect(res.ok).toBe(true);
    const [, init] = fetchMock.mock.calls[0];
    const headers = init?.headers as Record<string, string>;
    expect(headers["X-ADOS-Dashboard-Session"]).toBe("session-token");
    expect(JSON.parse(String(init?.body)).requested_permissions).toEqual(["mavlink.read"]);
  });

  it("grades each permission with the risk the agent sent", () => {
    const manifest: PluginManifestSummary = {
      ok: true,
      plugin_id: "demo",
      version: "1.0.0",
      name: "Demo",
      risk: "low",
      signer_id: null,
      signed: true,
      halves: ["agent"],
      permissions: [
        { id: "flight.rate_setpoint", required: true, risk: "critical" },
        { id: "telemetry.read", required: true, risk: "low" },
      ],
    };
    const rows = groupPermissions(manifest).flatMap((g) => g.rows);
    expect(rows.find((r) => r.id === "flight.rate_setpoint")?.risk).toBe("critical");
    expect(requiresCoolOff(manifest)).toBe(true);
  });
});
