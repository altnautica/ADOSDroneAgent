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
  /** Visual height of a top-bar control; its hit area stays at least 48. */
  control_px: number;
  text: string;
  text_dim: string;
  severity_ok: string;
  severity_warning: string;
  severity_critical: string;
  /** The alert list's item glyph per severity. */
  severity_icon: { critical: string; warning: string; info: string };
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
    "--fs-control": `${t.control_px}px`,
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

/** A flight-screen popover under `anchor` (window px): the chrome tokens at
 *  the chrome scale, as a CSS zoom like the zones' (`lib/ui-scale.ts`), so
 *  the popover reads at the size of the bar it opens from. `widthPx` is the
 *  unscaled width, never wider than the window less 8 px a side; the panel's
 *  left edge sits on the anchor's (`align` "left") or its right edge on the
 *  anchor's right, kept on screen. CSS zoom scales the panel's own offsets,
 *  so they are given unscaled. `maxHeight` (unscaled px) is the room under
 *  the anchor, for the panel's scroller. */
export function popoverStyle(
  anchor: DOMRect,
  widthPx: number,
  scale: number,
  align: "left" | "right" = "left",
): { panel: CSSProperties; maxHeight: number } {
  const margin = 8;
  const w = Math.min(widthPx * scale, window.innerWidth - 2 * margin);
  const x = align === "left" ? anchor.left : anchor.right - w;
  const left = Math.max(margin, Math.min(x, window.innerWidth - w - margin));
  const top = anchor.bottom + margin;
  return {
    panel: {
      ...chromeStyle(),
      position: "absolute",
      zoom: scale === 1 ? undefined : scale,
      left: left / scale,
      top: top / scale,
      width: w / scale,
    },
    maxHeight: Math.max(0, window.innerHeight - top - margin) / scale,
  };
}

/** The severity colour of a status-pill severity. */
export function severityColor(s: "ok" | "warning" | "critical", t: ChromeTokens = CHROME_TOKENS): string {
  return s === "critical" ? t.severity_critical : s === "warning" ? t.severity_warning : t.severity_ok;
}

/** The alert list's item glyph (`chrome.severity_icon`): ✕ critical,
 *  ⚠ warning, ℹ anything else. */
export function severityIcon(s: string, t: ChromeTokens = CHROME_TOKENS): string {
  return s === "critical" ? t.severity_icon.critical : s === "warning" ? t.severity_icon.warning : t.severity_icon.info;
}
