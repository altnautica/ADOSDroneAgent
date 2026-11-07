// Client for the agent's FC setup routes (pairing-key auth, WS ticket scope
// `fc.setup`):
//   GET  /api/v1/fc-setup                                    wizards, state, report
//   POST /api/v1/fc-setup/{wizard}/start                     -> {ok, wizard}
//   POST /api/v1/fc-setup/{wizard}/steps/{step}/actions/{id} {args?} -> {ok, step}
//   POST /api/v1/fc-setup/{wizard}/steps/{step}/run          -> {ok, step}
//   POST /api/v1/fc-setup/{wizard}/abort                     -> {ok, wizard}
//   POST /api/v1/fc-setup/revert {snapshot}                  -> {ok, restored}
//   WS   /api/v1/fc-setup/stream                             step/wizard frames, 5 Hz while running
// A refusal is HTTP 409/422 with `{ok:false, err:"<msg_key>"}`.

import { ApiError, apiFetch } from "../api-fetch";
import { mintWsTicket, ticketProtocols } from "../ws-ticket";

import { parseFrame, parseStatus, parseStep, parseWizard } from "./parse";
import type { SetupFrame, SetupStatus, SetupStep, SetupValue, Wizard } from "./types";

export const FC_SETUP_BASE = "/api/v1/fc-setup";
export const FC_SETUP_SCOPE = "fc.setup";

const seg = encodeURIComponent;

/** Every fc-setup URL, built in one place. */
export const fcSetupUrls = {
  status: () => FC_SETUP_BASE,
  start: (wizard: string) => `${FC_SETUP_BASE}/${seg(wizard)}/start`,
  action: (wizard: string, step: string, action: string) =>
    `${FC_SETUP_BASE}/${seg(wizard)}/steps/${seg(step)}/actions/${seg(action)}`,
  run: (wizard: string, step: string) => `${FC_SETUP_BASE}/${seg(wizard)}/steps/${seg(step)}/run`,
  abort: (wizard: string) => `${FC_SETUP_BASE}/${seg(wizard)}/abort`,
  revert: () => `${FC_SETUP_BASE}/revert`,
  /** The stream URL on the page's own origin (`ws:` or `wss:`). */
  stream: (loc: { protocol: string; host: string }) =>
    `${loc.protocol === "https:" ? "wss:" : "ws:"}//${loc.host}${FC_SETUP_BASE}/stream`,
};

/** The engine's message key from a refused request, else null. */
export function setupErrorKey(err: unknown): string | null {
  if (!(err instanceof ApiError)) return null;
  const body = err.body;
  if (body && typeof body === "object" && "err" in body && typeof body.err === "string") return body.err;
  return null;
}

function badBody(path: string): Error {
  return new Error(`unexpected response from ${path}`);
}

export async function fetchSetupStatus(signal?: AbortSignal): Promise<SetupStatus> {
  const path = fcSetupUrls.status();
  const status = parseStatus(await apiFetch(path, { signal }));
  if (!status) throw badBody(path);
  return status;
}

async function postWizard(path: string): Promise<Wizard> {
  const body = await apiFetch<{ wizard?: unknown }>(path, { method: "POST", body: {} });
  const wizard = parseWizard(body?.wizard);
  if (!wizard) throw badBody(path);
  return wizard;
}

async function postStep(path: string, body: unknown): Promise<SetupStep> {
  const res = await apiFetch<{ step?: unknown }>(path, { method: "POST", body });
  const step = parseStep(res?.step);
  if (!step) throw badBody(path);
  return step;
}

export function startWizard(wizard: string): Promise<Wizard> {
  return postWizard(fcSetupUrls.start(wizard));
}

export function abortWizard(wizard: string): Promise<Wizard> {
  return postWizard(fcSetupUrls.abort(wizard));
}

export function runStepAction(
  wizard: string,
  step: string,
  action: string,
  args?: Record<string, SetupValue>,
): Promise<SetupStep> {
  return postStep(fcSetupUrls.action(wizard, step, action), args && Object.keys(args).length ? { args } : {});
}

export function rerunStep(wizard: string, step: string): Promise<SetupStep> {
  return postStep(fcSetupUrls.run(wizard, step), {});
}

export async function revertToSnapshot(snapshot: string): Promise<number> {
  const body = await apiFetch<{ restored?: unknown }>(fcSetupUrls.revert(), {
    method: "POST",
    body: { snapshot },
  });
  return typeof body?.restored === "number" ? body.restored : 0;
}

const STREAM_RETRY_MIN_MS = 1000;
const STREAM_RETRY_MAX_MS = 15_000;
/** The server keeps alive at 1 Hz; silent this long means redial. */
const STREAM_SILENT_MS = 5000;

/**
 * Hold the setup stream open: dial with a fresh `fc.setup` ticket, hand every
 * parsed frame to `onFrame`, and redial with backoff after a close or a silent
 * socket. `onOpen` fires on each (re)connect, so the caller can re-read the
 * full status it may have missed. Returns the stop function.
 */
export function openSetupStream(onFrame: (frame: SetupFrame) => void, onOpen?: () => void): () => void {
  let stopped = false;
  let socket: WebSocket | null = null;
  let retry: number | undefined;
  let watchdog: number | undefined;
  let delay = STREAM_RETRY_MIN_MS;
  const abort = new AbortController();

  const armWatchdog = () => {
    window.clearTimeout(watchdog);
    watchdog = window.setTimeout(() => socket?.close(), STREAM_SILENT_MS);
  };

  const scheduleRedial = () => {
    if (stopped || retry !== undefined) return;
    retry = window.setTimeout(() => {
      retry = undefined;
      void dial();
    }, delay);
    delay = Math.min(STREAM_RETRY_MAX_MS, delay * 2);
  };

  async function dial(): Promise<void> {
    const ticket = await mintWsTicket(FC_SETUP_SCOPE, abort.signal);
    if (stopped) return;
    const ws = new WebSocket(fcSetupUrls.stream(window.location), ticketProtocols(ticket));
    socket = ws;
    ws.onopen = () => {
      armWatchdog();
      onOpen?.();
    };
    ws.onmessage = (ev) => {
      delay = STREAM_RETRY_MIN_MS;
      armWatchdog();
      const frame = typeof ev.data === "string" ? parseFrame(ev.data) : null;
      if (frame) onFrame(frame);
    };
    ws.onclose = () => {
      window.clearTimeout(watchdog);
      if (socket === ws) socket = null;
      scheduleRedial();
    };
  }

  void dial();
  return () => {
    stopped = true;
    abort.abort();
    window.clearTimeout(retry);
    window.clearTimeout(watchdog);
    socket?.close();
    socket = null;
  };
}
