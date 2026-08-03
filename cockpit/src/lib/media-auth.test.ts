import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

// The suite runs in the node environment (see vitest.config.ts), so the browser
// storage these modules read is stood up here rather than by pulling in a DOM —
// the same shape the api-key suite uses.
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

// The key and shape `session.ts` persists. Pinned here on purpose: if either is
// changed without updating this, these tests fail rather than silently
// exercising an absent session and passing for the wrong reason.
const STORAGE_KEY = "ados-dashboard-session";

function storeSession(token: string): void {
  localStorage.setItem(
    STORAGE_KEY,
    JSON.stringify({ token, expiresAt: Math.floor(Date.now() / 1000) + 3600 }),
  );
}

/** `session.ts` memoises its read, so each case needs a fresh module graph. */
async function loadWithMediaAuth() {
  vi.resetModules();
  return (await import("@/lib/media-auth")).withMediaAuth;
}

beforeEach(() => {
  vi.stubGlobal("window", {} as Window & typeof globalThis);
  vi.stubGlobal("localStorage", fakeStorage());
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("withMediaAuth", () => {
  it("carries the session in the URL for a player that cannot send a header", async () => {
    // A <video> element fetches its own playlist and segments and gives no hook
    // to attach a header, so this is the only way an off-box viewer can
    // authenticate element-driven playback.
    storeSession("tok123");
    const withMediaAuth = await loadWithMediaAuth();
    expect(withMediaAuth("/hls/main/index.m3u8")).toBe(
      "/hls/main/index.m3u8?ados_session=tok123",
    );
  });

  it("appends rather than clobbering an existing query", async () => {
    storeSession("tok123");
    const withMediaAuth = await loadWithMediaAuth();
    expect(withMediaAuth("/whep?camera=front")).toBe(
      "/whep?camera=front&ados_session=tok123",
    );
  });

  it("leaves the URL untouched when there is no session", async () => {
    // An on-box viewer needs no credential. Appending an empty parameter would
    // put a credential-shaped token in the URL that authenticates nothing.
    const withMediaAuth = await loadWithMediaAuth();
    expect(withMediaAuth("/whep")).toBe("/whep");
    expect(withMediaAuth("/whep")).not.toContain("ados_session");
  });

  it("encodes the token so a stray character cannot break out of the parameter", async () => {
    storeSession("a b&c=d");
    const withMediaAuth = await loadWithMediaAuth();
    expect(withMediaAuth("/whep")).toBe("/whep?ados_session=a%20b%26c%3Dd");
  });
});
