// While the Feed is mounted and the page is visible:
//   - polls `GET /api/plugins/{id}/state` every 750 ms for each running
//     extension that contributes skills, so the picker shows the plugin's own
//     reported state (and its age) rather than an optimistic one;
//   - re-reads `GET /api/plugins` every 5 s, so a plugin that is disabled or
//     crashes is unregistered with a toast, and a newly running one appears;
//   - settles activation writes (echoed, or reverted after 3 s), and sends
//     the stops queued while the link was down once a poll reaches the agent.

import { useEffect } from "react";

import { pollLoop } from "@/hooks/use-status-poll";
import { flushQueuedStops } from "@/lib/skill-runner";
import { ApiError, apiFetch } from "@/shared/api-fetch";
import { useExtensionsStore } from "@/stores/extensions-store";

export const EXTENSION_STATE_POLL_MS = 750;
export const EXTENSION_INSTALLS_POLL_MS = 5000;

/** The agent answered (a 404 is a plugin with no state yet, not a lost link). */
function reachedAgent(e: unknown): boolean {
  return e instanceof ApiError && e.status > 0 && e.status < 502;
}

export function useExtensionStatePoll(): void {
  const extensions = useExtensionsStore((s) => s.extensions);

  useEffect(() => {
    const withSkills = extensions.filter((e) => e.skills.length > 0);
    const stops = withSkills.map((ext) =>
      pollLoop(
        async (signal) => {
          let reached = true;
          try {
            const body = await apiFetch<unknown>(`/api/plugins/${encodeURIComponent(ext.pluginId)}/state`, { signal });
            useExtensionsStore.getState().applyState(ext, body);
          } catch (e) {
            // No state: the skill's last state ages and dims after 3 s.
            reached = reachedAgent(e);
          }
          useExtensionsStore.getState().settlePending();
          if (reached && useExtensionsStore.getState().queuedStops.length > 0) await flushQueuedStops();
        },
        () => EXTENSION_STATE_POLL_MS,
      ),
    );
    return () => stops.forEach((stop) => stop());
  }, [extensions]);

  useEffect(
    () =>
      pollLoop(
        () => useExtensionsStore.getState().syncInstalls(),
        () => EXTENSION_INSTALLS_POLL_MS,
      ),
    [],
  );
}
