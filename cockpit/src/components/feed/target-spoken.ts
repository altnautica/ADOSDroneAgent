// A target glyph's spoken label, read by the target layer's accessible names.

import type { GlyphLayout } from "@/shared/hud/target-layout";
import { specText } from "@/shared/hud/spec-text";

/** The glyph's spoken label (`target.chip`): its class and track id, then its
 *  state and tag words. */
export function spokenState(g: GlyphLayout): string {
  const who = [g.item.detection?.classLabel, g.item.trackId].filter((w) => w != null && w !== "").join(" ");
  const words = [g.kind.toLowerCase().replace("_", " "), ...g.item.bottomTags.map((t) => t.text)];
  return specText("target.chip", { who, word: words.join(", ") });
}
