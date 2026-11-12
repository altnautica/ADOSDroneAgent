import { readFileSync } from "node:fs";
import path from "node:path";

import { describe, expect, it } from "vitest";

import { RESTART_TIMEOUT_MS, restartElapsedS, restartPhase, type RestartPending } from "@/lib/restart-pending";

const src = (rel: string) => readFileSync(path.resolve(__dirname, "..", rel), "utf8");
const at = 1_000_000;
const pending = (over: Partial<RestartPending> = {}): RestartPending => ({
  kind: "reboot",
  since: at,
  baselineUptimeS: 3_600,
  sawDown: false,
  ...over,
});

describe("a restart the operator asked for", () => {
  it("waits while the old process still answers", () => {
    expect(restartPhase(pending(), at + 2_000, { reachable: true, uptimeS: 3_602 })).toBe("waiting");
    expect(restartPhase(pending(), at + 10_000, { reachable: false, uptimeS: null })).toBe("waiting");
  });

  it("is back once the agent answers after going silent", () => {
    expect(restartPhase(pending({ sawDown: true }), at + 40_000, { reachable: true, uptimeS: 3_700 })).toBe("back");
  });

  it("is back when the uptime restarted between two polls", () => {
    expect(restartPhase(pending(), at + 20_000, { reachable: true, uptimeS: 5 })).toBe("back");
  });

  it("gives up after the timeout", () => {
    expect(restartPhase(pending(), at + RESTART_TIMEOUT_MS + 1, { reachable: false, uptimeS: null })).toBe("timedOut");
    expect(restartElapsedS(pending(), at + 12_500)).toBe(12);
  });

  it("starts from reboot and from a profile apply, and replaces the unreachable advice", () => {
    expect(src("routes/diagnostics-route.tsx")).toContain('beginRestart("reboot"');
    expect(src("routes/settings/profile-settings.tsx")).toContain('beginRestart("profile"');
    const host = src("components/layout/banner-host.tsx");
    expect(host).toContain("<RestartBanner />");
    expect(host).toContain("agentUnreachable: heartbeat.isError && !restarting");
    expect(src("routes/home.tsx")).toContain('t("layout.restart.offlineBody")');
  });
});
