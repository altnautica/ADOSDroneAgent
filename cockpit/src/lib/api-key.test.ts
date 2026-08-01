import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { consumeUrlKey, getApiKey } from "@/lib/api-key";

// The suite runs in the node environment (the cockpit's tests are otherwise
// pure), so the browser globals this module reads are stood up here rather than
// by pulling in a DOM.
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

let store: Storage;

/** Point the module at a URL and a clean store, the way a page load would. */
function visit(href: string) {
  store = fakeStorage();
  const replaceState = vi.fn();
  vi.stubGlobal("window", {
    location: new URL(href),
    history: { replaceState },
  });
  vi.stubGlobal("localStorage", store);
  return replaceState;
}

describe("the cockpit's credential", () => {
  beforeEach(() => visit("http://node.local/cockpit/"));
  afterEach(() => vi.unstubAllGlobals());

  it("is the same one the dashboard stores", () => {
    // Same origin, same agent, same credential. Storing it separately meant
    // signing in on the dashboard did nothing here, and the cockpit had no way
    // of its own to obtain one.
    store.setItem("ados-api-key", "from-the-dashboard");
    expect(getApiKey()).toBe("from-the-dashboard");
  });

  it("accepts the spelling the agent's own redirect preserves", () => {
    // The agent keeps `?key=` across its `/cockpit` → `/cockpit/` redirect,
    // while this page used to read only `?ados_key=`, so a reach link the
    // product itself produced was silently ignored.
    visit("http://node.local/cockpit/?key=abc123");
    consumeUrlKey();
    expect(getApiKey()).toBe("abc123");
  });

  it("still accepts the spelling it always did", () => {
    visit("http://node.local/cockpit/?ados_key=xyz789");
    consumeUrlKey();
    expect(getApiKey()).toBe("xyz789");
  });

  it("strips every accepted spelling from the address bar", () => {
    // The credential must not be left sitting in browser history.
    const replaceState = visit(
      "http://node.local/cockpit/?key=a&ados_key=b&tab=feed",
    );
    consumeUrlKey();
    expect(replaceState).toHaveBeenCalledOnce();
    const rewritten = String(replaceState.mock.calls[0]?.[2] ?? "");
    expect(rewritten).not.toContain("key=");
    expect(rewritten).not.toContain("ados_key=");
    // An unrelated parameter is left alone.
    expect(rewritten).toContain("tab=feed");
  });

  it("does nothing when the URL carries no key", () => {
    const replaceState = visit("http://node.local/cockpit/");
    consumeUrlKey();
    expect(getApiKey()).toBeNull();
    expect(replaceState).not.toHaveBeenCalled();
  });
});
