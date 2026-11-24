// The setup wizard's live state for a React surface: one `GET /api/v1/fc-setup`
// read, then the setup stream folded in frame by frame, plus the operator's
// requests (start, resume, step action, re-run, abort, revert). Each request
// applies the engine's reply at once, so the screen moves before the next
// frame lands.

import { useCallback, useEffect, useRef, useState } from "react";

import {
  abortWizard,
  fetchSetupStatus,
  loadErrorText,
  openSetupStream,
  rerunStep,
  resumeWizard,
  revertToSnapshot,
  runStepAction,
  setupErrorKey,
  startWizard,
} from "./client";
import { applyFrame, needsFullStatus, withStep, withWizard } from "./parse";
import { setupErrorText } from "./strings";
import type { SetupStatus, SetupStep, SetupValue, Wizard } from "./types";

export interface FcSetupController {
  status: SetupStatus | null;
  /** True until the first read settles. */
  loading: boolean;
  /** The setup stream is live: false until a socket opens, false again once
   *  it has been closed or silent for 5 s (the stream keeps alive at 1 Hz),
   *  true again on reconnect. Always true when the stream is not held open. */
  streamUp: boolean;
  /** The last failed read as display text (the engine's message, or "not
   *  available on this node" for a 503), or null. */
  loadError: string | null;
  /** The last refused or failed request, as display text, or null. */
  requestError: string | null;
  /** The request in flight (`<wizard>`, `<wizard>/<step>/<action>`, `revert`), or null. */
  busy: string | null;
  refresh: () => Promise<void>;
  /** Start (or start over) a wizard; resolves true when the engine took it. */
  start: (wizard: string) => Promise<boolean>;
  /** Continue an interrupted wizard where it stopped; true when it did. */
  resume: (wizard: string) => Promise<boolean>;
  abort: (wizard: string) => Promise<void>;
  action: (step: SetupStep, actionId: string, args?: Record<string, SetupValue>) => Promise<void>;
  /** Run one step on its own; true when the engine took it. */
  rerun: (step: SetupStep) => Promise<boolean>;
  /** Restore a parameter snapshot; resolves to the restored count, or null on failure. */
  revert: (snapshot: string) => Promise<number | null>;
}

function requestMessage(err: unknown): string {
  const key = setupErrorKey(err);
  if (key) return setupErrorText(key);
  return err instanceof Error ? err.message : String(err);
}

export interface UseFcSetupOptions {
  /** Hold the setup stream open while mounted (default true). */
  stream?: boolean;
}

/** A status read's outcome: the status, or the error it failed with. */
type ReadOutcome = { next: SetupStatus } | { err: unknown };

/** The least time between the full status reads a stream frame triggers
 *  while the status lacks the FC's identity (`needsFullStatus`); keepalives
 *  arrive at 1 Hz, so this holds those reads to one every 2 s. */
const FRAME_REREAD_MS = 2000;

export function useFcSetup({ stream = true }: UseFcSetupOptions = {}): FcSetupController {
  const [status, setStatus] = useState<SetupStatus | null>(null);
  const [loading, setLoading] = useState(true);
  const [streamUp, setStreamUp] = useState(false);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [requestError, setRequestError] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const mounted = useRef(true);
  const statusRef = useRef<SetupStatus | null>(null);
  const reading = useRef(false);
  /** The last read failed (the engine was not up yet). */
  const failed = useRef(false);
  const lastRead = useRef(0);
  useEffect(() => {
    statusRef.current = status;
  }, [status]);

  // One status read: records when it ran and whether it failed, and hands
  // its outcome to `settle`, the only place that sets state from it.
  const read = useCallback(() => {
    reading.current = true;
    lastRead.current = Date.now();
    return fetchSetupStatus()
      .then(
        (next): ReadOutcome => {
          failed.current = false;
          return { next };
        },
        (err: unknown): ReadOutcome => {
          failed.current = true;
          return { err };
        },
      )
      .finally(() => {
        reading.current = false;
      });
  }, []);

  const settle = useCallback((outcome: ReadOutcome) => {
    if (!mounted.current) return;
    if ("next" in outcome) {
      setStatus(outcome.next);
      setLoadError(null);
    } else {
      setLoadError(loadErrorText(outcome.err));
    }
    setLoading(false);
  }, []);

  const refresh = useCallback(() => read().then(settle), [read, settle]);

  useEffect(() => {
    mounted.current = true;
    void read().then(settle);
    // Each (re)connect re-reads the whole status it may have missed. A frame
    // re-reads it while the last read failed or had no FC identity (taken
    // while the agent was restarting): frames never carry the uid or
    // firmware, so the screen would otherwise stay stale until a reload.
    const stop = stream
      ? openSetupStream(
          (frame) => {
            setStatus((s) => (s ? applyFrame(s, frame) : s));
            const due = !reading.current && Date.now() - lastRead.current >= FRAME_REREAD_MS;
            if (due && (failed.current || needsFullStatus(statusRef.current))) void refresh();
          },
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
  }, [read, refresh, settle, stream]);

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

  const applyWizard = (w: Wizard | null): boolean => {
    if (w) setStatus((s) => (s ? withWizard(s, w) : s));
    return w !== null;
  };
  const applyStep = (step: SetupStep | null): boolean => {
    if (step) setStatus((s) => (s ? withStep(s, step) : s));
    return step !== null;
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
    resume: async (wizard) => applyWizard(await request(wizard, () => resumeWizard(wizard))),
    abort: async (wizard) => {
      applyWizard(await request(wizard, () => abortWizard(wizard)));
    },
    action: async (step, actionId, args) => {
      applyStep(
        await request(`${step.wizard}/${step.step}/${actionId}`, () =>
          runStepAction(step.wizard, step.step, actionId, args),
        ),
      );
    },
    rerun: async (step) =>
      applyStep(await request(`${step.wizard}/${step.step}/run`, () => rerunStep(step.wizard, step.step))),
    revert: async (snapshot) => {
      const restored = await request("revert", () => revertToSnapshot(snapshot));
      if (restored !== null) await refresh();
      return restored;
    },
  };
}
