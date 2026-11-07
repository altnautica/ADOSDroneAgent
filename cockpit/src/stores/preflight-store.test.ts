import { beforeEach, describe, expect, it } from "vitest";

import { useCockpitUiStore } from "@/stores/cockpit-ui-store";
import { requirePreflight, usePreflightStore } from "@/stores/preflight-store";

describe("pre-flight checklist gate", () => {
  beforeEach(() => {
    usePreflightStore.setState({ passed: false, pending: null, returnTo: null });
    useCockpitUiStore.setState({ sheet: null });
  });

  it("reopens the Launch sheet when the checklist passes, then launches", () => {
    useCockpitUiStore.getState().openSheet("launch");
    let launched = 0;
    requirePreflight(() => {
      // The launch runs with the Launch sheet back on screen.
      expect(useCockpitUiStore.getState().sheet).toBe("launch");
      launched++;
    });
    expect(useCockpitUiStore.getState().sheet).toBe("preflight");
    usePreflightStore.getState().pass();
    expect(launched).toBe(1);
  });

  it("closes the checklist when it was opened from no sheet", () => {
    requirePreflight(() => undefined);
    usePreflightStore.getState().pass();
    expect(useCockpitUiStore.getState().sheet).toBeNull();
  });

  it("fires directly once passed this session", () => {
    usePreflightStore.setState({ passed: true });
    let n = 0;
    requirePreflight(() => n++);
    expect(n).toBe(1);
    expect(useCockpitUiStore.getState().sheet).toBeNull();
  });
});
