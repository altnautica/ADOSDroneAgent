import { describe, expect, it } from "vitest";

import { ROTATIONS, rotationLabel, toRotation } from "@/lib/settings/camera";
import { stepLeafChange } from "@/lib/settings/guarded-leaf";

describe("camera rotation", () => {
  it("accepts only the legal clockwise rotations", () => {
    expect(ROTATIONS).toEqual(["0", "90", "180", "270"]);
    expect(toRotation(90)).toBe("90");
    expect(toRotation("270")).toBe("270");
    expect(toRotation(45)).toBeNull();
    expect(toRotation(undefined)).toBeNull();
  });

  it("labels a rotation in degrees", () => {
    expect(rotationLabel("180")).toBe("180°");
  });

  it("writes without a guard", () => {
    expect(stepLeafChange({ key: "video.camera.hflip", value: true }, false).kind).toBe("write");
  });
});
