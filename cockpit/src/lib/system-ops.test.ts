import { describe, expect, it } from "vitest";

import {
  SYSTEM_ACTION_GESTURE,
  maxTemp,
  parseServices,
  restartVerdict,
  serviceTone,
  shortServiceName,
} from "@/lib/system-ops";

describe("service inventory", () => {
  it("reads the object shape and keeps systemd availability", () => {
    const inv = parseServices({
      services: [{ name: "ados-video.service", state: "active", active: true }, { state: "active" }, { name: "" }],
      systemd_available: false,
    });
    expect(inv.services.map((s) => s.name)).toEqual(["ados-video.service"]);
    expect(inv.systemdAvailable).toBe(false);
  });

  it("reads the older bare-list shape", () => {
    const inv = parseServices([{ name: "ados-supervisor", state: "active" }]);
    expect(inv.services).toHaveLength(1);
    expect(inv.systemdAvailable).toBe(true);
  });

  it("returns an empty inventory for a body that is not a list", () => {
    expect(parseServices(null)).toEqual({ services: [], systemdAvailable: true });
    expect(parseServices("oops").services).toEqual([]);
  });

  it("tones failed, running and other units", () => {
    expect(serviceTone({ name: "a", state: "failed" })).toBe("err");
    expect(serviceTone({ name: "a", state: "active", active: true })).toBe("ok");
    expect(serviceTone({ name: "a", state: "inactive" })).toBe("warn");
  });

  it("shortens unit names", () => {
    expect(shortServiceName("ados-video.service")).toBe("video");
  });
});

describe("restart verdict", () => {
  it("treats only status ok as a restart", () => {
    expect(restartVerdict({ status: "ok", message: "done" })).toEqual({ ok: true, message: "done" });
    expect(restartVerdict({ status: "error", message: "unknown unit" })).toEqual({ ok: false, message: "unknown unit" });
    expect(restartVerdict({})).toEqual({ ok: false, message: null });
    expect(restartVerdict(null)).toEqual({ ok: false, message: null });
  });
});

describe("temperatures", () => {
  it("takes the hottest sensor and null for none", () => {
    expect(maxTemp({ cpu: 61, gpu: 72.5 })).toBe(72.5);
    expect(maxTemp({})).toBeNull();
    expect(maxTemp(undefined)).toBeNull();
  });
});

describe("confirm gates", () => {
  it("guards reboot and service restart, never a plain tap", () => {
    expect(SYSTEM_ACTION_GESTURE.reboot).toBe("guarded");
    expect(SYSTEM_ACTION_GESTURE["restart-service"]).toBe("guarded");
  });
});
