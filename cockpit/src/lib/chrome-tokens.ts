// The flight screen's panel look from the cockpit spec (`tokens.json`
// `chrome`): a translucent dark panel with a backdrop blur (an opaque
// fallback where blur is unavailable), a hairline border, the corner radius,
// the type scale, the icon size and the spacing. The Feed sets these as CSS
// custom properties on its root; `styles/globals.css` turns them into the
// `chrome-*` classes, so no panel colour is hard-coded.

import type { CSSProperties } from "react";

import tokensJson from "@/shared/cockpit-spec/tokens.json";

export interface ChromeTokens {
  panel_bg: string;
  panel_bg_no_blur: string;
  panel_blur_px: number;
  border: string;
  border_px: number;
  radius_px: number;
  radius_small_px: number;
  text: string;
  text_dim: string;
  severity_ok: string;
  severity_warning: string;
  severity_critical: string;
  stop_bg: string;
  stop_text: string;
  type: {
    top_bar_px: number;
    top_bar_line_px: number;
    value_px: number;
    label_px: number;
    chip_px: number;
    chip_weight: number;
  };
  icon_px: number;
  gap_px: number;
  pad_px: number;
  edge_inset_px: number;
}

export const CHROME_TOKENS: ChromeTokens = tokensJson.chrome as unknown as ChromeTokens;

/** The CSS custom properties the `chrome-*` classes read. */
export function chromeStyle(t: ChromeTokens = CHROME_TOKENS): CSSProperties {
  return {
    "--fs-panel-bg": t.panel_bg,
    "--fs-panel-bg-solid": t.panel_bg_no_blur,
    "--fs-blur": `${t.panel_blur_px}px`,
    "--fs-border": t.border,
    "--fs-border-w": `${t.border_px}px`,
    "--fs-radius": `${t.radius_px}px`,
    "--fs-radius-sm": `${t.radius_small_px}px`,
    "--fs-text": t.text,
    "--fs-text-dim": t.text_dim,
    "--fs-ok": t.severity_ok,
    "--fs-warn": t.severity_warning,
    "--fs-crit": t.severity_critical,
    "--fs-stop-bg": t.stop_bg,
    "--fs-stop-text": t.stop_text,
    "--fs-type-bar": `${t.type.top_bar_px}px`,
    "--fs-type-bar-line": `${t.type.top_bar_line_px}px`,
    "--fs-type-value": `${t.type.value_px}px`,
    "--fs-type-label": `${t.type.label_px}px`,
    "--fs-type-chip": `${t.type.chip_px}px`,
    "--fs-chip-weight": String(t.type.chip_weight),
    "--fs-icon": `${t.icon_px}px`,
    "--fs-gap": `${t.gap_px}px`,
    "--fs-pad": `${t.pad_px}px`,
    "--fs-edge": `${t.edge_inset_px}px`,
  } as CSSProperties;
}

/** The severity colour of a status-pill severity. */
export function severityColor(s: "ok" | "warning" | "critical", t: ChromeTokens = CHROME_TOKENS): string {
  return s === "critical" ? t.severity_critical : s === "warning" ? t.severity_warning : t.severity_ok;
}
