import { beforeEach, describe, expect, it, vi } from "vitest";

const apiFetch = vi.fn();
vi.mock("../api-fetch", () => ({ apiFetch: (...args: unknown[]) => apiFetch(...args) }));

import { profileChangeNoteKeys, putConfigChecked } from "./apply-actions";

describe("putConfigChecked", () => {
  beforeEach(() => {
    apiFetch.mockReset();
    apiFetch.mockResolvedValue({ ok: true });
  });

  it("sends only key and value by default", async () => {
    await putConfigChecked("mavlink.baud_rate", "115200");
    expect(apiFetch).toHaveBeenCalledWith("/api/config", {
      method: "PUT",
      body: { key: "mavlink.baud_rate", value: "115200" },
    });
  });

  it("adds force when the override is asked for", async () => {
    await putConfigChecked("mavlink.baud_rate", "115200", { force: true });
    expect(apiFetch).toHaveBeenCalledWith("/api/config", {
      method: "PUT",
      body: { key: "mavlink.baud_rate", value: "115200", force: true },
    });
  });

  it("sends a number as a JSON number, so a null optional key can take it", async () => {
    await putConfigChecked("vision.detector.rate_hz", 2.5);
    expect(apiFetch).toHaveBeenCalledWith("/api/config", {
      method: "PUT",
      body: { key: "vision.detector.rate_hz", value: 2.5 },
    });
  });
});

describe("profileChangeNoteKeys", () => {
  it("says what a switch stops, and calls a role change a restart", () => {
    expect(profileChangeNoteKeys("ground_station", "ground_station")).toEqual([
      "settings.profile.confirmRoleRestart",
    ]);
    expect(profileChangeNoteKeys("ground_station", "drone")).toEqual([
      "settings.profile.confirmSwitch",
      "settings.profile.confirmKioskStops",
      "settings.profile.confirmAccessPointStops",
    ]);
    // A visor keeps the access point; a ground station's kiosk still goes.
    expect(profileChangeNoteKeys("ground_station", "visor")).toEqual([
      "settings.profile.confirmSwitch",
      "settings.profile.confirmKioskStops",
    ]);
    expect(profileChangeNoteKeys("visor", "drone")).toEqual([
      "settings.profile.confirmSwitch",
      "settings.profile.confirmAccessPointStops",
    ]);
    expect(profileChangeNoteKeys("drone", "ground_station")).toEqual(["settings.profile.confirmSwitch"]);
  });
});
