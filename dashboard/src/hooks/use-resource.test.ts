import { QueryClient } from "@tanstack/react-query";
import { describe, expect, it, vi } from "vitest";

vi.mock("@/shared/api-fetch", () => ({
  apiFetch: vi.fn(async (path: string) => ({ path })),
}));

import { resourceQueryOptions } from "./use-resource";

describe("useResource cache key", () => {
  it("keeps two readers with one name and different paths apart", async () => {
    const qc = new QueryClient();
    const a = resourceQueryOptions<{ path: string }>("status", "/api/a");
    const b = resourceQueryOptions<{ path: string }>("status", "/api/b");
    expect(a.queryKey).not.toEqual(b.queryKey);

    const ra = await qc.fetchQuery(a);
    const rb = await qc.fetchQuery(b);
    expect(ra.path).toBe("/api/a");
    expect(rb.path).toBe("/api/b");
    expect(qc.getQueryData(a.queryKey)).toEqual({ path: "/api/a" });
  });
});
