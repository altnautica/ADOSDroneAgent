import { describe, expect, it, vi } from "vitest";

import { CLOSED_GATE, openGate, settleGate } from "@/lib/settings/apply-form";
import { profileDirty, profilePayload, type ProfileDraft } from "@/lib/settings/profile";
import { ApiError } from "@/shared/api-fetch";
import { withArmedRetry } from "@/shared/api/armed-retry";

const initial = { profile: "ground_station", role: "direct" } as const;

describe("profileDirty", () => {
  it("counts a profile change, and a role change only on a ground station", () => {
    expect(profileDirty({ ...initial, autoRestart: true }, initial)).toBe(false);
    expect(profileDirty({ profile: "drone", role: "direct", autoRestart: true }, initial)).toBe(true);
    expect(profileDirty({ profile: "ground_station", role: "relay", autoRestart: true }, initial)).toBe(true);
    expect(
      profileDirty({ profile: "drone", role: "relay", autoRestart: true }, { profile: "drone", role: "direct" }),
    ).toBe(false);
  });

  it("never counts the auto-restart option", () => {
    expect(profileDirty({ ...initial, autoRestart: false }, initial)).toBe(false);
  });
});

describe("profilePayload", () => {
  it("sends the role only for a ground station and force only on the retry", () => {
    expect(profilePayload({ profile: "drone", role: "relay", autoRestart: true }, false)).toEqual({
      profile: { profile: "drone", ground_role: undefined, auto_restart: true },
      force: undefined,
    });
    expect(profilePayload({ profile: "ground_station", role: "relay", autoRestart: false }, true)).toEqual({
      profile: { profile: "ground_station", ground_role: "relay", auto_restart: false },
      force: true,
    });
  });
});

describe("profile apply gate", () => {
  const draft: ProfileDraft = { profile: "visor", role: "direct", autoRestart: true };

  it("does not post until the slide confirms", () => {
    const post = vi.fn();
    const gate = openGate(draft);
    const cancelled = settleGate(gate, false);
    if (cancelled) post(profilePayload(cancelled, false));
    expect(post).not.toHaveBeenCalled();
    const confirmed = settleGate(gate, true);
    if (confirmed) post(profilePayload(confirmed, false));
    expect(post).toHaveBeenCalledOnce();
    expect(settleGate(CLOSED_GATE, true)).toBeNull();
  });

  it("retries with force only after the armed confirm says yes", async () => {
    const armed = new ApiError("409 armed", 409, { error: "E_ARMED", override: "force" });
    const calls: boolean[] = [];
    const call = (force: boolean) => {
      calls.push(force);
      return force ? Promise.resolve("ok") : Promise.reject(armed);
    };
    await expect(withArmedRetry(call, () => Promise.resolve(false))).rejects.toBe(armed);
    expect(calls).toEqual([false]);
    calls.length = 0;
    await expect(withArmedRetry(call, () => Promise.resolve(true))).resolves.toBe("ok");
    expect(calls).toEqual([false, true]);
    expect(profilePayload(draft, true).force).toBe(true);
  });
});
