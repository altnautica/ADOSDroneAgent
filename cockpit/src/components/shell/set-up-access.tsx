// "Set up access" — the PIN-gated cockpit-door flow for a browser reaching the
// node from the trusted LAN while the node is not paired.
//
// An unpaired node answers its own identity and refuses every DATA route to an
// ordinary LAN peer; the owner's browser lands on the cockpit with every field
// dashed and the pairing code in the banner. Pairing from Mission Control is
// one way out. This is the other, the one the founder chose over blind
// trusted-network-open: set or enter the dashboard PIN, and the browser holds a
// short-lived dashboard session that the agent accepts as the data-plane
// credential while the node stays unpaired.
//
// The flow mirrors the dashboard's PIN splash exactly (and reuses its routes):
//   1. `GET  /api/dashboard/pin/status` → is a PIN already set?
//   2. No  → `POST /api/dashboard/pin/set`     (trust-on-first-use, as the
//           dashboard-PIN store already authorizes `!pin_set` in-handler)
//      Yes → `POST /api/dashboard/pin/verify`  (enter the existing PIN)
//   3. The successful response carries a signed `session` + `expires_at`,
//      stored via `setSession` — the same key the dashboard writes, so every
//      `/api/*` call (apiFetch reads `getSession`) now carries
//      `X-ADOS-Dashboard-Session` and the data loads on the next poll.
//
// The route that answers "who are you" is deliberately public, so /pin/status,
// /pin/set and /pin/verify are reachable before any credential exists; the data
// routes behind them stay gated until the session is held.

import { useState } from "react";
import { KeyRound } from "lucide-react";

import { ActionButton } from "@/components/ui/data";
import { apiFetch, ApiError } from "@/lib/api";
import { setSession } from "@/lib/session";
import { useReachStore } from "@/stores/reach-store";

interface PinStatus {
  pin_set?: boolean;
  locked?: boolean;
  locked_until?: number | null;
}

interface PinResponse {
  ok?: boolean;
  session?: string;
  expires_at?: number;
  remaining_attempts?: number;
  locked_until?: number;
}

/** Minimum PIN length, mirroring the agent's dashboard_pin MIN_PIN_LEN. */
const MIN_PIN = 4;

export function SetUpAccess() {
  const [mode, setMode] = useState<"idle" | "set" | "enter">("idle");
  const [pin, setPin] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const begin = async () => {
    if (busy) return;
    setBusy(true);
    setError(null);
    try {
      const status = await apiFetch<PinStatus>("/api/dashboard/pin/status");
      setMode(status.pin_set ? "enter" : "set");
    } catch {
      setError("Could not read the access-PIN status on this node.");
      setMode("idle");
    } finally {
      setBusy(false);
    }
  };

  const submit = async () => {
    if (busy) return;
    if (!pin || pin.length < MIN_PIN) {
      setError("Enter at least 4 digits.");
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const body = { pin };
      const res = await apiFetch<PinResponse>(
        mode === "set" ? "/api/dashboard/pin/set" : "/api/dashboard/pin/verify",
        { method: "POST", body },
      );
      if (res.session) {
        setSession(res.session, res.expires_at ?? 0);
      }
      // Success clears the refusal: the next poll carries the session and the
      // DATA surfaces load without a reload.
      useReachStore.getState().report(null, true);
      setPin("");
      setMode("idle");
    } catch (e) {
      const msg = e instanceof ApiError ? e.message : "Access setup failed.";
      setError(msg);
    } finally {
      setBusy(false);
    }
  };

  const cancel = () => {
    setMode("idle");
    setPin("");
    setError(null);
  };

  const open = mode === "set" || mode === "enter";

  return (
    <div className="flex w-full flex-col items-center gap-[0.4rem]">
      {!open ? (
        <ActionButton
          label="Set up access with the dashboard PIN"
          icon={KeyRound}
          onClick={begin}
          busy={busy}
          full
        />
      ) : (
        <>
          <span className="text-[0.78rem] text-muted-foreground">
            {mode === "set"
              ? "No access PIN is set yet. Choose a 4–12 digit PIN:"
              : "Enter this node&apos;s dashboard PIN to unlock its data:"}
          </span>
          <div className="flex w-full max-w-[20rem] items-center gap-[0.4rem]">
            <input
              type="password"
              inputMode="numeric"
              pattern="[0-9]*"
              autoComplete="off"
              maxLength={12}
              value={pin}
              disabled={busy}
              onChange={(e) => {
                setPin(e.target.value.replace(/\D/g, ""));
                setError(null);
              }}
              onKeyDown={(e) => {
                if (e.key === "Enter") submit();
              }}
              aria-label={mode === "set" ? "Choose a PIN" : "Enter the PIN"}
              className="touch-target w-full min-w-0 rounded-lg bg-input px-[0.7rem] text-center font-mono text-[1.1rem] tracking-[0.3em] text-foreground"
            />
            <ActionButton label="Unlock" onClick={submit} busy={busy} variant="primary" />
          </div>
          <button
            type="button"
            onClick={cancel}
            disabled={busy}
            className="touch-target text-[0.72rem] text-muted-foreground hover:text-foreground"
          >
            Cancel
          </button>
        </>
      )}
      {error ? <span className="text-[0.74rem] text-err">{error}</span> : null}
    </div>
  );
}
