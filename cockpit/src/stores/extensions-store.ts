// The node's running extensions, their skills' reported state and the
// activation writes awaiting an echo. Loaded when the shell mounts (and again
// when the Extensions screen opens); while the Feed is on screen the install
// list is re-read periodically so a plugin that stops is unregistered, and
// the skills' state topics are polled (`use-extension-state-poll`).

import { create } from "zustand";

import {
  detailLocales,
  isEnabledInstall,
  parseExtensionDetail,
  pickLocale,
  reportedStatesFor,
  type InstalledExtension,
  type RejectedSkill,
} from "@/lib/extensions";
import { specText } from "@/shared/hud/spec-text";
import { buildBindings, echoOutcome, type BindingConflict } from "@/lib/skill-gates";
import type { ParamValue, PendingWrite, ReportedSkillState, Skill } from "@/lib/skills";
import { apiFetch } from "@/shared/api-fetch";
import { toast } from "@/shared/api/toast";

interface InstallRow {
  plugin_id?: unknown;
  status?: unknown;
  failure_reason?: unknown;
}

interface ExtensionsState {
  extensions: InstalledExtension[];
  /** Manifest skills refused by validation, across every extension. */
  rejected: RejectedSkill[];
  /** Keyboard collisions: the later skill got no key. */
  conflicts: BindingConflict[];
  /** Canonical chord → cockpit action id (built-ins and plugin skills). */
  keys: Record<string, string>;
  loaded: boolean;
  error: string | null;
  reported: Record<string, ReportedSkillState | undefined>;
  /** Toggle activation writes awaiting their echo, by skill id. */
  pending: Record<string, PendingWrite | undefined>;
  /** Stops written while the link was down, sent when it returns. */
  queuedStops: string[];
  /** Each plugin's effective config (`GET /api/plugins/{id}/config`). */
  config: Record<string, Record<string, ParamValue> | undefined>;

  load: () => Promise<void>;
  syncInstalls: () => Promise<void>;
  applyState: (ext: InstalledExtension, body: unknown, at?: number) => void;
  markPending: (skillId: string, want: boolean, at?: number) => void;
  clearPending: (skillId: string) => void;
  settlePending: (now?: number) => void;
  queueStop: (skillId: string) => void;
  dequeueStop: (skillId: string) => void;
  loadConfig: (pluginId: string) => Promise<void>;
}

async function fetchInstalls(): Promise<InstallRow[]> {
  const res = await apiFetch<{ installs?: InstallRow[] }>("/api/plugins");
  return Array.isArray(res?.installs) ? res.installs : [];
}

/** One extension's detail and locale strings, parsed. */
async function fetchExtension(pluginId: string): Promise<InstalledExtension> {
  const base = `/api/plugins/${encodeURIComponent(pluginId)}`;
  const detail = await apiFetch<unknown>(base);
  const lang = pickLocale(detailLocales(detail), typeof navigator !== "undefined" ? navigator.language : null);
  let strings: Record<string, string> | null = null;
  if (lang) {
    try {
      const body = await apiFetch<unknown>(`${base}/locales/${encodeURIComponent(lang)}`);
      if (body && typeof body === "object" && !Array.isArray(body)) {
        strings = Object.fromEntries(
          Object.entries(body as Record<string, unknown>).filter((e): e is [string, string] => typeof e[1] === "string"),
        );
      }
    } catch {
      // No bundle served: labels humanise their keys.
    }
  }
  return parseExtensionDetail(pluginId, detail, strings);
}

function enabledIds(rows: InstallRow[]): string[] {
  return rows
    .filter((i) => typeof i.plugin_id === "string" && isEnabledInstall(i.status))
    .map((i) => i.plugin_id as string);
}

/** The derived lists every change of the extension set recomputes. */
function derive(extensions: InstalledExtension[]) {
  const table = buildBindings(extensions.flatMap((e) => e.skills));
  return { extensions, rejected: extensions.flatMap((e) => e.rejected), conflicts: table.conflicts, keys: table.keys };
}

function withoutKeys<T>(rec: Record<string, T>, drop: (key: string) => boolean): Record<string, T> {
  return Object.fromEntries(Object.entries(rec).filter(([k]) => !drop(k)));
}

