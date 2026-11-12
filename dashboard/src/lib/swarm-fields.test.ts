import { readFileSync } from "node:fs";
import path from "node:path";

import { describe, expect, it } from "vitest";

import { SWARM_MODES, separationOrderOk } from "@/lib/swarm-fields";

const repo = path.resolve(__dirname, "../../..");
const page = readFileSync(path.resolve(__dirname, "../routes/settings/swarm-settings.tsx"), "utf8");
const schema = JSON.parse(readFileSync(path.join(repo, "schemas/agent-config.schema.json"), "utf8"));
const defs = schema.$defs;

describe("swarm settings fields", () => {
  it("writes only keys the agent's swarm block has", () => {
    const keys = [...page.matchAll(/configKey(?:=|: )[{"`]*swarm\.([a-z_.${}]+)/g)].map((m) => m[1]);
    const swarm = defs.SwarmConfig.properties;
    expect(keys).toEqual(expect.arrayContaining(["mode", "separation.radius_m", "separation.hard_m", "flock.${key}", "flock.radius_m", "flock.neighbors"]));
    for (const k of ["mode", "separation", "flock"]) expect(swarm).toHaveProperty(k);
    expect(Object.keys(defs.SwarmSeparationConfig.properties).sort()).toEqual(["hard_m", "radius_m"]);
    expect(Object.keys(defs.SwarmFlockConfig.properties)).toEqual(
      expect.arrayContaining(["cohesion", "alignment", "separation_gain", "radius_m", "neighbors"]),
    );
    expect([...SWARM_MODES]).toEqual(swarm.mode.enum);
  });

  it("keeps the repulsion radius above the hard floor and confirms separation writes", () => {
    expect(separationOrderOk(8, 4)).toBe(true);
    expect(separationOrderOk(4, 4)).toBe(false);
    expect(separationOrderOk(undefined, 4)).toBe(true);
    expect(page).toContain("check={(n) => (separationOrderOk(n, hard) ? null : orderError)}");
    expect(page.match(/confirm=\{confirm\}/g)).toHaveLength(2);
  });
});
