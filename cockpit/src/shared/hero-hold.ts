// A hero hold: keeps the drone on its `hero` video profile while a surface
// that plays its video is showing.
//
// A WFB drone boots on `thumbnail` (320x180 at 1 fps) and waits to be
// promoted. A hold is a lease on the agent's `POST /api/video/profile`
// (`{profile: "hero", holder, lease_s}`), one per holder id:
// - the first `acquire()` takes the lease at once; it is renewed at a third of
//   the lease, so two renewals can fail before it lapses;
// - a failed request (a 429 request budget, a video service restarting, a
//   dropped connection) is retried after 1, 2, 4 and then 8 s, never dropped;
// - a node without the route (a ground station, `404`) or one that refuses the
//   request outright (`400`) is left alone until the next `acquire()` after a
//   full release;
// - the last `release()` lets go after a short grace with
//   `DELETE ?holder=`, so a surface remounting (a layout change) does not
//   respawn the encoder twice; a page that dies without releasing lapses on
//   its own after `lease_s`.
// The requests skip the auth prompt: a background renewal never asks for a
// PIN.

import { useEffect } from "react";

import { ApiError, apiFetch } from "./api-fetch";

export const VIDEO_PROFILE_PATH = "/api/video/profile";
/** The lease asked for; the agent drops it this long after the last renewal. */
export const HERO_LEASE_S = 30;
export const HERO_RENEW_MS = (HERO_LEASE_S * 1000) / 3;
/** The waits before each retry of a failed request, then every renewal period. */
export const HERO_RETRY_MS: readonly number[] = [1000, 2000, 4000, 8000];
/** How long the last release waits before letting the lease go. */
export const HERO_RELEASE_GRACE_MS = 3000;

/** The part of the profile state a hold reads. */
export interface VideoProfileState {
  profile?: string;
  leases?: { holder: string }[];
}

export interface HeroHold {
  readonly holder: string;
  /** Hold the drone on hero; returns this holder's release (idempotent). */
  acquire: () => () => void;
  /** The latest create/renew request, or null when none went out. */
  answer: () => Promise<VideoProfileState> | null;
  /** Renew now when held and no request is in flight (a page shown again
   *  after the browser throttled its timers). */
  resume: () => void;
  /** Whether some surface holds the drone. */
  active: () => boolean;
}

export function createHeroHold(holder: string, { graceMs = HERO_RELEASE_GRACE_MS }: { graceMs?: number } = {}): HeroHold {
  let count = 0;
  let running = false;
  /** The route is absent or refuses this holder: stop until a full release. */
  let refused = false;
  /** A request went out since the last release, so there may be a lease. */
  let sent = false;
  let inFlight = false;
  let failures = 0;
  let epoch = 0;
  let timer: ReturnType<typeof setTimeout> | undefined;
  let releaseTimer: ReturnType<typeof setTimeout> | undefined;
  let last: Promise<VideoProfileState> | null = null;

  const schedule = (ms: number) => {
    clearTimeout(timer);
    timer = setTimeout(renew, ms);
  };

  function renew() {
    clearTimeout(timer);
    timer = undefined;
    if (!running || refused) return;
    const mine = epoch;
    inFlight = true;
    sent = true;
    const req = apiFetch<VideoProfileState>(VIDEO_PROFILE_PATH, {
      method: "POST",
      body: { profile: "hero", holder, lease_s: HERO_LEASE_S },
      skipAuthSignal: true,
    });
    last = req;
    req.then(
      () => {
        if (mine !== epoch) return;
        inFlight = false;
        failures = 0;
        schedule(HERO_RENEW_MS);
      },
      (err: unknown) => {
        if (mine !== epoch) return;
        inFlight = false;
        if (err instanceof ApiError && (err.status === 404 || err.status === 400)) {
          refused = true;
          sent = false;
          return;
        }
        schedule(HERO_RETRY_MS[failures] ?? HERO_RENEW_MS);
        failures += 1;
      },
    );
  }

  function stop() {
    releaseTimer = undefined;
    if (count > 0 || !running) return;
    epoch += 1;
    running = false;
    refused = false;
    inFlight = false;
    failures = 0;
    clearTimeout(timer);
    timer = undefined;
    last = null;
    if (!sent) return;
    sent = false;
    // A failed release lapses on its own after the lease.
    apiFetch(`${VIDEO_PROFILE_PATH}?holder=${encodeURIComponent(holder)}`, { method: "DELETE", skipAuthSignal: true }).catch(
      () => undefined,
    );
  }

  return {
    holder,
    acquire() {
      count += 1;
      clearTimeout(releaseTimer);
      releaseTimer = undefined;
      if (!running) {
        running = true;
        renew();
      }
      let released = false;
      return () => {
        if (released) return;
        released = true;
        count -= 1;
        if (count > 0) return;
        if (graceMs <= 0) stop();
        else releaseTimer = setTimeout(stop, graceMs);
      };
    },
    answer: () => last,
    resume() {
      if (running && !refused && !inFlight) renew();
    },
    active: () => count > 0,
  };
}

/** The part of `document` a hold watches. */
export interface VisibilitySource {
  readonly visibilityState: string;
  addEventListener: (type: "visibilitychange", cb: () => void) => void;
  removeEventListener: (type: "visibilitychange", cb: () => void) => void;
}

/** Hold `hold` while the page is visible; returns the unbind, which releases. */
export function bindHeroHold(hold: HeroHold, doc: VisibilitySource): () => void {
  let release: (() => void) | null = null;
  const sync = () => {
    if (doc.visibilityState === "visible") {
      if (release) hold.resume();
      else release = hold.acquire();
    } else {
      release?.();
      release = null;
    }
  };
  sync();
  doc.addEventListener("visibilitychange", sync);
  return () => {
    doc.removeEventListener("visibilitychange", sync);
    release?.();
    release = null;
  };
}

/** Hold `hold` while `enabled`, the component is mounted and the page is
 *  visible. */
export function useHeroHold(hold: HeroHold, enabled: boolean): void {
  useEffect(() => (enabled && typeof document !== "undefined" ? bindHeroHold(hold, document) : undefined), [hold, enabled]);
}
