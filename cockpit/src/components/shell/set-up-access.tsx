// The PIN access gate, drawn inside the reach notice whenever the agent refuses
// this browser: an unpaired node (403) or a paired node with no credential for
// us (401). An off-box browser that opens the cockpit therefore lands on this
// form, with the shell still around it.
//
// The flow uses the agent's public PIN routes (`@/shared/api/pin`):
//   1. `GET  /api/dashboard/pin/status` picks the mode: set, enter, locked, or
//      unknown when the read failed (never guessed as `set`).
//   2. `set`   → `POST /api/dashboard/pin/set` (trust-on-first-use), entered
//               twice so a typo cannot lock the owner out.
//      `enter` → `POST /api/dashboard/pin/verify`.
//   3. A success stores the signed session (`setSession`), so every later
//      `apiFetch` carries it and the next poll loads the data.
// A wrong PIN reports the attempts left; a lockout shows its countdown (this
// browser) or the node-wide lock message, and the form returns by itself when
// a per-browser lockout ends.
//
// Loaded lazily by the reach notice, so its locale stays out of the shell.

import { useCallback, useEffect, useState } from "react";
import { KeyRound, RotateCw } from "lucide-react";
import { useTranslation } from "react-i18next";

import { ActionButton } from "@/components/ui/data";
import { canSubmitPin, fmtMMSS, lockRemainingS, pinFeedback, pinMismatch } from "@/lib/access-gate";
import {
  PIN_MAX_LENGTH,
  fetchPinStatus,
  lockMessage,
  setDashboardPin,
  splashMode,
  verifyDashboardPin,
  type LockScope,
  type PinStatus,
  type SplashMode,
} from "@/shared/api/pin";
import "@/shared/i18n";
import { useReachStore } from "@/stores/reach-store";

function PinField({
  value,
  onChange,
  onEnter,
  disabled,
  label,
}: {
  value: string;
  onChange: (v: string) => void;
  onEnter: () => void;
  disabled: boolean;
  label: string;
}) {
  return (
    <input
      type="password"
      inputMode="numeric"
      pattern="[0-9]*"
      autoComplete="off"
      maxLength={PIN_MAX_LENGTH}
      value={value}
      disabled={disabled}
      aria-label={label}
      placeholder={label}
      onChange={(e) => onChange(e.target.value.replace(/\D/g, "").slice(0, PIN_MAX_LENGTH))}
      onKeyDown={(e) => {
        if (e.key === "Enter") onEnter();
      }}
      className="touch-target w-full min-w-0 rounded-lg bg-input px-[0.7rem] text-center font-mono text-[1.1rem] tracking-[0.3em] text-foreground"
    />
  );
}