export const useExtensionsStore = create<ExtensionsState>((set, get) => ({
  ...derive([]),
  loaded: false,
  error: null,
  reported: {},
  pending: {},
  queuedStops: [],
  config: {},

  load: async () => {
    try {
      const ids = enabledIds(await fetchInstalls());
      const details = await Promise.allSettled(ids.map(fetchExtension));
      const extensions = details.flatMap((r) => (r.status === "fulfilled" ? [r.value] : []));
      set({ ...derive(extensions), loaded: true, error: null });
    } catch (err) {
      set({ loaded: true, error: err instanceof Error ? err.message : String(err) });
    }
  },

  // A plugin that left the running set (disabled, failed, uninstalled) is
  // unregistered with a toast naming why; a newly running one is loaded.
  syncInstalls: async () => {
    if (!get().loaded) return;
    const rows = await fetchInstalls();
    const running = new Set(enabledIds(rows));
    const current = get().extensions;
    const gone = current.filter((e) => !running.has(e.pluginId));
    const known = new Set(current.map((e) => e.pluginId));
    const added = await Promise.allSettled([...running].filter((id) => !known.has(id)).map(fetchExtension));
    const fresh = added.flatMap((r) => (r.status === "fulfilled" ? [r.value] : []));
    if (gone.length === 0 && fresh.length === 0) return;
    for (const ext of gone) {
      const row = rows.find((r) => r.plugin_id === ext.pluginId);
      const reason =
        (typeof row?.failure_reason === "string" && row.failure_reason) ||
        (typeof row?.status === "string" && row.status) ||
        "removed";
      if (ext.skills.length > 0) toast.err(specText("skill.plugin_stopped", { plugin: ext.name, reason }));
    }
    const goneIds = new Set(gone.flatMap((e) => e.skills.map((s) => s.id)));
    const goneSkill = (id: string) => goneIds.has(id);
    set((s) => ({
      ...derive([...s.extensions.filter((e) => running.has(e.pluginId)), ...fresh]),
      reported: withoutKeys(s.reported, goneSkill),
      pending: withoutKeys(s.pending, goneSkill),
      queuedStops: s.queuedStops.filter((id) => !goneSkill(id)),
    }));
  },

  applyState: (ext, body, at = Date.now()) => {
    set((s) => ({ reported: { ...s.reported, ...reportedStatesFor(ext, body, at) } }));
    get().settlePending(at);
  },

  markPending: (skillId, want, at = Date.now()) => set((s) => ({ pending: { ...s.pending, [skillId]: { want, at } } })),

  clearPending: (skillId) => set((s) => ({ pending: withoutKeys(s.pending, (k) => k === skillId) })),

  // A write the plugin echoed ends; one never echoed in 3 s reverts with a
  // toast. A stop queued behind a lost link waits for the link instead.
  settlePending: (now = Date.now()) => {
    const { pending, reported, queuedStops, extensions } = get();
    const done: string[] = [];
    for (const [id, write] of Object.entries(pending)) {
      if (!write || queuedStops.includes(id)) continue;
      const outcome = echoOutcome(write, reported[id], now);
      if (outcome === "pending") continue;
      done.push(id);
      if (outcome === "expired") {
        const label = extensions.flatMap((e) => e.skills).find((sk) => sk.id === id)?.label ?? id;
        toast.err(specText("skill.did_not_respond", { skill: label }));
      }
    }
    if (done.length > 0) set((s) => ({ pending: withoutKeys(s.pending, (k) => done.includes(k)) }));
  },

  queueStop: (skillId) =>
    set((s) => (s.queuedStops.includes(skillId) ? s : { queuedStops: [...s.queuedStops, skillId] })),

  dequeueStop: (skillId) => set((s) => ({ queuedStops: s.queuedStops.filter((id) => id !== skillId) })),

  loadConfig: async (pluginId) => {
    const body = await apiFetch<{ values?: unknown }>(`/api/plugins/${encodeURIComponent(pluginId)}/config`);
    const raw = body?.values && typeof body.values === "object" ? (body.values as Record<string, unknown>) : {};
    const values: Record<string, ParamValue> = {};
    for (const [k, v] of Object.entries(raw)) {
      if (typeof v === "string" || typeof v === "boolean" || (typeof v === "number" && Number.isFinite(v))) values[k] = v;
    }
    set((s) => ({ config: { ...s.config, [pluginId]: values } }));
  },
}));

/** Every registered plugin skill, in load order. */
export function pluginSkills(): Skill[] {
  return useExtensionsStore.getState().extensions.flatMap((e) => e.skills);
}

/** Manifest skills validation refused, for Settings › Controls. */
export function rejectedSkills(): RejectedSkill[] {
  return useExtensionsStore.getState().rejected;
}

/** Keyboard collisions (the later skill got no key), for Settings › Controls. */
export function bindingConflicts(): BindingConflict[] {
  return useExtensionsStore.getState().conflicts;
}

/** Write an extension skill's activation key. */
export async function activateExtensionSkill(pluginId: string, configKey: string, value: boolean): Promise<void> {
  await apiFetch(`/api/plugins/${encodeURIComponent(pluginId)}/config`, {
    method: "PUT",
    body: { key: configKey, value },
  });
}
