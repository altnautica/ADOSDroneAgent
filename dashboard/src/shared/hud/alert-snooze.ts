// Alert snooze: the alert keys (`PillAlert.key`, `id|text`) the operator hid
// from the alert list. A snoozed warning leaves the list and the pill count;
// a snoozed critical only folds. A key whose alert has cleared is pruned on
// the next update, so an alert that clears and comes back shows again.
// Kept per device in localStorage `ados-alert-snooze` (a JSON array of keys).
// Pure apart from storage: no framework imports.

export const SNOOZE_STORAGE_KEY = "ados-alert-snooze";

/** The snoozed keys on this device; empty when storage is blocked or holds
 *  anything but an array of strings. */
export function loadSnoozed(): Set<string> {
  if (typeof localStorage === "undefined") return new Set();
  try {
    const raw = localStorage.getItem(SNOOZE_STORAGE_KEY);
    const v: unknown = raw ? JSON.parse(raw) : [];
    return new Set(Array.isArray(v) ? v.filter((k): k is string => typeof k === "string") : []);
  } catch {
    return new Set();
  }
}

/** Store the snoozed keys; a blocked or full storage keeps them for this
 *  page only. */
export function saveSnoozed(s: ReadonlySet<string>): void {
  if (typeof localStorage === "undefined") return;
  try {
    localStorage.setItem(SNOOZE_STORAGE_KEY, JSON.stringify([...s]));
  } catch {
    // storage blocked or full
  }
}

/** The snoozed keys still live; the same set when nothing was pruned, so a
 *  caller can skip a store write or a re-render. */
export function pruneSnoozed(s: ReadonlySet<string>, liveKeys: Iterable<string>): ReadonlySet<string> {
  const live = new Set(liveKeys);
  let pruned = false;
  for (const k of s) {
    if (!live.has(k)) {
      pruned = true;
      break;
    }
  }
  return pruned ? new Set([...s].filter((k) => live.has(k))) : s;
}

/** `s` with `key` snoozed (`on`) or shown again. */
export function withSnoozed(s: ReadonlySet<string>, key: string, on: boolean): Set<string> {
  const next = new Set(s);
  if (on) next.add(key);
  else next.delete(key);
  return next;
}
