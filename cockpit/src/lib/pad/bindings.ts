// The binding model behind Settings › Controls: conflict detection, the
// locked-binding rule, the editor's operations and the export/import file.
// The export file is `ds4.json`'s `export_format`: the COMPLETE effective
// binding list plus the stick mode and deadzone (the ground station's native
// input applies the same file through `PUT /api/v1/ground-station/input/bindings`).

import {
  BUTTON_CONTROLS,
  DEADZONE_DEFAULT,
  DEADZONE_MAX,
  DEADZONE_MIN,
  DEFAULT_BINDINGS,
  DEFAULT_STICK_MODE,
  EXPORT_KIND,
  EXPORT_VERSION,
  LOCKED_ACTIONS,
  SPEC_ACTION_BY_ID,
  effectiveHoldMs,
  isControlId,
  isGesture,
  isRumbleKind,
  isWhenCondition,
  type ControlId,
  type Gesture,
  type LockedAction,
  type PadBinding,
  type StickMode,
  type WhenCondition,
} from "@/lib/pad/ds4-map";

export type PadKind = "ds4" | "generic";
export type ControlsProfile = "ados_default" | "user";

export interface ControlsDoc {
  v: number;
  kind: string;
  pad: PadKind;
  stick_mode: StickMode;
  deadzone: number;
  profile: ControlsProfile;
  bindings: PadBinding[];
}

export function cloneBindings(rows: readonly PadBinding[]): PadBinding[] {
  return rows.map((b) => ({ ...b, with: [...b.with] }));
}

export const defaultBindings = (): PadBinding[] => cloneBindings(DEFAULT_BINDINGS);

function sameRow(a: PadBinding, b: PadBinding): boolean {
  return (
    a.control === b.control &&
    a.gesture === b.gesture &&
    a.hold_ms === b.hold_ms &&
    a.when === b.when &&
    a.action === b.action &&
    a.rumble === b.rumble &&
    a.locked === b.locked &&
    a.with.length === b.with.length &&
    a.with.every((w, i) => w === b.with[i])
  );
}

export function isDefaultBindings(rows: readonly PadBinding[]): boolean {
  return rows.length === DEFAULT_BINDINGS.length && rows.every((b, i) => sameRow(b, DEFAULT_BINDINGS[i]));
}

// ── conflicts ────────────────────────────────────────────────────────────────

/** Pairs of conditions that never hold together, so two rows on one slot that
 *  carry them do not conflict (D-pad ↑ param vs layer level, Triangle hold
 *  takeoff vs land, PS pause vs cancel RTH). */
const EXCLUSIVE: ReadonlyArray<readonly [WhenCondition, WhenCondition]> = [
  ["picker_open", "picker_closed"],
  ["on_ground", "airborne"],
  ["rth_active", "not_rth"],
];

function whensOverlap(a: WhenCondition | null, b: WhenCondition | null): boolean {
  if (a === null || b === null || a === b) return true;
  return !EXCLUSIVE.some(([x, y]) => (a === x && b === y) || (a === y && b === x));
}

function slotKey(b: PadBinding): string {
  return `${[b.control, ...[...b.with].sort()].join("+")}:${b.gesture}`;
}

export interface BindingConflict {
  /** Indices into the binding list. */
  rows: [number, number];
  reason: "same_slot" | "same_action";
}

/** Conflicts: the same control (and combo) and gesture bound twice under
 *  conditions that can hold together, or one action bound to two single
 *  controls with the same gesture. Combos may repeat an action (Options +
 *  Circle is PS's documented fallback). */
export function findConflicts(rows: readonly PadBinding[]): BindingConflict[] {
  const out: BindingConflict[] = [];
  for (let i = 0; i < rows.length; i++) {
    const a = rows[i];
    if (a.action === "none") continue;
    for (let j = i + 1; j < rows.length; j++) {
      const b = rows[j];
      if (b.action === "none") continue;
      if (slotKey(a) === slotKey(b) && whensOverlap(a.when, b.when)) {
        out.push({ rows: [i, j], reason: "same_slot" });
      } else if (
        a.action === b.action &&
        a.gesture === b.gesture &&
        a.with.length === 0 &&
        b.with.length === 0 &&
        a.control !== b.control &&
        whensOverlap(a.when, b.when)
      ) {
        out.push({ rows: [i, j], reason: "same_action" });
      }
    }
  }
  return out;
}

