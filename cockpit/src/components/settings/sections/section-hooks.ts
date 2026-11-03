// Hooks and helpers shared by the agent settings sections: a config leaf
// reader and a checked single-leaf writer (toasts the outcome, refreshes the
// shared config snapshot), guarded single-leaf writes, the armed-override
// confirm fed to `withArmedRetry`, and the residual-route error helpers. The
// components they pair with live in `section-kit.tsx`.

import { createElement, useCallback, useRef, useState, type ReactNode } from "react";
import { useTranslation } from "react-i18next";

import { ArmedConfirmDialog } from "@/components/settings/sections/section-kit";
import { CLOSED_GATE, settleGate, type ConfirmGate } from "@/lib/settings/apply-form";
import { stepLeafChange, type LeafChange } from "@/lib/settings/guarded-leaf";
import { getAtPath } from "@/lib/settings-schema";
import { useConfigStore, useConfigSubscription } from "@/stores/config-store";
import { putConfigChecked } from "@/shared/api/apply-actions";
import { ApiError } from "@/shared/api-fetch";
import { toast, toastFromError } from "@/shared/api/toast";
import "@/shared/i18n";

/** The shared `/api/config` snapshot plus a typed leaf getter. */
export function useAgentConfig() {
  useConfigSubscription();
  const config = useConfigStore((s) => s.config);
  const ready = useConfigStore((s) => s.ready);
  const get = useCallback(
    <T>(path: string, fallback: T): T => {
      if (config == null) return fallback;
      const v = getAtPath(config, path);
      return v === undefined || v === null ? fallback : (v as T);
    },
    [config],
  );
  return { config, ready, get };
}

/** How a leaf write reaches the agent; the default is the plain checked PUT. */
export type LeafWrite = (key: string, value: string) => Promise<unknown>;

const plainWrite: LeafWrite = (key, value) => putConfigChecked(key, value);

/** Write one dot-path leaf with the soft-error check. Resolves true on a
 *  stored write (toasting either way) and refreshes the shared snapshot so the
 *  page re-reads the coerced value; a refused write leaves the snapshot, which
 *  is the rollback. */
export function useLeafWriter(write: LeafWrite = plainWrite) {
  const { t } = useTranslation();
  const [busy, setBusy] = useState(false);
  const save = useCallback(
    async (key: string, value: string | number | boolean, opts: { quiet?: boolean } = {}): Promise<boolean> => {
      setBusy(true);
      try {
        await write(key, String(value));
        if (!opts.quiet) toast.ok(t("cockpit.settingsSections.saved"));
        return true;
      } catch (err) {
        const msg = err instanceof Error ? err.message : String(err);
        toast.err(t("cockpit.settingsSections.saveFailed", { error: msg }));
        return false;
      } finally {
        setBusy(false);
        void useConfigStore.getState().refresh();
      }
    },
    [t, write],
  );
  return { save, busy };
}

/** The armed-override confirm: `confirm()` opens a dialog with a slide and
 *  resolves true only when the operator slides it. Render `dialog`. */
export function useArmedConfirm(): { confirm: () => Promise<boolean>; dialog: ReactNode } {
  // The pending confirm's resolver; non-null while the dialog is open.
  const [resolver, setResolver] = useState<((ok: boolean) => void) | null>(null);

  const confirm = useCallback(
    () =>
      new Promise<boolean>((resolve) => {
        setResolver(() => resolve);
      }),
    [],
  );

  const dialog = resolver
    ? createElement(ArmedConfirmDialog, {
        onResult: (ok: boolean) => {
          resolver(ok);
          setResolver(null);
        },
      })
    : null;

  return { confirm, dialog };
}

/** True for the answer a residual (py) route gives when the setup service is absent. */
export function isResidualMissing(err: unknown): boolean {
  return err instanceof ApiError && err.status === 501;
}

/** Toast an action error, naming the absent setup service on a 501. */
export function toastActionError(err: unknown, residualMessage: string): void {
  if (isResidualMissing(err)) toast.err(residualMessage);
  else toastFromError(err);
}

/** Guarded single-leaf writes. `request(change, guarded)` writes an unguarded
 *  change at once; a guarded one is held, unsent, until `fire()` (resolving
 *  with the write's outcome) or `cancel()` (resolving false). `write` swaps the
 *  transport (keep it stable). Render `PendingLeafBar` while `pending` is set. */
export function useGuardedLeaf(write?: LeafWrite) {
  const { save, busy } = useLeafWriter(write);
  const [gate, setGate] = useState<ConfirmGate<LeafChange>>(CLOSED_GATE);
  const settle = useRef<((ok: boolean) => void) | null>(null);

  const request = useCallback(
    (change: LeafChange, guarded: boolean): Promise<boolean> => {
      const step = stepLeafChange(change, guarded);
      if (step.kind === "write") return save(step.change.key, step.change.value);
      // A newer held change supersedes the one waiting.
      settle.current?.(false);
      return new Promise<boolean>((resolve) => {
        settle.current = resolve;
        setGate(step.gate);
      });
    },
    [save],
  );

  const finish = useCallback(
    async (confirmed: boolean) => {
      const change = settleGate(gate, confirmed);
      const resolve = settle.current;
      settle.current = null;
      setGate(CLOSED_GATE);
      resolve?.(change ? await save(change.key, change.value) : false);
    },
    [gate, save],
  );

  return {
    request,
    pending: gate.open ? gate.payload : null,
    fire: () => void finish(true),
    cancel: () => void finish(false),
    busy,
  };
}
