import { describe, expect, it } from "vitest";

import { followSourceLine } from "./follow-chrome";
import { parseFollowState } from "./nav-types";

describe("follow subject source", () => {
  it("parses the source and words beacon and predicted only", () => {
    const at = (subject_source: unknown) => parseFollowState({ phase: "following", subject_source })!;
    expect(at("beacon").subjectSource).toBe("beacon");
    expect(followSourceLine(at("beacon").subjectSource)).toBe("Following on phone position");
    expect(followSourceLine(at("predicted").subjectSource)).toBe("Predicted");
    expect(followSourceLine(at("vision").subjectSource)).toBeNull();
    expect(at(null).subjectSource).toBeNull();
    expect(at("radar").subjectSource).toBeNull();
    expect(followSourceLine(null)).toBeNull();
  });
});
