import { afterEach, describe, expect, it } from "vitest";

import { requestSkill } from "@/lib/skill-runner";
import { CORE_BY_ID } from "@/lib/skills";
import { useConfirmStore } from "@/stores/confirm-store";

const ctx = { fcConnected: true, live: true, armed: true };

afterEach(() => {
  useConfirmStore.setState({ busy: false, pending: null, guardUntil: null });
});

describe("requestSkill while a command awaits its acknowledgement", () => {
  it("holds ordinary skills off", () => {
    useConfirmStore.setState({ busy: true });
    expect(requestSkill(CORE_BY_ID.land, ctx)).toBe(false);
    expect(useConfirmStore.getState().pending).toBeNull();
  });

  it("still arms the Kill guard", () => {
    useConfirmStore.setState({ busy: true });
    expect(requestSkill(CORE_BY_ID.kill, ctx)).toBe(true);
    expect(useConfirmStore.getState().pending?.skill.id).toBe("kill");
    expect(useConfirmStore.getState().guardUntil).not.toBeNull();
  });
});
