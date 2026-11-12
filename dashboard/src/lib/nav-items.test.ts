import { readFileSync } from "node:fs";
import path from "node:path";

import { describe, expect, it } from "vitest";

import { navGroups } from "./nav-items";
import { ROUTES, SETTINGS_PREFIX } from "@/route-table";
import { gateDecision } from "./node-identity";

const paths = (items: { to: string }[]) => items.map((i) => i.to);
const read = (f: string) => readFileSync(path.resolve(__dirname, "..", f), "utf8");

describe("sidebar", () => {
  it.each([
    ["drone", "direct"],
    ["visor", "direct"],
    ["ground_station", "direct"],
    ["ground_station", "relay"],
    ["ground_station", "receiver"],
  ] as const)("lists exactly the pages the route table opens to %s/%s", (profile, role) => {
    const expected = ROUTES.filter(
      (r) =>
        !r.path.startsWith(SETTINGS_PREFIX) &&
        r.profiles !== null &&
        gateDecision(profile, role, r.profiles, r.roles ?? undefined) === "pass",
    ).map((r) => r.path);
    expect(paths(navGroups(profile, role).profileItems)).toEqual(expected);
  });

  it("gives a visor its GNSS page and no LoRa page", () => {
    const items = paths(navGroups("visor", "direct").profileItems);
    expect(items).toContain("/gnss");
    expect(items).not.toContain("/lora");
  });

  it("lists no profile pages before the profile is known", () => {
    expect(navGroups("auto", "direct").profileItems).toEqual([]);
  });

  it("links Home at / and redirects the table's /home there", () => {
    expect(paths(navGroups("drone", "direct").top)).toEqual(["/"]);
    expect(read("App.tsx")).toMatch(/if \(entry\.key === "home"\) return <Navigate to="\/" replace \/>/);
    expect(read("components/layout/banner-host.tsx")).toMatch(/pathname === "\/"/);
  });
});
