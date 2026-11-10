import { beforeEach, describe, expect, it, vi } from "vitest";

import tipsJson from "@/shared/cockpit-spec/tips.json";
import { TIPS_KEY, useTipsStore, type TipsScreen } from "@/stores/tips-store";

const FIRST = tipsJson.rules.first_delay_ms;
const GAP = tipsJson.rules.gap_between_ms;
const AUTO = tipsJson.rules.auto_dismiss_ms;

const mem = new Map<string, string>();
vi.stubGlobal("localStorage", {
  getItem: (k: string) => mem.get(k) ?? null,
  setItem: (k: string, v: string) => void mem.set(k, v),
  removeItem: (k: string) => void mem.delete(k),
});

const ALL_ANCHORS = {
  status_pill: true,
  flight_state_chip: true,
  layer_button: true,
  stop: true,
  skill_picker_chip: true,
  bottom_left_card: true,
  gimbal_pad: true,
};

function ground(over: Partial<TipsScreen> = {}): TipsScreen {
  return {
    armed: false,
    airborne: false,
    flightScreen: true,
    sheetOrMenuOpen: false,
    tourVisible: false,
    skillsAvailable: false,
    gotoOffered: false,
    gimbalAvailable: false,
    anchorsVisible: ALL_ANCHORS,
    ...over,
  };
}

const tips = () => useTipsStore.getState();

beforeEach(() => {
  mem.clear();
  tips().reload();
  tips().flightScreenClosed();
});

describe("tips store", () => {
  it("shows the first tip after the first delay, then the next after the gap", () => {
    tips().flightScreenOpened(0);
    expect(tips().tick(ground(), FIRST - 1).kind).toBe("wait");
    expect(tips().tick(ground(), FIRST)).toMatchObject({ kind: "show", tipId: "status_pill", textKey: "tips.status_pill" });
    tips().dismissShown(FIRST + 1000);
    expect(tips().tips.seen).toEqual(["status_pill"]);
    expect(tips().tick(ground(), FIRST + 1000 + GAP - 1).kind).toBe("wait");
    expect(tips().tick(ground(), FIRST + 1000 + GAP)).toMatchObject({ kind: "show", tipId: "flight_state_chip" });
  });

  it("persists the seen set and the open count per device", () => {
    tips().flightScreenOpened(0);
    tips().tick(ground(), FIRST);
    tips().dismissShown(FIRST);
    const saved = JSON.parse(mem.get(TIPS_KEY) ?? "{}") as { seen: string[]; open_count: number };
    expect(saved).toEqual({ seen: ["status_pill"], open_count: 1 });
    tips().reload();
    expect(tips().tips).toEqual({ seen: ["status_pill"], openCount: 1 });
  });

  it("arming hides the tip and marks it seen, and nothing shows while armed", () => {
    tips().flightScreenOpened(0);
    tips().tick(ground(), FIRST);
    expect(tips().tick(ground({ armed: true }), FIRST + 100)).toMatchObject({ kind: "hide", tipId: "status_pill", markSeen: true, reason: "arm" });
    expect(tips().tips.seen).toContain("status_pill");
    for (let t = FIRST + 200; t < FIRST + 30_000; t += 500) {
      expect(tips().tick(ground({ armed: true, airborne: true }), t).kind).toBe("none");
    }
  });

  it("auto-dismiss hides without marking seen, and the tip returns on the next open", () => {
    tips().flightScreenOpened(0);
    tips().tick(ground(), FIRST);
    expect(tips().tick(ground(), FIRST + AUTO)).toMatchObject({ kind: "hide", markSeen: false, reason: "auto_dismiss" });
    expect(tips().tips.seen).toEqual([]);
    // Not again on this open: the next tip follows after the gap.
    expect(tips().tick(ground(), FIRST + AUTO + GAP)).toMatchObject({ kind: "show", tipId: "flight_state_chip" });
    tips().flightScreenClosed();
    tips().flightScreenOpened(100_000);
    expect(tips().tick(ground(), 100_000 + FIRST)).toMatchObject({ kind: "show", tipId: "status_pill" });
  });

  it("waits while a sheet, a menu or the controller tour is up, and skips an absent anchor", () => {
    tips().flightScreenOpened(0);
    expect(tips().tick(ground({ sheetOrMenuOpen: true }), FIRST).kind).toBe("none");
    expect(tips().tick(ground({ tourVisible: true }), FIRST).kind).toBe("none");
    expect(tips().tick(ground({ anchorsVisible: { ...ALL_ANCHORS, status_pill: false } }), FIRST)).toMatchObject({
      kind: "show",
      tipId: "flight_state_chip",
    });
    expect(tips().tips.seen).toEqual([]);
  });

  it("offers the trigger tips once their trigger fires, and the card tip on the second open", () => {
    mem.set(TIPS_KEY, JSON.stringify({ seen: ["status_pill", "flight_state_chip", "layer_level", "stop"], open_count: 0 }));
    tips().reload();
    tips().flightScreenOpened(0);
    expect(tips().tick(ground(), FIRST).kind).toBe("none");
    expect(tips().tick(ground({ gimbalAvailable: true, gotoOffered: true, skillsAvailable: true }), FIRST)).toMatchObject({ tipId: "skills_chip" });
    tips().dismissShown(FIRST);
    expect(tips().tick(ground({ gotoOffered: true }), FIRST + GAP)).toMatchObject({ tipId: "fly_here" });
    tips().dismissShown(FIRST + GAP);
    expect(tips().tick(ground({ gimbalAvailable: true }), FIRST + 2 * GAP)).toMatchObject({ tipId: "gimbal_pad" });
    tips().dismissShown(FIRST + 2 * GAP);
    expect(tips().tick(ground(), FIRST + 3 * GAP).kind).toBe("none");
    tips().flightScreenClosed();
    tips().flightScreenOpened(50_000);
    expect(tips().tick(ground(), 50_000 + FIRST)).toMatchObject({ tipId: "card_switch" });
  });

  it("reset clears the seen set and the open count", () => {
    tips().flightScreenOpened(0);
    tips().tick(ground(), FIRST);
    tips().dismissShown(FIRST);
    tips().reset();
    expect(tips().tips).toEqual({ seen: [], openCount: 0 });
    expect(JSON.parse(mem.get(TIPS_KEY) ?? "{}")).toEqual({ seen: [], open_count: 0 });
  });
});
