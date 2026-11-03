import { describe, expect, it } from "vitest";

import { hasCommandHandlers, offerCommand, pushCommandHandler } from "@/lib/command-stack";
import type { NavCommand } from "@/nav/navigator";

describe("the overlay command stack", () => {
  it("offers a command to the newest handler first", () => {
    const seen: string[] = [];
    const offA = pushCommandHandler(() => {
      seen.push("a");
      return true;
    });
    const offB = pushCommandHandler(() => {
      seen.push("b");
      return true;
    });
    expect(offerCommand("back")).toBe(true);
    expect(seen).toEqual(["b"]);
    offB();
    offA();
  });

  it("falls through a handler that declines", () => {
    const seen: NavCommand[] = [];
    const offA = pushCommandHandler((cmd) => {
      seen.push(cmd);
      return true;
    });
    const offB = pushCommandHandler((cmd) => cmd === "back");
    expect(offerCommand("next")).toBe(true);
    expect(seen).toEqual(["next"]);
    offB();
    offA();
  });

  it("reports nothing taken once every handler is gone", () => {
    const off = pushCommandHandler(() => true);
    off();
    expect(hasCommandHandlers()).toBe(false);
    expect(offerCommand("activate")).toBe(false);
  });

  it("removes only the handler it registered", () => {
    const offA = pushCommandHandler((cmd) => cmd === "prev");
    const offB = pushCommandHandler((cmd) => cmd === "next");
    offA();
    expect(offerCommand("prev")).toBe(false);
    expect(offerCommand("next")).toBe(true);
    offB();
  });
});
