import { describe, expect, it } from "vitest";

import { profileActionForKey } from "@/lib/feed-actions";

describe("profile keys", () => {
  it("maps F1–F3 to the profile actions once prefixed", () => {
    expect(profileActionForKey("F1")).toBe("profile:cine");
    expect(profileActionForKey("f2")).toBe("profile:normal");
    expect(profileActionForKey("f3")).toBe("profile:sport");
    expect(profileActionForKey("f4")).toBeNull();
  });
});
