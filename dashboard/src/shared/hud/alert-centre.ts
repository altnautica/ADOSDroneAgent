// The alert centre's surface contract (`alert-rows.tsx` draws it): where a
// row's fix goes on this surface (`AlertActions`), the surface's look
// (`AlertSkin`, one per app), and the snooze set kept in storage
// (`useAlertSnooze` over `alert-snooze.ts`).

import { useEffect, useRef, useState, type ReactNode } from "react";

import tokens from "../cockpit-spec/tokens.json";

import { loadSnoozed, pruneSnoozed, saveSnoozed, withSnoozed } from "./alert-snooze";
import type { PillAlert } from "./status-pill";

type Severity = PillAlert["severity"];

/** Where a row's fix action goes on this surface. */
export interface AlertActions {
  /** Open Aircraft setup (at `step` when given); null on a node with no
   *  setup engine of its own (a ground station): setup fixes then read as a
   *  hint naming where to fix them, with no button. */
  openSetup: ((step?: string) => void) | null;
  /** Open a settings section by its id (`sensors`, `vision`). */
  openSettings: (section: string) => void;
  /** Open the parameter editor at these names; null where the surface has
   *  no parameter editor (the action is left out). */
  openParameters: ((names: string[]) => void) | null;
  /** The surface's own action for an alert that carries no agent fix, or
   *  null. */
  extra?: (a: PillAlert) => ReactNode;
}

/** A surface's look: the cockpit's chrome over video, the dashboard's cards. */
export interface AlertSkin {
  /** A dot or glyph in a severity. */
  dot: (s: Severity) => string;
  /** Words in a severity (a critical title). */
  text: (s: Severity) => string;
  /** Dividers and outlines. */
  border: string;
  /** Secondary words. */
  dim: string;
  /** A secondary button. */
  button: string;
  /** The primary button. */
  primary: string;
  /** The minimum height of every control. */
  control: string;
}

/** The cockpit's chrome skin: tokens over video, 48 px controls. */
export const CHROME_ALERT_SKIN: AlertSkin = {
  dot: (s) => (s === "critical" ? tokens.chrome.severity_critical : tokens.chrome.severity_warning),
  text: (s) => (s === "critical" ? tokens.chrome.text_critical : tokens.chrome.text),
  border: "border-[var(--fs-border)]",
  dim: "opacity-70",
  button: "border border-[var(--fs-border)] bg-white/10 hover:bg-white/15 active:bg-white/20",
  primary: "border border-[var(--fs-border)] bg-white/20 font-semibold hover:bg-white/25 active:bg-white/30",
  control: "min-h-[48px] min-w-[48px]",
};

/** The dashboard's skin: the brand roles. */
export const CARD_ALERT_SKIN: AlertSkin = {
  dot: (s) => (s === "critical" ? "hsl(var(--destructive))" : "hsl(var(--warn))"),
  text: (s) => (s === "critical" ? "hsl(var(--destructive))" : "inherit"),
  border: "border-border",
  dim: "text-muted-foreground",
  button: "border border-border bg-muted/40 hover:bg-muted",
  primary: "bg-primary text-primary-foreground hover:bg-primary-hover",
  control: "min-h-9 max-md:min-h-11",
};

/** How long after mount the snooze store waits before pruning: the first
 *  reads of a page (no pilot frame yet) list nothing a key could match. */
const SNOOZE_SETTLE_MS = 5000;

/** The snooze set for `alerts`, kept in storage. Keys whose alert has
 *  cleared are pruned while `settled` (the surface's sources answer) and
 *  the page has been up a few seconds, so a reload keeps them. */
export function useAlertSnooze(alerts: readonly PillAlert[], settled: boolean) {
  const [snoozed, setSnoozedSet] = useState<ReadonlySet<string>>(loadSnoozed);
  const mountedAt = useRef<number | null>(null);
  useEffect(() => {
    mountedAt.current = Date.now();
  }, []);
  const liveKey = alerts.map((a) => a.key).join("\n");
  useEffect(() => {
    if (!settled || mountedAt.current === null || Date.now() - mountedAt.current < SNOOZE_SETTLE_MS) return;
    setSnoozedSet((s) => {
      const next = pruneSnoozed(s, liveKey ? liveKey.split("\n") : []);
      if (next !== s) saveSnoozed(next);
      return next;
    });
  }, [liveKey, settled]);
  const setSnoozed = (key: string, on: boolean) =>
    setSnoozedSet((s) => {
      const next = withSnoozed(s, key, on);
      saveSnoozed(next);
      return next;
    });
  return { snoozed, setSnoozed };
}
