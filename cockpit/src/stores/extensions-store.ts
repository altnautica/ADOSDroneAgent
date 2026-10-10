// The node's running extensions and their skills' reported state. Loaded once
// when the shell mounts (and again when the Extensions screen opens); the
// state of extension skills is polled only while the Feed is on screen.

import { create } from "zustand";

import {
  isEnabledInstall,
  parseExtensionDetail,
  reportedStatesFor,
  type InstalledExtension,
} from "@/lib/extensions";
import type { ReportedSkillState } from "@/lib/skills";
import { apiFetch } from "@/shared/api-fetch";

interface ExtensionsState {
  extensions: InstalledExtension[];
  loaded: boolean;
  error: string | null;
  reported: Record<string, ReportedSkillState | undefined>;
  load: () => Promise<void>;
  applyState: (ext: InstalledExtension, body: unknown) => void;
}

export const useExtensionsStore = create<ExtensionsState>((set) => ({
  extensions: [],
  loaded: false,
  error: null,
  reported: {},

  load: async () => {
    try {
      const res = await apiFetch<{ installs?: { plugin_id?: unknown; status?: unknown }[] }>(
        "/api/plugins",
      );
      const ids = (Array.isArray(res?.installs) ? res.installs : [])
        .filter((i) => typeof i.plugin_id === "string" && isEnabledInstall(i.status))
        .map((i) => i.plugin_id as string);
      const details = await Promise.allSettled(
        ids.map((id) => apiFetch<unknown>(`/api/plugins/${encodeURIComponent(id)}`)),
      );
      const extensions = details.flatMap((r, i) =>
        r.status === "fulfilled" ? [parseExtensionDetail(ids[i], r.value)] : [],
      );
      set({ extensions, loaded: true, error: null });
    } catch (err) {
      set({ loaded: true, error: err instanceof Error ? err.message : String(err) });
    }
  },

  applyState: (ext, body) =>
    set((s) => ({ reported: { ...s.reported, ...reportedStatesFor(ext, body) } })),
}));

/** Flip an extension skill's config key: a toggle writes the opposite of its
 *  reported state, a one-shot writes `true` (the plugin clears it). */
export async function activateExtensionSkill(
  pluginId: string,
  configKey: string,
  value: boolean,
): Promise<void> {
  await apiFetch(`/api/plugins/${encodeURIComponent(pluginId)}/config`, {
    method: "PUT",
    body: { key: configKey, value },
  });
}
