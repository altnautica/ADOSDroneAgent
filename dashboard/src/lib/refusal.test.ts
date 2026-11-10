import { describe, expect, it } from "vitest";

import { refusalDetail } from "@/lib/refusal";
import { ApiError } from "@/shared/api-fetch";
import i18n from "@/shared/i18n";

describe("refusal detail", () => {
  it("reads the fallback through the locale when the refusal carries no detail", () => {
    i18n.addResource("en", "translation", "common.refusedFallback", "LOCALISED FALLBACK");
    const err = new ApiError("Unauthorized", 401, null);
    expect(refusalDetail(err)).toBe("LOCALISED FALLBACK");
    i18n.addResource("en", "translation", "common.refusedFallback", "This device refused the request.");
    expect(refusalDetail(err)).toBe("This device refused the request.");
  });
});
