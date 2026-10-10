// Polls `GET /api/plugins/{id}/state` every 750 ms for each running extension
// that contributes skills, while the Feed is mounted and the page is visible:
// one request per extension per tick, so the Skill Bar shows the plugin's own
// reported state rather than an optimistic one.

import { useEffect } from "react";

import { pollLoop } from "@/hooks/use-status-poll";
import { apiFetch } from "@/shared/api-fetch";
import { useExtensionsStore } from "@/stores/extensions-store";

export const EXTENSION_STATE_POLL_MS = 750;

export function useExtensionStatePoll(): void {
  const extensions = useExtensionsStore((s) => s.extensions);

  useEffect(() => {
    const withSkills = extensions.filter((e) => e.skills.some((s) => s.extension?.stateTopic));
    const stops = withSkills.map((ext) =>
      pollLoop(
        async (signal) => {
          try {
            const body = await apiFetch<unknown>(
              `/api/plugins/${encodeURIComponent(ext.pluginId)}/state`,
              { signal },
            );
            useExtensionsStore.getState().applyState(ext, body);
          } catch {
            // No state yet: the skill reads idle until the plugin reports.
          }
        },
        () => EXTENSION_STATE_POLL_MS,
      ),
    );
    return () => stops.forEach((stop) => stop());
  }, [extensions]);
}