/** The controls a conflict touches (each row's control and combo members). */
export function conflictedControls(rows: readonly PadBinding[], conflicts: readonly BindingConflict[]): Set<ControlId> {
  const set = new Set<ControlId>();
  for (const c of conflicts) {
    for (const i of c.rows) {
      set.add(rows[i].control);
      for (const w of rows[i].with) set.add(w);
    }
  }
  return set;
}

// ── locked bindings ──────────────────────────────────────────────────────────

const isPressable = (c: ControlId) => BUTTON_CONTROLS.includes(c);

/** The default row a locked action ships with: its hold time and `when` gate
 *  are part of the safety rule (arm 2 s with the left stick centred, kill
 *  3 s), so a moved row must keep them. */
function lockedDefault(l: LockedAction): PadBinding | undefined {
  return DEFAULT_BINDINGS.find((b) => b.locked && b.action === l.action && b.gesture === l.gesture);
}

/** Whether a row still delivers a locked action: on pressable controls only
 *  (never a stick or the lightbar, which fire nothing), with its default
 *  `when` and at least its default hold time. */
export function satisfiesLocked(b: PadBinding, l: LockedAction): boolean {
  if (b.action !== l.action || b.gesture !== l.gesture) return false;
  if (l.combo && b.with.length === 0) return false;
  if (!isPressable(b.control) || !b.with.every(isPressable)) return false;
  const def = lockedDefault(l);
  if (!def) return true;
  if (b.when !== def.when) return false;
  return def.hold_ms == null || (b.hold_ms != null && b.hold_ms >= def.hold_ms);
}

/** The `locked_actions` entries no row satisfies. */
export function missingLocked(rows: readonly PadBinding[]): LockedAction[] {
  return LOCKED_ACTIONS.filter((l) => !rows.some((b) => satisfiesLocked(b, l)));
}

export function lockedLabel(l: LockedAction): string {
  const label = SPEC_ACTION_BY_ID[l.action]?.label ?? l.action;
  return l.combo ? `${label} (combo ${l.gesture})` : `${label} (${l.gesture})`;
}

/** An emergency-stop hold row never holds for less than the spec stop hold. */
export function clampStopHold(b: PadBinding): PadBinding {
  const ms = effectiveHoldMs(b);
  return ms === b.hold_ms ? b : { ...b, hold_ms: ms };
}

export type EditResult = { ok: true; rows: PadBinding[] } | { ok: false; error: string };

/** Set a row's action. A locked row keeps its action: it can only move. */
export function setRowAction(rows: readonly PadBinding[], index: number, action: string): EditResult {
  const row = rows[index];
  if (!row) return { ok: false, error: "No such binding." };
  if (row.locked && row.action !== action) {
    const label = SPEC_ACTION_BY_ID[row.action]?.label ?? row.action;
    return { ok: false, error: `${label} is locked: it can be moved to another control, never removed.` };
  }
  const next = cloneBindings(rows);
  next[index] = clampStopHold({ ...next[index], action });
  return { ok: true, rows: next };
}

/** Add a row on a control for a gesture that has none yet. */
export function addRow(rows: readonly PadBinding[], control: ControlId, gesture: Gesture, action: string): PadBinding[] {
  const next = cloneBindings(rows);
  next.push(clampStopHold({
    control,
    with: [],
    gesture,
    hold_ms: gesture === "hold" ? 1500 : null,
    when: null,
    action,
    rumble: gesture === "hold" ? "ramp" : null,
    locked: false,
  }));
  return next;
}

/** Delete a row (setting it to None). Locked rows refuse. */
export function removeRow(rows: readonly PadBinding[], index: number): EditResult {
  const row = rows[index];
  if (!row) return { ok: false, error: "No such binding." };
  if (row.locked) return setRowAction(rows, index, "none");
  const next = cloneBindings(rows);
  next.splice(index, 1);
  return { ok: true, rows: next };
}

/** Move a row to another primary control, keeping its combo members (minus
 *  the new control) — the one edit a locked row allows. */
export function moveRow(rows: readonly PadBinding[], index: number, control: ControlId): EditResult {
  const row = rows[index];
  if (!row) return { ok: false, error: "No such binding." };
  if (row.locked && !isPressable(control)) {
    const label = SPEC_ACTION_BY_ID[row.action]?.label ?? row.action;
    return { ok: false, error: `${label} is locked: it can only move to a button.` };
  }
  const next = cloneBindings(rows);
  next[index] = { ...next[index], control, with: row.with.filter((w) => w !== control) };
  return { ok: true, rows: next };
}

