// The setup wizard's live state for a React surface: one `GET /api/v1/fc-setup`
// read, then the setup stream folded in frame by frame, plus the operator's
// requests (start, step action, re-run, abort, revert). Each request applies
// the engine's reply at once, so the screen moves before the next frame lands.

import { useCallback, useEffect, useRef, useState } from "react";

import {
  abortWizard,
  fetchSetupStatus,
  openSetupStream,
  rerunStep,
  revertToSnapshot,
  runStepAction,
  setupErrorKey,
  startWizard,
} from "./client";
import { applyFrame, withStep, withWizard } from "./parse";
import { setupText } from "./strings";
import type { SetupStatus, SetupStep, SetupValue, Wizard } from "./types";

export interface FcSetupController {
  status: SetupStatus | null;
  /** True until the first read settles. */
  loading: boolean;
  /** The setup stream is live: false until a socket opens, false again once
   *  it has been closed or silent for 5 s (the stream keeps alive at 1 Hz),
   *  true again on reconnect. Always true when the stream is not held open. */
  streamUp: boolean;
  /** The last failed read, or null. */
  loadError: string | null;
  /** The last refused or failed request, as display text, or null. */
  requestError: string | null;
  /** The request in flight (`<wizard>`, `<wizard>/<step>/<action>`, `revert`), or null. */
  busy: string | null;
  refresh: () => Promise<void>;
  start: (wizard: string) => Promise<void>;
  abort: (wizard: string) => Promise<void>;
  action: (step: SetupStep, actionId: string, args?: Record<string, SetupValue>) => Promise<void>;
  rerun: (step: SetupStep) => Promise<void>;
  /** Restore a parameter snapshot; resolves to the restored count, or null on failure. */
  revert: (snapshot: string) => Promise<number | null>;
}

function requestMessage(err: unknown): string {
  const key = setupErrorKey(err);
  if (key) return setupText(key);
  return err instanceof Error ? err.message : String(err);
}

export interface UseFcSetupOptions {
  /** Hold the setup stream open while mounted (default true). */
  stream?: boolean;
}

export function useFcSetup({ stream = true }: UseFcSetupOptions = {}): FcSetupController {
  const [status, setStatus] = useState<SetupStatus | null>(null);
  const [loading, setLoading] = useState(true);
  const [streamUp, setStreamUp] = useState(false);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [requestError, setRequestError] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const mounted = useRef(true);

  const refresh = useCallback(async () => {
    try {
      const next = await fetchSetupStatus();
      if (!mounted.current) return;
      setStatus(next);
      setLoadError(null);
    } catch (err) {
      if (mounted.current) setLoadError(err instanceof Error ? err.message : String(err));
    } finally {
      if (mounted.current) setLoading(false);
    }
  }, []);

  useEffect(() => {
    mounted.current = true;
    fetchSetupStatus()
      .then((next) => {
        if (!mounted.current) return;
        setStatus(next);
        setLoadError(null);
      })
      .catch((err: unknown) => {
        if (mounted.current) setLoadError(err instanceof Error ? err.message : String(err));
      })
      .finally(() => {
        if (mounted.current) setLoading(false);
      });
    const stop = stream
      ? openSetupStream(
          (frame) => setStatus((s) => (s ? applyFrame(s, frame) : s)),
          () => void refresh(),
          (up) => {
            if (mounted.current) setStreamUp(up);
          },
        )
      : undefined;
    return () => {
      mounted.current = false;
      stop?.();
    };
  }, [refresh, stream]);

  const request = useCallback(async <T,>(key: string, run: () => Promise<T>): Promise<T | null> => {
    setBusy(key);
    setRequestError(null);
    try {
      return await run();
    } catch (err) {
      if (mounted.current) setRequestError(requestMessage(err));
      return null;
    } finally {
      if (mounted.current) setBusy(null);
    }
  }, []);

  const applyWizard = (w: Wizard | null) => {
    if (w) setStatus((s) => (s ? withWizard(s, w) : s));
  };
  const applyStep = (step: SetupStep | null) => {
    if (step) setStatus((s) => (s ? withStep(s, step) : s));
  };

  return {
    status,
    loading,
    streamUp: stream ? streamUp : true,
    loadError,
    requestError,
    busy,
    refresh,
    start: async (wizard) => applyWizard(await request(wizard, () => startWizard(wizard))),
    abort: async (wizard) => applyWizard(await request(wizard, () => abortWizard(wizard))),
    action: async (step, actionId, args) =>
      applyStep(
        await request(`${step.wizard}/${step.step}/${actionId}`, () =>
          runStepAction(step.wizard, step.step, actionId, args),
        ),
      ),
    rerun: async (step) =>
      applyStep(await request(`${step.wizard}/${step.step}/run`, () => rerunStep(step.wizard, step.step))),
    revert: async (snapshot) => {
      const restored = await request("revert", () => revertToSnapshot(snapshot));
      if (restored !== null) await refresh();
      return restored;
    },
  };
}
