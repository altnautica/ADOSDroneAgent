// The Feed / HUD screen (the default). It is a full-bleed screen: it stacks the
// L0 video layer and the L2 HUD overlay, and the shell floats its chrome (L3)
// translucently on top. This is the pilot's flying view — the ADOS Cockpit
// grammar on the panel.

import { HudLayer } from "@/components/shell/hud-layer";
import { VideoLayer } from "@/components/shell/video-layer";

export function FeedScreen() {
  return (
    <div className="absolute inset-0">
      <VideoLayer />
      <HudLayer />
    </div>
  );
}
