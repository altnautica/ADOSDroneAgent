import { useEffect, useMemo, useRef, useState } from "react";
import { Loader2, Lock, ShieldCheck } from "lucide-react";

import { Button } from "@/components/ui/button";
import {
  PIN_MAX_LENGTH,
  fetchNodeIdentity,
  isValidPin,
  setDashboardPin,
  verifyDashboardPin,
  type NodeIdentity,
  type PinStatus,
} from "@/lib/pin";

/** One numeric PIN field: 4-12 digits, the agent's own rule (`isValidPin`). */
function PinField({
  value,
  onChange,
  onEnter,
  autoFocus,
  disabled,
  label,
}: {
  value: string;
  onChange: (next: string) => void;
  onEnter?: () => void;
  autoFocus?: boolean;
  disabled?: boolean;
  label: string;
}) {
  return (
    <input
      type="password"
      inputMode="numeric"
      autoComplete="off"
      pattern="[0-9]*"
      maxLength={PIN_MAX_LENGTH}
      disabled={disabled}
      autoFocus={autoFocus}
      aria-label={label}
      value={value}
      onChange={(e) => onChange(e.target.value.replace(/\D/g, "").slice(0, PIN_MAX_LENGTH))}
      onKeyDown={(e) => {
        if (e.key === "Enter" && onEnter) onEnter();
      }}
      className="h-14 w-full rounded-md border border-border bg-background text-center font-mono text-2xl tracking-[0.4em] caret-primary shadow-sm transition-colors focus-visible:border-primary focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-ring disabled:cursor-not-allowed disabled:opacity-50"
    />
  );
}

/** Seconds remaining until `until` (unix seconds), ticking each second; 0 once
 * the window passes. `onExpire` fires when it reaches 0 so the splash can leave
 * the locked state. */
function useCountdown(until: number | null, onExpire: () => void): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (until == null) return;
    const id = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(id);
  }, [until]);
  const remaining = until == null ? 0 : Math.max(0, Math.ceil(until - now / 1000));
  const expiredRef = useRef(false);
  useEffect(() => {
    if (until != null && remaining <= 0 && !expiredRef.current) {
      expiredRef.current = true;
      onExpire();
    }
    if (until != null && remaining > 0) expiredRef.current = false;
  }, [until, remaining, onExpire]);
  return remaining;
}

function formatMMSS(total: number): string {
  const m = Math.floor(total / 60);
  const s = total % 60;
  return `${m}:${String(s).padStart(2, "0")}`;
}

/**
 * Full-screen, ADOS-branded dashboard access gate. Shown by
 * `DashboardAccessGate` when a paired agent is reached off-box without a
 * credential. Three modes off the PIN status:
 *  - `set`   — no PIN yet: the first LAN visitor picks one (trust-on-first-use).
 *  - `enter` — a PIN exists: enter it to unlock.
 *  - locked  — too many wrong attempts: a countdown until the next try.
 */