// ── export / import ──────────────────────────────────────────────────────────

export function clampDeadzone(v: number): number {
  if (!Number.isFinite(v)) return DEADZONE_DEFAULT;
  return Math.min(DEADZONE_MAX, Math.max(DEADZONE_MIN, Math.round(v * 100) / 100));
}

export function buildDoc(
  rows: readonly PadBinding[],
  stickMode: StickMode,
  deadzone: number,
  pad: PadKind,
): ControlsDoc {
  return {
    v: EXPORT_VERSION,
    kind: EXPORT_KIND,
    pad,
    stick_mode: stickMode,
    deadzone: clampDeadzone(deadzone),
    profile: isDefaultBindings(rows) && stickMode === DEFAULT_STICK_MODE ? "ados_default" : "user",
    bindings: cloneBindings(rows),
  };
}

function parseRow(raw: unknown, i: number, errors: string[]): PadBinding | null {
  if (typeof raw !== "object" || raw === null) {
    errors.push(`bindings[${i}] is not an object`);
    return null;
  }
  const r = raw as Record<string, unknown>;
  const withRaw = Array.isArray(r.with) ? r.with : [];
  if (!isControlId(r.control)) errors.push(`bindings[${i}].control is not a control`);
  if (!withRaw.every(isControlId)) errors.push(`bindings[${i}].with names an unknown control`);
  if (!isGesture(r.gesture)) errors.push(`bindings[${i}].gesture is not tap, hold, double_tap or analog`);
  if (r.when != null && !isWhenCondition(r.when)) errors.push(`bindings[${i}].when is not a known condition`);
  if (typeof r.action !== "string" || r.action === "") errors.push(`bindings[${i}].action is missing`);
  if (r.hold_ms != null && (typeof r.hold_ms !== "number" || r.hold_ms <= 0)) errors.push(`bindings[${i}].hold_ms is not a positive number`);
  if (r.gesture === "hold" && typeof r.hold_ms !== "number") errors.push(`bindings[${i}] is a hold without hold_ms`);
  if (!isControlId(r.control) || !withRaw.every(isControlId) || !isGesture(r.gesture) || typeof r.action !== "string") {
    return null;
  }
  const row: PadBinding = {
    control: r.control,
    with: withRaw as ControlId[],
    gesture: r.gesture,
    hold_ms: typeof r.hold_ms === "number" ? r.hold_ms : null,
    when: isWhenCondition(r.when) ? r.when : null,
    action: r.action,
    rumble: isRumbleKind(r.rumble) ? r.rumble : null,
    locked: r.locked === true,
  };
  if (typeof r.rate_full_dps === "number") row.rate_full_dps = r.rate_full_dps;
  return clampStopHold(row);
}

export type ImportResult =
  | { ok: true; doc: ControlsDoc }
  | { ok: false; errors: string[]; missingLocked: LockedAction[] };

/** Parse an export file. Refuses a malformed file and one that drops a locked
 *  binding (the same rule the ground station's 422 applies). */
export function parseControlsDoc(text: string): ImportResult {
  let raw: unknown;
  try {
    raw = JSON.parse(text);
  } catch {
    return { ok: false, errors: ["Not a JSON file."], missingLocked: [] };
  }
  if (typeof raw !== "object" || raw === null) return { ok: false, errors: ["Not a controls file."], missingLocked: [] };
  const r = raw as Record<string, unknown>;
  const errors: string[] = [];
  if (r.kind !== EXPORT_KIND) errors.push(`kind is not "${EXPORT_KIND}"`);
  if (r.stick_mode !== "mode1" && r.stick_mode !== "mode2") errors.push("stick_mode is not mode1 or mode2");
  if (!Array.isArray(r.bindings)) errors.push("bindings is not a list");
  const rows = Array.isArray(r.bindings)
    ? r.bindings.map((b, i) => parseRow(b, i, errors)).filter((b): b is PadBinding => b !== null)
    : [];
  if (errors.length > 0) return { ok: false, errors, missingLocked: [] };
  const missing = missingLocked(rows);
  if (missing.length > 0) {
    return { ok: false, errors: [`Missing locked bindings: ${missing.map(lockedLabel).join(", ")}`], missingLocked: missing };
  }
  return {
    ok: true,
    doc: buildDoc(
      rows,
      r.stick_mode as StickMode,
      typeof r.deadzone === "number" ? r.deadzone : DEADZONE_DEFAULT,
      r.pad === "generic" ? "generic" : "ds4",
    ),
  };
}