export function SetUpAccess() {
  const { t } = useTranslation();
  const [mode, setMode] = useState<SplashMode | "loading">("loading");
  const [pin, setPin] = useState("");
  const [confirm, setConfirm] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [lockedUntil, setLockedUntil] = useState<number | null>(null);
  const [lockScope, setLockScope] = useState<LockScope>("peer");
  const [now, setNow] = useState(() => Date.now());

  const applyStatus = useCallback((st: PinStatus | null) => {
    setMode(splashMode(st));
    setLockedUntil(st?.locked ? (st.locked_until ?? null) : null);
    setLockScope(st?.scope === "node" ? "node" : "peer");
  }, []);
  // A failed read is `unknown`, never guessed as `set`.
  const load = useCallback(
    () => fetchPinStatus().then(applyStatus, () => applyStatus(null)),
    [applyStatus],
  );

  useEffect(() => {
    let cancelled = false;
    fetchPinStatus().then(
      (st) => !cancelled && applyStatus(st),
      () => !cancelled && applyStatus(null),
    );
    return () => {
      cancelled = true;
    };
  }, [applyStatus]);

  // Tick the lockout countdown; when a per-browser lockout ends, re-read the
  // status so the form comes back in the right mode.
  const remaining = lockRemainingS(lockedUntil, now);
  useEffect(() => {
    if (lockedUntil == null) return;
    const id = setInterval(() => {
      const t0 = Date.now();
      setNow(t0);
      if (lockRemainingS(lockedUntil, t0) <= 0) {
        clearInterval(id);
        setLockedUntil(null);
        void load();
      }
    }, 1000);
    return () => clearInterval(id);
  }, [lockedUntil, load]);

  const locked = lockedUntil != null && remaining > 0;
  const formMode: SplashMode = mode === "loading" ? "unknown" : locked ? "locked" : mode === "locked" ? "enter" : mode;
  const form = { mode: formMode, pin, confirm, busy, locked };

  const submit = async () => {
    if (!canSubmitPin(form)) return;
    if (pinMismatch(form)) {
      setError(t("access.splash.mismatch"));
      setConfirm("");
      return;
    }
    setBusy(true);
    setError(null);
    const res = formMode === "set" ? await setDashboardPin(pin) : await verifyDashboardPin(pin);
    setBusy(false);
    setPin("");
    setConfirm("");
    const fb = pinFeedback(res);
    switch (fb.kind) {
      case "unlocked":
        // The session is stored; clear the refusal so the next poll loads.
        useReachStore.getState().report(null, true);
        return;
      case "locked":
        setNow(Date.now());
        setLockedUntil(fb.lockedUntil);
        setLockScope(fb.scope);
        return;
      case "wrong":
        setError(
          fb.remaining > 0
            ? t("access.splash.incorrectRemaining", { count: fb.remaining })
            : t("access.splash.incorrect"),
        );
        return;
      case "message":
        setError(fb.message);
        return;
    }
  };

  if (mode === "loading") {
    return <span className="text-[0.78rem] text-muted-foreground">{t("cockpit.access.reading")}</span>;
  }

  if (formMode === "unknown") {
    return (
      <div className="flex flex-col items-center gap-[0.3rem]">
        <span className="text-[0.78rem] text-muted-foreground">{t("access.splash.unknownState")}</span>
        <ActionButton label={t("cockpit.access.retry")} icon={RotateCw} onClick={() => {
            setMode("loading");
            setError(null);
            void load();
          }} />
      </div>
    );
  }

  if (locked) {
    const lockText =
      lockScope === "node" ? lockMessage("node", remaining) : `${lockMessage("peer", remaining)} ${fmtMMSS(remaining)}`;
    return (
      <div role="status" className="flex flex-col items-center gap-[0.2rem] text-[0.8rem]">
        <span className="font-semibold text-err">{t("cockpit.access.locked")}</span>
        <span className="text-muted-foreground">
          {lockText}
        </span>
      </div>
    );
  }

  const setting = formMode === "set";
  return (
    <div className="flex w-full flex-col items-center gap-[0.4rem]">
      <span className="text-[0.78rem] text-muted-foreground">
        {setting ? t("access.splash.setPrompt") : t("access.splash.enterPrompt")}
      </span>
      <div className="flex w-full max-w-[22rem] flex-col items-stretch gap-[0.35rem]">
        <PinField
          value={pin}
          onChange={(v) => {
            setPin(v);
            setError(null);
          }}
          onEnter={() => void submit()}
          disabled={busy}
          label={setting ? t("access.splash.newPin") : t("access.splash.pin")}
        />
        {setting ? (
          <PinField
            value={confirm}
            onChange={(v) => {
              setConfirm(v);
              setError(null);
            }}
            onEnter={() => void submit()}
            disabled={busy}
            label={t("access.splash.confirmPin")}
          />
        ) : null}
        <ActionButton
          label={setting ? t("access.splash.setPin") : t("access.splash.unlock")}
          icon={KeyRound}
          onClick={() => void submit()}
          busy={busy}
          disabled={!canSubmitPin(form)}
          variant="primary"
          full
        />
      </div>
      <span className="text-[0.72rem] text-muted-foreground">
        {setting ? t("access.splash.setFooter") : t("access.splash.enterFooter")}
      </span>
      {error ? <span className="text-[0.75rem] text-err">{error}</span> : null}
    </div>
  );
}
