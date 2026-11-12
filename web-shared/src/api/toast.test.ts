import { describe, expect, it } from "vitest";

import { ApiError } from "../api-fetch";
import { onToast, toast, toastFromError, type ToastMessage } from "./toast";

function capture(run: () => void): ToastMessage[] {
  const seen: ToastMessage[] = [];
  const off = onToast((t) => seen.push(t));
  run();
  off();
  return seen;
}

describe("the toast bus", () => {
  it("delivers each toast to the host with its kind", () => {
    const seen = capture(() => {
      toast.ok("Saved");
      toast.err("Failed", "Try again");
      toast.info("Heads up");
    });
    expect(seen.map((t) => [t.kind, t.message])).toEqual([
      ["ok", "Saved"],
      ["err", "Failed"],
      ["info", "Heads up"],
    ]);
    expect(seen[1].description).toBe("Try again");
    expect(new Set(seen.map((t) => t.id)).size).toBe(3);
  });

  it("stops delivering once the host unsubscribes", () => {
    const seen: ToastMessage[] = [];
    const off = onToast((t) => seen.push(t));
    off();
    toast.ok("late");
    expect(seen).toEqual([]);
  });

  it("turns an API refusal into a readable error without the status number", () => {
    const seen = capture(() => toastFromError(new ApiError("409 already paired", 409, null)));
    expect(seen[0]).toMatchObject({ kind: "err", message: "already paired" });
  });

  it("words an armed refusal the same way on every screen", () => {
    const err = new ApiError("409 Vehicle is armed", 409, { error: "E_ARMED", override: "force" });
    const seen = capture(() => toastFromError(err, "Could not save."));
    // The guard also refuses when no flight controller reports the arm state,
    // so the text names that case and how to clear it on a bench.
    expect(seen[0].message).toBe(
      "Refused: the vehicle is armed or its arm state is unknown. Disarm it, or connect the flight controller so it reports disarmed.",
    );
  });

  it("falls back to the given wording for an unknown throw", () => {
    const seen = capture(() => toastFromError("nope", "Could not save."));
    expect(seen[0].message).toBe("Could not save.");
  });
});
