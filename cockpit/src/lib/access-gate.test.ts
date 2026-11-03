import { describe, expect, it } from "vitest";

import { canSubmitPin, fmtMMSS, lockRemainingS, pinFeedback, pinMismatch, type PinForm } from "@/lib/access-gate";
import { splashMode } from "@/shared/api/pin";

const form = (over: Partial<PinForm>): PinForm => ({ mode: "enter", pin: "", confirm: "", busy: false, locked: false, ...over });

describe("pin form gate", () => {
  it("picks the mode from the status, never guessing set on a failed read", () => {
    expect(splashMode(null)).toBe("unknown");
    expect(splashMode({ pin_set: false, locked: false, locked_until: null })).toBe("set");
    expect(splashMode({ pin_set: true, locked: false, locked_until: null })).toBe("enter");
    expect(splashMode({ pin_set: true, locked: true, locked_until: 10 })).toBe("locked");
  });

  it("submits a valid PIN in enter mode only when not busy or locked", () => {
    expect(canSubmitPin(form({ pin: "1234" }))).toBe(true);
    expect(canSubmitPin(form({ pin: "123" }))).toBe(false);
    expect(canSubmitPin(form({ pin: "12a4" }))).toBe(false);
    expect(canSubmitPin(form({ pin: "1234", busy: true }))).toBe(false);
    expect(canSubmitPin(form({ pin: "1234", locked: true }))).toBe(false);
    expect(canSubmitPin(form({ mode: "unknown", pin: "1234" }))).toBe(false);
  });

  it("needs both entries in set mode, and they must match", () => {
    expect(canSubmitPin(form({ mode: "set", pin: "1234" }))).toBe(false);
    expect(canSubmitPin(form({ mode: "set", pin: "1234", confirm: "1235" }))).toBe(true);
    expect(pinMismatch(form({ mode: "set", pin: "1234", confirm: "1235" }))).toBe(true);
    expect(pinMismatch(form({ mode: "set", pin: "1234", confirm: "1234" }))).toBe(false);
    expect(pinMismatch(form({ mode: "enter", pin: "1234" }))).toBe(false);
  });
});

describe("pin submit feedback", () => {
  it("maps each agent answer", () => {
    expect(pinFeedback({ ok: true })).toEqual({ kind: "unlocked" });
    expect(pinFeedback({ ok: false, kind: "wrong", remaining: 2 })).toEqual({ kind: "wrong", remaining: 2 });
    expect(pinFeedback({ ok: false, kind: "locked", lockedUntil: 100, scope: "node" })).toEqual({
      kind: "locked",
      lockedUntil: 100,
      scope: "node",
    });
    expect(pinFeedback({ ok: false, kind: "invalid", message: "too short" })).toEqual({ kind: "message", message: "too short" });
  });
});

describe("lockout countdown", () => {
  it("counts whole seconds down to zero", () => {
    expect(lockRemainingS(100, 95_000)).toBe(5);
    expect(lockRemainingS(100, 99_100)).toBe(1);
    expect(lockRemainingS(100, 120_000)).toBe(0);
    expect(lockRemainingS(null, 0)).toBe(0);
  });

  it("formats minutes and seconds", () => {
    expect(fmtMMSS(65)).toBe("1:05");
    expect(fmtMMSS(0)).toBe("0:00");
  });
});