export function PinSplash({
  status,
  onUnlocked,
}: {
  status: PinStatus | null;
  onUnlocked: () => void;
}) {
  const setMode = !status?.pin_set;
  const [pin, setPin] = useState("");
  const [confirm, setConfirm] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [lockedUntil, setLockedUntil] = useState<number | null>(
    status?.locked ? (status.locked_until ?? null) : null,
  );
  const [node, setNode] = useState<NodeIdentity | null>(null);

  useEffect(() => {
    const ctrl = new AbortController();
    fetchNodeIdentity(ctrl.signal).then(setNode).catch(() => {});
    return () => ctrl.abort();
  }, []);

  const remaining = useCountdown(lockedUntil, () => {
    setLockedUntil(null);
    setError(null);
    setPin("");
    setConfirm("");
  });
  const locked = lockedUntil != null && remaining > 0;

  const canSubmit = !busy && !locked && isValidPin(pin) && (!setMode || isValidPin(confirm));

  const submit = async () => {
    if (!canSubmit) return;
    setError(null);
    if (setMode && pin !== confirm) {
      setError("The PINs do not match.");
      setConfirm("");
      return;
    }
    setBusy(true);
    const res = setMode ? await setDashboardPin(pin) : await verifyDashboardPin(pin);
    setBusy(false);
    if (res.ok) {
      onUnlocked();
      return;
    }
    setPin("");
    setConfirm("");
    if (res.kind === "locked") {
      setLockedUntil(res.lockedUntil);
      setError(null);
    } else if (res.kind === "wrong") {
      setError(
        res.remaining > 0
          ? `Incorrect PIN. ${res.remaining} attempt${res.remaining === 1 ? "" : "s"} left.`
          : "Incorrect PIN.",
      );
    } else {
      setError(res.message);
    }
  };

  const identityLine = useMemo(() => {
    const bits = [node?.name, node?.profile.replace("_", " "), window.location.hostname].filter(
      Boolean,
    );
    return bits.join(" · ");
  }, [node]);

  return (
    <div className="fixed inset-0 z-[200] flex items-center justify-center overflow-y-auto bg-background p-6">
      <div className="w-full max-w-sm">
        {/* Brand lockup */}
        <div className="mb-8 flex flex-col items-center gap-3 text-center">
          <div className="flex items-center gap-2">
            <img src="/brand.svg" alt="" className="h-8 w-8 rounded-md" />
            <span className="text-lg font-semibold tracking-tight">ADOS</span>
          </div>
          <div className="flex items-center gap-1.5 text-xs font-medium uppercase tracking-widest text-warn">
            <Lock className="h-3.5 w-3.5" />
            Dashboard access
          </div>
          {identityLine && (
            <p className="max-w-full truncate text-sm text-muted-foreground">{identityLine}</p>
          )}
        </div>

        {locked ? (
          <div className="flex flex-col items-center gap-4 text-center">
            <p className="text-sm text-muted-foreground">
              Too many attempts. Try again in
            </p>
            <p className="font-mono text-4xl tabular-nums">{formatMMSS(remaining)}</p>
          </div>
        ) : (
          <div className="space-y-6">
            <p className="text-center text-sm text-muted-foreground">
              {setMode
                ? "Set a PIN to unlock this dashboard from other devices on your network."
                : "Enter the PIN to unlock this dashboard."}
            </p>

            <div className="space-y-4">
              <div className="space-y-2">
                {setMode && (
                  <p className="text-center text-xs font-medium text-muted-foreground">Choose a 4–12 digit PIN</p>
                )}
                <PinField
                  label={setMode ? "New PIN" : "PIN"}
                  value={pin}
                  onChange={(v) => {
                    setPin(v);
                    setError(null);
                  }}
                  onEnter={submit}
                  autoFocus
                  disabled={busy}
                />
              </div>

              {setMode && (
                <div className="space-y-2">
                  <p className="text-center text-xs font-medium text-muted-foreground">Confirm PIN</p>
                  <PinField
                    label="Confirm PIN"
                    value={confirm}
                    onChange={(v) => {
                      setConfirm(v);
                      setError(null);
                    }}
                    onEnter={submit}
                    disabled={busy}
                  />
                </div>
              )}
            </div>

            {error && <p className="text-center text-sm text-destructive">{error}</p>}

            <Button className="w-full" onClick={submit} disabled={!canSubmit}>
              {busy ? (
                <Loader2 className="h-4 w-4 animate-spin" />
              ) : setMode ? (
                <ShieldCheck className="h-4 w-4" />
              ) : (
                <Lock className="h-4 w-4" />
              )}
              {setMode ? "Set PIN" : "Unlock"}
            </Button>
          </div>
        )}

        <p className="mt-8 text-center text-xs text-muted-foreground">
          {setMode
            ? "The operator at the device, or Mission Control, can reset this PIN."
            : "Forgot it? Reset the PIN from Mission Control or on the device."}
        </p>
      </div>
    </div>
  );
}
