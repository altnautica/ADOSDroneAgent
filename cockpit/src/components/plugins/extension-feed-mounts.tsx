// Extension surfaces on the Feed, each gated on the matching granted slot
// capability:
//   video.overlay  — a full-feed draw layer above the video (z 10); it does not
//                    take input, so taps reach the controls beneath
//   cockpit.widget — a small frame in its declared HUD corner (z 20)
// `cockpit.panel` frames open from the quick menu instead.

import type { ExtensionPanel, InstalledExtension } from "@/lib/extensions";
import { cn } from "@/lib/utils";
import { useExtensionsStore } from "@/stores/extensions-store";

import { PluginFrameHost } from "@/components/plugins/plugin-overlay-host";

/** The slot capability each mount needs. */
export const SLOT_CAPABILITY: Record<string, string> = {
  "video.overlay": "ui.slot.video-overlay",
  "cockpit.widget": "ui.slot.cockpit-widget",
  "cockpit.panel": "ui.slot.cockpit-panel",
};

/** Panels of `slot` the extension is allowed to mount. */
export function mountablePanels(ext: InstalledExtension, slot: string): ExtensionPanel[] {
  const cap = SLOT_CAPABILITY[slot];
  if (!cap || !ext.grantedCapabilities.includes(cap)) return [];
  return ext.panels.filter((p) => p.slot === slot);
}

const CORNER_CLASS: Record<string, string> = {
  "top-left": "left-[0.5rem] top-[6rem]",
  "top-right": "right-[0.5rem] top-[6rem]",
  "bottom-left": "left-[0.5rem] bottom-[11rem]",
  "bottom-right": "right-[0.5rem] bottom-[11rem]",
};

export function ExtensionFeedMounts() {
  const extensions = useExtensionsStore((s) => s.extensions);
  const overlays = extensions.flatMap((ext) => mountablePanels(ext, "video.overlay").map((p) => ({ ext, p })));
  const widgets = extensions.flatMap((ext) => mountablePanels(ext, "cockpit.widget").map((p) => ({ ext, p })));

  return (
    <>
      {overlays.map(({ ext, p }) => (
        <div key={`${ext.pluginId}:${p.id}`} className="pointer-events-none absolute inset-0 z-10">
          <PluginFrameHost ext={ext} title={p.title} className="h-full w-full" />
        </div>
      ))}
      {widgets.map(({ ext, p }) => (
        <div
          key={`${ext.pluginId}:${p.id}`}
          className={cn(
            "pointer-events-auto absolute z-20 h-[9rem] w-[14rem] overflow-hidden rounded-xl border border-hud-hair bg-hud-glass backdrop-blur-hud",
            CORNER_CLASS[p.zone ?? ""] ?? CORNER_CLASS["bottom-left"],
          )}
        >
          <PluginFrameHost ext={ext} title={p.title} className="h-full w-full" />
        </div>
      ))}
    </>
  );
}
