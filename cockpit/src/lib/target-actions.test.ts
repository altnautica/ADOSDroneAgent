// The target menu's confirm gate: a designating plugin action locks the box,
// then turns the follow toggle skill on only through the skill's own confirm
// sheet (never directly); turning it off and Release are taps.

import { beforeEach, describe, expect, it, vi } from "vitest";

import type * as ExtensionsStore from "@/stores/extensions-store";

vi.mock("@/lib/api", () => ({
  designateTarget: vi.fn(),
  putPluginConfig: vi.fn(),
  sendCommand: vi.fn(),
}));
vi.mock("@/stores/extensions-store", async (orig) => ({
  ...(await orig<typeof ExtensionsStore>()),
  activateExtensionSkill: vi.fn(),
}));

import { designateTarget, putPluginConfig } from "@/lib/api";
import { parseExtensionDetail, type TargetAction } from "@/lib/extensions";
import type { SkillContext } from "@/lib/skills";
import { lockTarget, releaseTarget, runTargetAction } from "@/lib/target-actions";
import { useConfirmStore } from "@/stores/confirm-store";
import { activateExtensionSkill, useExtensionsStore } from "@/stores/extensions-store";
import { useNavStreamStore } from "@/stores/nav-stream-store";

const FOLLOW = "com.altnautica.follow-me";
const SKILL_ID = `${FOLLOW}:follow`;
const ext = parseExtensionDetail(FOLLOW, {
  manifest: {
    gcs: {
      contributes: {
        skills: [{ id: "follow", toggle: true, confirm: true, activation: { config_key: "active" }, state: { topic: "follow.state" } }],
        target_actions: [
          { id: "follow", label: "Follow", designate: true, config_key: "active", config_value: true },
          { id: "stop", label: "Stop", config_key: "active", config_value: false },
          { id: "tag", label: "Tag", config_key: "tag", config_value: "x" },
        ],
      },
    },
  },
});
const [followAction, stopAction, tagAction] = ext.targetActions as [TargetAction, TargetAction, TargetAction];
const target = { cameraId: "main", bbox: { x: 1, y: 2, width: 3, height: 4 }, classLabel: "person", confidence: 0.9, trackId: 12 };

function ctx(active: boolean): SkillContext {
  return {
    fcConnected: true,
    live: true,
    armed: true,
    reported: { [SKILL_ID]: { state: active ? "active" : "idle" } },
  };
}

describe("target actions", () => {
  beforeEach(() => {
    vi.mocked(designateTarget).mockReset().mockResolvedValue({ designated: true, track_id: 12 } as never);
    vi.mocked(putPluginConfig).mockReset().mockResolvedValue({});
    vi.mocked(activateExtensionSkill).mockReset().mockResolvedValue(undefined as never);
    useExtensionsStore.setState({ extensions: [ext] });
    useNavStreamStore.getState().clear();
    useConfirmStore.setState({ pending: null, busy: false });
  });

  it("Follow locks the box, then opens the skill's confirm sheet instead of starting", async () => {
    await runTargetAction(followAction, target, ctx(false));
    expect(designateTarget).toHaveBeenCalledWith("main", target.bbox, target);
    expect(useNavStreamStore.getState().designated).toEqual({ cameraId: "main", trackId: 12, bbox: target.bbox });
    expect(useConfirmStore.getState().pending?.skill.id).toBe(SKILL_ID);
    expect(activateExtensionSkill).not.toHaveBeenCalled();
  });

  it("a refused lock never reaches the confirm gate", async () => {
    vi.mocked(designateTarget).mockResolvedValue({ designated: false } as never);
    await runTargetAction(followAction, target, ctx(false));
    expect(useConfirmStore.getState().pending).toBeNull();
    expect(useNavStreamStore.getState().designated).toBeNull();
  });

  it("Follow on an already-active skill opens nothing", async () => {
    await runTargetAction(followAction, target, ctx(true));
    expect(useConfirmStore.getState().pending).toBeNull();
  });

  it("turning the toggle off is a tap", async () => {
    await runTargetAction(stopAction, target, ctx(true));
    expect(useConfirmStore.getState().pending).toBeNull();
    expect(activateExtensionSkill).toHaveBeenCalledWith(FOLLOW, "active", false);
  });

  it("a non-skill action writes the plugin key directly", async () => {
    await runTargetAction(tagAction, target, ctx(false));
    expect(putPluginConfig).toHaveBeenCalledWith(FOLLOW, "tag", "x");
    expect(designateTarget).not.toHaveBeenCalled();
  });

  it("Release clears the designation and stops a running follow", async () => {
    await lockTarget("main", target.bbox);
    await releaseTarget(ctx(true));
    expect(useNavStreamStore.getState().designated).toBeNull();
    expect(activateExtensionSkill).toHaveBeenCalledWith(FOLLOW, "active", false);
  });

  it("Release with follow idle sends nothing", async () => {
    await releaseTarget(ctx(false));
    expect(activateExtensionSkill).not.toHaveBeenCalled();
  });
});
