import { readFileSync } from "node:fs";
import path from "node:path";

import { describe, expect, it } from "vitest";

import { suggestionDraft, suggestionLabel } from "@/lib/profile-suggestion";

const page = readFileSync(path.resolve(__dirname, "../routes/settings/profile-settings.tsx"), "utf8");

describe("redetect suggestion", () => {
  it("names the suggested profile and role in words", () => {
    expect(suggestionLabel({ detected: "ground_station", ground_role_hint: "relay" })).toBe("Ground station · Relay");
    expect(suggestionLabel({ detected: "drone" })).toBe("Drone");
  });

  it("loads the suggestion into the draft, keeping the drafted role without a hint", () => {
    expect(suggestionDraft({ detected: "ground_station", ground_role_hint: "receiver" }, "direct")).toEqual({
      profile: "ground_station",
      role: "receiver",
    });
    expect(suggestionDraft({ detected: "drone" }, "relay")).toEqual({ profile: "drone", role: "relay" });
  });

  it("keeps the redetect answer and applies it through the existing confirm", () => {
    expect(page).toMatch(/setSuggestion\(await apiFetch<ProfileSuggestion>\("\/api\/v1\/setup\/profile\/redetect"/);
    expect(page).toMatch(/const draft = suggestionDraft\(suggestion, role\);[\s\S]{0,120}setConfirmOpen\(true\);/);
    expect(page).toContain('t("settings.profile.applySuggestion")');
  });
});
