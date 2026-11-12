import { readFileSync } from "node:fs";
import path from "node:path";

import { describe, expect, it } from "vitest";

import { profileWordKey, roleWordKey } from "./profile-words";

describe("profile words", () => {
  it("maps profile and role ids to their labels, unknown to unrecognised", () => {
    expect(profileWordKey("ground_station")).toBe("access.profileGate.profileGroundStation");
    expect(roleWordKey("receiver")).toBe("access.profileGate.roleReceiver");
    expect(profileWordKey("toaster")).toBe("access.profileGate.profileOther");
    expect(roleWordKey("mystery")).toBe("access.profileGate.profileOther");
  });

  it("Settings › Profile shows the words, not `ground_station / role receiver`", () => {
    const s = readFileSync(path.resolve(__dirname, "../components/settings/sections/profile-section.tsx"), "utf8");
    expect(s).not.toContain('t("settings.profile.roleSeparator")}${initialRole}');
    expect(s).toContain("profileWordKey(initialProfile)");
  });
});
