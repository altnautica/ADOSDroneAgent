import { describe, expect, it, vi } from "vitest";

import { followActionForKey, FOLLOW_ACTION_KEYS, runFollowAction, type FollowActionDeps } from "./follow-actions";
import { FOLLOW_PROMPT_ACTIONS } from "@/shared/hud/follow-prompt";

function deps(over: Partial<FollowActionDeps> = {}): FollowActionDeps {
  return { put: vi.fn().mockResolvedValue({ ok: true }), flight: vi.fn(() => true), startPick: vi.fn(), seq: 0, ...over };
}

describe("runFollowAction", () => {
  it("resume bumps operator_resume_seq by one", async () => {
    const put = vi.fn().mockResolvedValue({ ok: true });
    await runFollowAction("resume", deps({ put, seq: 4 }));
    expect(put).toHaveBeenCalledWith("com.altnautica.follow-me", "operator_resume_seq", 5);
  });

  it("rth and land request their flight built-in, with its gate and confirm", async () => {
    const flight = vi.fn(() => true);
    const put = vi.fn();
    expect(await runFollowAction("rth", deps({ flight, put }))).toEqual({ ok: true });
    expect(flight).toHaveBeenCalledWith("rth");
    expect(await runFollowAction("land", deps({ flight, put }))).toEqual({ ok: true });
    expect(flight).toHaveBeenCalledWith("land");
    expect(put).not.toHaveBeenCalled();
  });

  it("a refused built-in is not reported twice: its gate already says why", async () => {
    expect(await runFollowAction("land", deps({ flight: vi.fn(() => false) }))).toEqual({ ok: true });
  });

  it("redesignate arms the pick-a-target mode", async () => {
    const startPick = vi.fn();
    const put = vi.fn();
    const flight = vi.fn(() => true);
    await runFollowAction("redesignate", deps({ put, flight, startPick }));
    expect(startPick).toHaveBeenCalled();
    expect(put).not.toHaveBeenCalled();
    expect(flight).not.toHaveBeenCalled();
  });

  it("stop turns follow off", async () => {
    const put = vi.fn().mockResolvedValue({ ok: true });
    await runFollowAction("stop", deps({ put }));
    expect(put).toHaveBeenCalledWith("com.altnautica.follow-me", "active", false);
  });

  it("reports a failed config write without throwing", async () => {
    const put = vi.fn().mockRejectedValue(new Error("HTTP 503"));
    expect(await runFollowAction("resume", deps({ put }))).toEqual({
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
