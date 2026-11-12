// The flight-state chip's parts beyond the shared view: the inline button's
// words and action, the extra reduced-rate line, and the full text the chip
// shows on hover or tap once it is truncated.

import { openAircraftSetup } from "@/lib/aircraft-setup";
import { runCockpitAction } from "@/lib/cockpit-actions";
import { CHROME_TOKENS } from "@/lib/chrome-tokens";
import type { ChipView } from "@/shared/hud/flight-state";
import { specText } from "@/shared/hud/spec-text";
import { severityTextColor } from "@/shared/hud/status-pill";
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
 *  stream has fallen back to the radio. */
export function chipLines(chip: ChipView, radioFallback: boolean): { text: string; color: string }[] {
  const lines = chip.secondary.map((l) => ({ text: l.text, color: l.color }));
  if (radioFallback) lines.push({ text: specText("nav.reduced_rate"), color: CHROME_TOKENS.severity_warning });
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

/** The chip's whole text, as its tooltip and its tap panel read it. */
export function chipFullText(text: string, lines: readonly { text: string }[]): string {
  return specText("chip.full_text", { text: [text, ...lines.map((l) => l.text)].join(" · ") });
}
