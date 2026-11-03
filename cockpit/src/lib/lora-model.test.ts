import { describe, expect, it } from "vitest";

import { FOLLOW_COUNTRY, regionApplyBlock, regionBody } from "@/lib/lora-model";
import { radioWriteBlock } from "@/lib/radio-gate";

describe("LoRa region body", () => {
  it("sends null to follow the country setting, else the code", () => {
    expect(regionBody(FOLLOW_COUNTRY)).toBeNull();
    expect(regionBody("EU")).toBe("EU");
  });
});

describe("LoRa region confirm gate", () => {
  const disarmed = radioWriteBlock({ connected: true, armed: false });
  it("applies a changed region while disarmed", () => {
    expect(regionApplyBlock("EU", "US", disarmed)).toBeNull();
    expect(regionApplyBlock(FOLLOW_COUNTRY, "US", disarmed)).toBeNull();
  });

  it("holds an unchanged choice", () => {
    expect(regionApplyBlock("US", "US", disarmed)).toBe("unchanged");
  });

  it("refuses while armed and while the arm state is unknown", () => {
    expect(regionApplyBlock("EU", "US", radioWriteBlock({ connected: true, armed: true }))).toBe("armed");
    expect(regionApplyBlock("EU", "US", radioWriteBlock({ connected: true, armed: null }))).toBe("armedUnknown");
  });
});
