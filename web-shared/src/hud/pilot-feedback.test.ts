// Controller rumble follows the cockpit spec's feedback table
// (`ds4.json` `feedback`), the same rules the ground station's controller,
// Mission Control and Android run.

import { describe, expect, it } from "vitest";

import { feedbackAlert } from "./pilot-feedback";

/** The rumble one pilot-state update earns. */
function alert(prev: Record<string, unknown> | null, next: Record<string, unknown> | null): string | null {
  return feedbackAlert(prev, next);
}

function flying(extra: Record<string, unknown> = {}): Record<string, unknown> {
  return {
    owner: "pilot", profile: "normal", armed: true, landed: false, skill: null, control_mode: "ados",
    hold_reason: null, avoid_state: "clear", link_loss_in_s: null, rth_countdown_s: null, readiness: [],
    input_session: { src: "gs", session: 7, label: "Ground station" },
    ...extra,
  };
}

const a = flying();

describe("pad rumble from pilot-state changes", () => {
  it("rumbles a mode change on a profile change, an owner change or a takeover", () => {
    expect(alert(a, flying({ profile: "sport" }))).toBe("mode_change");
    expect(alert(a, flying({ owner: "goto" }))).toBe("mode_change");
    expect(alert(a, flying({ input_session: { src: "mc", session: 9, label: "MC" } }))).toBe("mode_change");
  });

  it("warns on avoiding or blocked, a new hold reason, a speed cap or a warn item", () => {
    expect(alert(a, flying({ avoid_state: "detour" }))).toBe("warning");
    expect(alert(flying({ avoid_state: "detour" }), flying({ avoid_state: "blocked" }))).toBe("warning");
    expect(alert(a, flying({ hold_reason: "obstacle-close" }))).toBe("warning");
    const capped = flying({ speed_cap: { reason: "gnss-denied", v_h_mps: 3 } });
    expect(alert(a, capped)).toBe("warning");
    expect(alert(capped, capped)).toBeNull();
    expect(alert(a, flying({ readiness: [{ id: "gps", ok: false, severity: "warn", msg: "" }] }))).toBe("warning");
  });

  it("is critical on a lost controller, a link-loss countdown, a failsafe or a lost pilot state", () => {
    expect(alert(a, flying({ hold_reason: "controller-lost" }))).toBe("critical");
    const link = flying({ link_loss_in_s: 9 });
    expect(alert(a, link)).toBe("critical");
    expect(alert(link, flying({ link_loss_in_s: 8 }))).toBeNull();
    expect(alert(a, flying({ fc_failsafe: true, owner: "fc_mode", fc_mode: 6 }))).toBe("critical");
    expect(alert(a, flying({ readiness: [{ id: "ekf", ok: false, severity: "block", msg: "" }] }))).toBe("critical");
    expect(alert(a, null)).toBe("critical");
    expect(alert(null, a)).toBeNull();
    expect(alert(a, flying({ profile: "sport", hold_reason: "controller-lost" }))).toBe("critical");
  });

  it("keeps a failing block item on the ground a pre-flight item", () => {
    const ground = flying({ armed: false, landed: true });
    expect(alert(ground, { ...ground, readiness: [{ id: "ekf", ok: false, severity: "block", msg: "" }] })).toBeNull();
  });
});
