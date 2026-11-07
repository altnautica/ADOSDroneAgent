// The cockpit's named actions: one registry every input path fires through
// (controller bindings, keyboard chords, the on-screen controls). Each feature
// registers the actions it implements while it is mounted (the skill picker
// registers `picker.*` and `skill.*`, the view store `view.next`, the flight
// built-ins their ids), so a binding names an action, never a component.
//
// An action runs in one of three phases: `press` (a tap or the start of a
// hold), `hold` (a hold gesture completed) and `release`. Most actions only
// handle `press`.

export type ActionPhase = "press" | "hold" | "release";

export type ActionGroup = "flight" | "skills" | "camera" | "view" | "profile" | "none";

export interface CockpitAction {
  id: string;
  label: string;
  group: ActionGroup;
  run: (phase: ActionPhase) => void;
}

const registry = new Map<string, CockpitAction>();
const listeners = new Set<() => void>();
let version = 0;

function changed(): void {
  version += 1;
  for (const l of listeners) l();
}

/** Register actions; returns an unregister for exactly those. */
export function registerCockpitActions(actions: readonly CockpitAction[]): () => void {
  for (const a of actions) registry.set(a.id, a);
  changed();
  return () => {
    for (const a of actions) if (registry.get(a.id) === a) registry.delete(a.id);
    changed();
  };
}

/** Fire an action by id. Returns false when nothing registers it. */
export function runCockpitAction(id: string, phase: ActionPhase = "press"): boolean {
  const a = registry.get(id);
  if (!a) return false;
  a.run(phase);
  return true;
}

/** Every registered action, for the binding editor's picker. */
export function listCockpitActions(): CockpitAction[] {
  return Array.from(registry.values()).sort(
    (x, y) => x.group.localeCompare(y.group) || x.label.localeCompare(y.label),
  );
}

export function cockpitActionLabel(id: string): string | null {
  return registry.get(id)?.label ?? null;
}

/** Subscribe to registry changes (for `useSyncExternalStore`). */
export function subscribeCockpitActions(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

export function cockpitActionsVersion(): number {
  return version;
}
