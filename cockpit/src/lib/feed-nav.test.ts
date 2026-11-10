// @vitest-environment happy-dom
// The panel's previous/next on the flight screen move focus through the
// flight controls, STOP first, so STOP is one press away from nothing focused.

import { afterEach, describe, expect, it } from "vitest";

import { feedNavCommand } from "@/lib/feed-actions";
import { useNavStore } from "@/stores/nav-store";

function mount(): HTMLElement {
  document.body.innerHTML = `
    <div data-feed-root>
      <button data-zone="menu">Menu</button>
      <span data-zone="stop"><button id="stop">STOP</button></span>
      <button id="land">Land</button>
    </div>`;
  return document.querySelector<HTMLElement>("[data-feed-root]")!;
}

afterEach(() => {
  document.body.innerHTML = "";
  useNavStore.setState({ quickMenuOpen: false });
});

describe("panel focus on the flight screen", () => {
  it.each(["next", "prev"] as const)("%s from nothing focused lands on STOP", (cmd) => {
    mount();
    expect(feedNavCommand(cmd)).toBe(true);
    expect(document.activeElement?.id).toBe("stop");
  });

  it("steps through the other flight controls and wraps", () => {
    mount();
    feedNavCommand("next");
    feedNavCommand("next");
    expect(document.activeElement?.getAttribute("data-zone")).toBe("menu");
    feedNavCommand("next");
    expect(document.activeElement?.id).toBe("land");
    feedNavCommand("next");
    expect(document.activeElement?.id).toBe("stop");
  });

  it("activate clicks the focused control, else opens the Menu", () => {
    const root = mount();
    let clicks = 0;
    root.querySelector("#land")!.addEventListener("click", () => (clicks += 1));
    feedNavCommand("activate");
    expect(useNavStore.getState().quickMenuOpen).toBe(true);
    useNavStore.setState({ quickMenuOpen: false });
    (root.querySelector("#land") as HTMLElement).focus();
    feedNavCommand("activate");
    expect(clicks).toBe(1);
    expect(useNavStore.getState().quickMenuOpen).toBe(false);
  });
});
