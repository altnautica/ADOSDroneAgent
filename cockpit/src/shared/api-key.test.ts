import { afterEach, describe, expect, it, vi } from "vitest";

// Node environment: the browser globals this module reads are stood up here.
function fakeStorage() {
  const map = new Map<string, string>();
  return {
    getItem: (k: string) => map.get(k) ?? null,
    setItem: (k: string, v: string) => void map.set(k, v),
    removeItem: (k: string) => void map.delete(k),
    clear: () => map.clear(),
    key: (i: number) => [...map.keys()][i] ?? null,
    get length() {
      return map.size;
    },
  } as Storage;
}

/** Point a fresh module instance at a URL and a store, the way a page load
 *  would (the module memoises its read, so each case reloads it). */
async function visit(href: string, seed: Record<string, string> = {}) {
  const store = fakeStorage();
  for (const [k, v] of Object.entries(seed)) store.setItem(k, v);
  const replaceState = vi.fn();
  vi.stubGlobal("window", {
    location: new URL(href),
    history: { replaceState, state: null },
    addEventListener: () => undefined,
  });
  vi.stubGlobal("localStorage", store);
  vi.resetModules();
  const mod = await import("./api-key");
  return { ...mod, replaceState };
}

afterEach(() => vi.unstubAllGlobals());

describe("the stored API key", () => {
  it("is the one key both agent surfaces share", async () => {
    const { getApiKey } = await visit("http://192.168.1.50:8080/cockpit/", {
      "ados-api-key": "from-the-dashboard",
    });
    expect(getApiKey()).toBe("from-the-dashboard");
  });

  it("accepts the spelling the agent's own redirect preserves", async () => {
    const { consumeUrlKey, getApiKey } = await visit("http://192.168.1.50:8080/cockpit/?key=abc123");
    consumeUrlKey();
    expect(getApiKey()).toBe("abc123");
  });

  it("accepts the deep-link spelling", async () => {
    const { consumeUrlKey, getApiKey } = await visit(
      "http://192.168.1.50:8080/cockpit/?ados_key=xyz789",
    );
    consumeUrlKey();
    expect(getApiKey()).toBe("xyz789");
  });

  it("strips every accepted spelling from the address bar", async () => {
    const { consumeUrlKey, replaceState } = await visit(
      "http://192.168.1.50:8080/cockpit/?key=a&ados_key=b&tab=feed",
    );
    consumeUrlKey();
    expect(replaceState).toHaveBeenCalledOnce();
    const rewritten = String(replaceState.mock.calls[0]?.[2] ?? "");
    expect(rewritten).not.toContain("key=");
    expect(rewritten).toContain("tab=feed");
  });

  it("does nothing when the URL carries no key", async () => {
    const { consumeUrlKey, getApiKey, replaceState } = await visit(
      "http://192.168.1.50:8080/cockpit/",
    );
    consumeUrlKey();
    expect(getApiKey()).toBeNull();
    expect(replaceState).not.toHaveBeenCalled();
  });
});
