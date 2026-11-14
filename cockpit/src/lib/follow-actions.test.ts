import { describe, expect, it, vi } from "vitest";

import { followActionForKey, FOLLOW_ACTION_KEYS, runFollowAction } from "./follow-actions";
import { FOLLOW_PROMPT_ACTIONS } from "@/shared/hud/follow-prompt";

describe("runFollowAction", () => {
  it("resume bumps operator_resume_seq by one", async () => {
    const put = vi.fn().mockResolvedValue({ ok: true });
    await runFollowAction("resume", { put, pilot: vi.fn(), startPick: vi.fn(), seq: 4 });
    expect(put).toHaveBeenCalledWith("com.altnautica.follow-me", "operator_resume_seq", 5);
  });

  it("rth and land go to the pilot", async () => {
    const pilot = vi.fn().mockResolvedValue({ ok: true });
    await runFollowAction("rth", { put: vi.fn(), pilot, startPick: vi.fn(), seq: 0 });
    expect(pilot).toHaveBeenCalledWith("rth");
    await runFollowAction("land", { put: vi.fn(), pilot, startPick: vi.fn(), seq: 0 });
    expect(pilot).toHaveBeenCalledWith("land");
  });

  it("redesignate arms the pick-a-target mode", async () => {
    const startPick = vi.fn();
    const put = vi.fn();
    const pilot = vi.fn();
    await runFollowAction("redesignate", { put, pilot, startPick, seq: 0 });
    expect(startPick).toHaveBeenCalled();
    expect(put).not.toHaveBeenCalled();
    expect(pilot).not.toHaveBeenCalled();
  });

  it("stop turns follow off", async () => {
    const put = vi.fn().mockResolvedValue({ ok: true });
    await runFollowAction("stop", { put, pilot: vi.fn(), startPick: vi.fn(), seq: 0 });
    expect(put).toHaveBeenCalledWith("com.altnautica.follow-me", "active", false);
  });

  it("reports a refused pilot action and a failed config write without throwing", async () => {
    const pilot = vi.fn().mockResolvedValue({ ok: false, err: "no pilot link" });
    expect(await runFollowAction("land", { put: vi.fn(), pilot, startPick: vi.fn(), seq: 0 })).toEqual({
      ok: false,
      err: "no pilot link",
    });
    const put = vi.fn().mockRejectedValue(new Error("HTTP 503"));
    expect(await runFollowAction("resume", { put, pilot: vi.fn(), startPick: vi.fn(), seq: 0 })).toEqual({
      ok: false,
      err: "HTTP 503",
    });
  });
});

describe("followActionForKey", () => {
  const plain = { shiftKey: false, ctrlKey: false, altKey: false, metaKey: false };

  it("maps R, P, H, L and X to the prompt's actions", () => {
    expect(FOLLOW_ACTION_KEYS).toEqual({ resume: "r", redesignate: "p", rth: "h", land: "l", stop: "x" });
    expect(followActionForKey({ ...plain, key: "r" }, FOLLOW_PROMPT_ACTIONS)).toBe("resume");
    expect(followActionForKey({ ...plain, key: "P" }, FOLLOW_PROMPT_ACTIONS)).toBe("redesignate");
    expect(followActionForKey({ ...plain, key: "h" }, FOLLOW_PROMPT_ACTIONS)).toBe("rth");
    expect(followActionForKey({ ...plain, key: "l" }, FOLLOW_PROMPT_ACTIONS)).toBe("land");
    expect(followActionForKey({ ...plain, key: "x" }, FOLLOW_PROMPT_ACTIONS)).toBe("stop");
  });

  it("leaves chords and keys of actions not offered to their other owners", () => {
    // Shift+L is the land built-in, Shift+R return home: the prompt never takes a chord.
    expect(followActionForKey({ ...plain, key: "L", shiftKey: true }, FOLLOW_PROMPT_ACTIONS)).toBeNull();
    expect(followActionForKey({ ...plain, key: "r", ctrlKey: true }, FOLLOW_PROMPT_ACTIONS)).toBeNull();
    expect(followActionForKey({ ...plain, key: "v" }, FOLLOW_PROMPT_ACTIONS)).toBeNull();
    expect(followActionForKey({ ...plain, key: "r" }, ["redesignate", "rth", "land", "stop"])).toBeNull();
  });
});
