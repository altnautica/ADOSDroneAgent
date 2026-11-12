import { readFileSync } from "node:fs";
import path from "node:path";

import { describe, expect, it } from "vitest";

import { cameraSourceOptions, isOtherSource } from "./camera-source";

const cam = (devicePath: string, label: string) => ({ devicePath, label, type: "usb", width: null, height: null });

describe("camera source picker", () => {
  it("lists each detected device path once, labelled with its name", () => {
    const opts = cameraSourceOptions([cam("/dev/video0", "USB Camera"), cam("/dev/video0", "dup"), cam("", "no path"), cam("/dev/video2", "")]);
    expect(opts).toEqual([
      { value: "/dev/video0", label: "USB Camera · /dev/video0" },
      { value: "/dev/video2", label: "/dev/video2" },
    ]);
  });

  it("keeps a path the scan does not list as free text", () => {
    const opts = cameraSourceOptions([cam("/dev/video0", "A")]);
    expect(isOtherSource("/dev/video0", opts)).toBe(false);
    expect(isOtherSource("rtsp://cam/stream", opts)).toBe(true);
    expect(isOtherSource("", opts)).toBe(false);
  });

  it("the Source leaf opens the picker instead of a bare keyboard", () => {
    const s = readFileSync(path.resolve(__dirname, "../components/settings/leaf-editor.tsx"), "utf8");
    expect(s).toMatch(/path === CAMERA_SOURCE_PATH \?[\s\S]*<CameraSourcePicker/);
  });
});
