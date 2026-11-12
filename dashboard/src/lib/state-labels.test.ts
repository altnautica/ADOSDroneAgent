import { readFileSync } from "node:fs";
import path from "node:path";

import { describe, expect, it } from "vitest";

import {
  gnssBaseLabel,
  gnssFixLabel,
  loraSourceLabel,
  serviceStateLabel,
  serviceStateTone,
  storageStateLabel,
  videoStateLabel,
  wfbStateLabel,
} from "@/lib/state-labels";
import { issueMessage } from "@/lib/validation-message";
import { cloudModeLabel, nodeSubject } from "@/lib/enum-labels";

const src = (rel: string) => readFileSync(path.resolve(__dirname, "..", rel), "utf8");

describe("state codes read as words", () => {
  it("names service, WFB, GNSS, video, storage and LoRa states", () => {
    expect(serviceStateLabel("activating")).toBe("Starting");
    expect(serviceStateLabel("ACTIVE")).toBe("Running");
    expect(wfbStateLabel("active")).toBe("Active");
    expect(gnssFixLabel("rtk_fixed")).toBe("RTK fixed");
    expect(gnssFixLabel("3d")).toBe("3D fix");
    expect(gnssBaseLabel("fixed")).toBe("Fixed");
    expect(videoStateLabel("no_camera")).toBe("No camera");
    expect(storageStateLabel("near_full")).toBe("Nearly full");
    expect(loraSourceLabel("network.regulatory.region")).toBe("from the country");
  });

  it("never echoes an unknown code", () => {
    for (const f of [serviceStateLabel, wfbStateLabel, gnssFixLabel, videoStateLabel, storageStateLabel]) {
      expect(f("weird_code")).toBe("Unrecognised");
    }
  });

  it("tones a unit on its way up as a warning, not an error", () => {
    expect(serviceStateTone("activating")).toBe("warn");
    expect(serviceStateTone("failed")).toBe("err");
  });

  it("routes every panel's state through the label maps", () => {
    expect(src("components/panels/services-panel.tsx")).toContain("serviceStateLabel(s.state)");
    expect(src("routes/diagnostics-route.tsx")).toContain("serviceStateLabel(svc.state)");
    const gnss = src("components/panels/gnss-panel.tsx");
    expect(gnss).toContain("gnssFixLabel(r.fix)");
    expect(gnss).toContain("gnssBaseLabel(g.base.state)");
    const lora = src("components/panels/lora-panel.tsx");
    expect(lora).toContain("loraSourceLabel(status.region.source)");
    expect(lora).not.toContain('r.rules.join(",")');
    expect(src("components/panels/video-panel.tsx")).toContain("wfbStateLabel(wfbState)");
    expect(src("lib/status-tiles.ts")).toContain("videoStateLabel(vState)");
    expect(src("routes/settings/capture-settings.tsx")).toContain("storageStateLabel(s.capture.state)");
  });
});

describe("form and posture words", () => {
  it("names the field a schema check failed on, without the wire path", () => {
    expect(issueMessage({ path: ["self_hosted", "url"] })).toBe("Enter the full server URL, starting with https://.");
    expect(issueMessage({ path: ["board_override"] })).not.toContain("board_override");
    for (const f of ["routes/settings/cloud-settings.tsx", "routes/settings/advanced-settings.tsx"]) {
      expect(src(f)).toContain("setValidationError(issueMessage(first))");
      expect(src(f)).not.toContain("first.path.join");
    }
  });

  it("names the cloud posture and the node in words", () => {
    expect(cloudModeLabel("self_hosted")).not.toContain("_");
    expect(nodeSubject("ground_station")).toBe("ground station");
    expect(src("routes/settings/cloud-settings.tsx")).toContain("values={{ mode: cloudModeLabel(mode) }}");
  });
});
