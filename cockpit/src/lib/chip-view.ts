// The flight-state chip's parts beyond the shared view: the inline button's
// words and action, the extra reduced-rate line, the full text screen
// readers get as the chip's label, and the blocking readiness rows its
// detail popover lists.

import { openAircraftSetup } from "@/lib/aircraft-setup";
import { runCockpitAction } from "@/lib/cockpit-actions";
import { CHROME_TOKENS } from "@/lib/chrome-tokens";
import type { ReadinessItem } from "@/shared/hud/cockpit-frames";
import { CHIP_COMPACT_PARTS, type ChipView } from "@/shared/hud/flight-state";
import { specText } from "@/shared/hud/spec-text";
import { pillAlerts, severityTextColor, type PillAlert } from "@/shared/hud/status-pill";
import { abortLaunch } from "@/stores/cockpit-ui-store";

/** The chip's inline button: its words and what a press does. */
export function chipButton(button: ChipView["button"]): { label: string; run: () => void } | null {
  switch (button) {
    case "take_control":
      return { label: specText("flow.take_control"), run: () => runCockpitAction("take_control") };
    case "resume":
      return { label: specText("flow.resume"), run: () => runCockpitAction("resume") };
    case "abort":
      return { label: specText("flow.launch.abort"), run: abortLaunch };
    case "setup":
      return { label: specText("setup.wizard.entry.set_up_aircraft"), run: () => openAircraftSetup() };
    default:
      return null;
  }
}

/** The chip's secondary line, plus the reduced-rate note while the nav
 *  stream has fallen back to the radio. The compact face keeps only the
 *  parts the sticks need: clearance, a late input, the speeds. */
export function chipLines(chip: ChipView, radioFallback: boolean, compact = false): { text: string; color: string }[] {
  const parts = compact ? chip.secondary.filter((l) => CHIP_COMPACT_PARTS[l.id]) : chip.secondary;
  const lines = parts.map((l) => ({ text: l.text, color: l.color }));
  if (radioFallback && !compact) lines.push({ text: specText("nav.reduced_rate"), color: CHROME_TOKENS.severity_warning });
  return lines;
}

/** The text colour of a chip line. A state hue is for the chip's dot and
 *  outline only; as small text it is too dark on the panel. A critical hue
 *  reads as `text_critical`, a warning hue as the warning text, anything
 *  else in the dim text colour. */
export function chipLineTextColor(color: string): string {
  const c = color.toUpperCase();
  if (c === CHROME_TOKENS.severity_critical.toUpperCase()) return severityTextColor("critical");
  if (c === CHROME_TOKENS.severity_warning.toUpperCase()) return severityTextColor("warning");
  return CHROME_TOKENS.text_dim;
}

/** The chip's whole text, read by screen readers as its label. */
export function chipFullText(text: string, lines: readonly { text: string }[]): string {
  return specText("chip.full_text", { text: [text, ...lines.map((l) => l.text)].join(" · ") });
}

/** The readiness items that block take-off, as the alert centre's rows
 *  (parameter table and fix included). */
export function chipBlockingAlerts(readiness: readonly ReadinessItem[]): PillAlert[] {
  return pillAlerts({ alerts: [], readiness, prearm: undefined, storagePct: null }).filter((a) => a.severity === "critical");
}
