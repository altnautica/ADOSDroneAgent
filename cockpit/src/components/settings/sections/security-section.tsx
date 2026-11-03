// Security: whether the node requires a key and the operator key's rotate and
// clear, the dashboard PIN's state with change and clear, and the auth
// enforcement flags. Enabling MAVLink WebSocket auth can lock out an un-keyed
// ground station, so it waits behind a slide; the PIN and key actions are
// guarded.

import { useCallback, useState, type ReactNode } from "react";
import type { TFunction } from "i18next";
import { useTranslation } from "react-i18next";
import { Copy, KeyRound, RotateCcw, Trash2 } from "lucide-react";

import { ActionButton, EmptyNote } from "@/components/ui/data";
import { Dialog } from "@/components/ui/dialog";
import { GuardedButton } from "@/components/ui/guarded-button";
import { SettingRow, SwitchSettingRow } from "@/components/ui/setting-row";
import {
  PAIRING_POLL_MS,
  PIN_POLL_MS,
  apiKeyView,
  ownKeyRisk,
  pinFormProblem,
  pinFormRequest,
  pinState,
  requestWsEnforce,
  settleWsEnforce,
  type PinFormProblem,
  type PinState,
  type WsEnforceStep,
} from "@/lib/settings/security";
import { CLOSED_GATE, type ConfirmGate } from "@/lib/settings/apply-form";
import { cn } from "@/lib/utils";
import { apiFetch } from "@/shared/api-fetch";
import { getApiKey } from "@/shared/api-key";
import { fetchPinStatus, lockMessage, setDashboardPin, PIN_MAX_LENGTH, type PinSubmitResult } from "@/shared/api/pin";
import {
  clearApiKey,
  clearDashboardPin,
  rotateApiKey,
  rotatedOperatorKey,
  type RotatedApiKey,
} from "@/shared/api/security";
import { toast, toastFromError } from "@/shared/api/toast";
import type { PairingInfoLite } from "@/shared/use-profile";
import { useResource } from "@/shared/use-resource";
import "@/shared/i18n";

import { SectionGroup, SlideConfirmDialog } from "./section-kit";
import { useAgentConfig, useLeafWriter } from "./section-hooks";

const WS_ENFORCE_KEY = "mavlink.ws_proxy_enforce_auth";

/** A state line with a badge and a wrapped explanation. */
function StateBlock({ title, badge, tone, children }: { title: string; badge: string; tone: "ok" | "warn" | "muted"; children: ReactNode }) {
  return (
    <div className="flex flex-col gap-[0.25rem] rounded-md bg-input/30 px-[0.75rem] py-[0.5rem]">
      <div className="flex items-center justify-between gap-[0.75rem]">
        <span className="text-[0.95rem] text-surface-foreground">{title}</span>
        <span
          className={cn(
            "rounded-full border px-[0.5rem] text-[0.75rem]",
            tone === "ok" && "border-ok/50 text-ok",
            tone === "warn" && "border-warn/50 text-warn",
            tone === "muted" && "border-border text-muted-foreground",
          )}
        >
          {badge}
        </span>
      </div>
      <p className="text-[0.8rem] leading-relaxed text-muted-foreground">{children}</p>
    </div>
  );
}

function Hint({ children }: { children: ReactNode }) {
  return <p className="px-[0.75rem] pb-[0.3rem] text-[0.75rem] leading-relaxed text-muted-foreground">{children}</p>;
}

function ApiKeyGroup({ configuredApiKey }: { configuredApiKey: unknown }) {
  const { t } = useTranslation();
  const pairing = useResource(
    useCallback((s: AbortSignal) => apiFetch<PairingInfoLite | null>("/api/pairing/info", { signal: s, skipAuthSignal: true }), []),
    PAIRING_POLL_MS,
  );
  const view = apiKeyView({
    pairing: pairing.data ?? null,
    pairingError: pairing.stale,
    configuredApiKey,
  });
  const [busy, setBusy] = useState(false);
  const [revealed, setRevealed] = useState<RotatedApiKey | null>(null);
  // Read each render: a rotation or a clear re-renders through `busy`.
  const risk = ownKeyRisk({ storedKey: getApiKey(), rotatedKey: rotatedOperatorKey(), view });

  const badge =
    view.state === "loading"
      ? t("common.dash")
      : view.state === "unknown"
        ? t("settingsMore.security.badgeUnknown")
        : view.state === "required"
          ? t("settingsMore.security.badgeSet")
          : t("settingsMore.security.badgeNotSet");

  const rotate = async () => {
    setBusy(true);
    try {
      // The client stores the new key for this panel before it returns.
      setRevealed(await rotateApiKey());
    } catch (err) {
      toastFromError(err, t("cockpit.settingsSections.security.rotateFailed"));
    } finally {
      setBusy(false);
    }
  };

  const clear = async () => {
    setBusy(true);
    try {
      // A stored key the node now refuses is dropped and the access gate is
      // raised by the client, so the panel goes straight to the PIN.
      const res = await clearApiKey();
      toast.ok(
        t(
          res.droppedStoredKey
            ? "cockpit.settingsSections.security.keyClearedOwn"
            : "cockpit.settingsSections.security.keyCleared",
        ),
      );
    } catch (err) {
      toastFromError(err, t("cockpit.settingsSections.security.clearKeyFailed"));
    } finally {
      setBusy(false);
    }
  };

  const copy = async (value: string) => {
    try {
      await navigator.clipboard.writeText(value);
      toast.ok(t("cockpit.settingsSections.security.copied"));
    } catch {
      toast.err(t("cockpit.settingsSections.security.copyFailed"));
    }
  };

  return (
    <SectionGroup title={t("cockpit.settingsSections.security.groupKey")}>
      <StateBlock
        title={t("settingsMore.security.apiKeyTitle")}
        badge={badge}
        tone={view.state === "required" ? "ok" : "muted"}
      >
        {view.state === "unknown"
          ? t("settingsMore.security.apiKeyReadError")
          : view.state === "required"
            ? view.configuredKey
              ? `${t("settingsMore.security.apiKeySet")} ${t("settingsMore.security.apiKeySecondKey")}`
              : t("settingsMore.security.apiKeySet")
            : view.state === "notRequired"
              ? view.configuredKey
                ? `${t("settingsMore.security.apiKeyNotSet")} ${t("settingsMore.security.apiKeyConfiguredUnpaired")}`
                : t("settingsMore.security.apiKeyNotSet")
              : t("common.loading")}
      </StateBlock>
      <Hint>{t("cockpit.settingsSections.security.keyHint")}</Hint>
      {risk !== "none" ? (
        <Hint>
          {t(
            risk === "operator"
              ? "cockpit.settingsSections.security.clearKeyOwn"
              : "cockpit.settingsSections.security.clearKeyMaybeOwn",
          )}
        </Hint>
      ) : null}
      <div className="flex flex-wrap gap-[0.5rem] px-[0.75rem]">
        <GuardedButton
          label={t("cockpit.settingsSections.security.rotate")}
          guardLabel={t("cockpit.settingsSections.security.rotateGuard")}
          icon={RotateCcw}
          disabled={busy}
          onFired={() => void rotate()}
        />
        <GuardedButton
          label={t("cockpit.settingsSections.security.clearKey")}
          guardLabel={t(
            risk === "none"
              ? "cockpit.settingsSections.security.clearKeyGuard"
              : "cockpit.settingsSections.security.clearKeyGuardOwn",
          )}
          icon={Trash2}
          disabled={busy || !view.configuredKey}
          onFired={() => void clear()}
        />
      </div>
      {revealed ? (
        <Dialog
          title={t("cockpit.settingsSections.security.newKeyTitle")}
          onDismiss={() => setRevealed(null)}
          dismiss={
            <ActionButton label={t("cockpit.settingsSections.security.copy")} icon={Copy} onClick={() => void copy(revealed.api_key)} />
          }
          confirm={
            <ActionButton label={t("cockpit.settingsSections.security.done")} variant="primary" onClick={() => setRevealed(null)} />
          }
        >
          <div className="flex flex-col gap-[0.5rem]">
            <span>{t("cockpit.settingsSections.security.newKeyBody")}</span>
            <code className="select-all break-all rounded-md bg-input px-[0.6rem] py-[0.5rem] font-mono text-[0.9rem] text-surface-foreground">
              {revealed.api_key}
            </code>
            {revealed.rotated_at ? (
              <span className="text-[0.75rem]">
                {t("cockpit.settingsSections.security.rotatedAt", { at: new Date(revealed.rotated_at).toLocaleString() })}
              </span>
            ) : null}
          </div>
        </Dialog>
      ) : null}
    </SectionGroup>
  );
}

function pinBadge(state: PinState, t: TFunction): string {
  switch (state) {
    case "loading":
      return t("common.dash");
    case "unknown":
      return t("settingsMore.security.badgeUnknown");
    case "locked":
      return t("settingsMore.security.badgeLocked");
    case "set":
      return t("settingsMore.security.badgeSet");
    default:
      return t("settingsMore.security.badgeNotSet");
  }
}

function PinInput({ label, value, onChange }: { label: string; value: string; onChange: (v: string) => void }) {
  return (
    <SettingRow title={label}>
      <input
        type="password"
        inputMode="numeric"
        autoComplete="off"
        maxLength={PIN_MAX_LENGTH}
        value={value}
        onChange={(e) => onChange(e.target.value.replace(/\D/g, ""))}
        aria-label={label}
        className="touch-target w-[clamp(7.5rem,40%,14rem)] rounded-md border border-border bg-input px-[0.6rem] font-mono text-[0.9rem] text-surface-foreground focus:border-primary focus:outline-none"
      />
    </SettingRow>
  );
}

function PinGroup() {
  const { t } = useTranslation();
  const status = useResource(useCallback((s: AbortSignal) => fetchPinStatus(s), []), PIN_POLL_MS);
  const state = pinState(status.data, status.stale);
  const pinSet = status.data?.pin_set === true;
  const [current, setCurrent] = useState("");
  const [pin, setPin] = useState("");
  const [confirm, setConfirm] = useState("");
  const [busy, setBusy] = useState(false);

  const form = { pinSet, current, pin, confirm };
  const problem = pinFormProblem(form);
  const touched = current !== "" || pin !== "" || confirm !== "";

  const problemText = (p: PinFormProblem): string => {
    switch (p) {
      case "current":
        return t("cockpit.settingsSections.security.problemCurrent");
      case "pin":
        return t("access.splash.choose");
      case "mismatch":
        return t("access.splash.mismatch");
      default:
        return t("cockpit.settingsSections.security.problemSame");
    }
  };

  const reportFailure = (res: Exclude<PinSubmitResult, { ok: true }>) => {
    switch (res.kind) {
      case "wrong":
        toast.err(
          res.remaining > 0
            ? t("access.splash.incorrectRemaining", { count: res.remaining })
            : t("access.splash.incorrect"),
        );
        return;
      case "locked":
        toast.err(
          res.scope === "node"
            ? lockMessage("node", Math.max(0, res.lockedUntil - Date.now() / 1000))
            : t("cockpit.settingsSections.security.pinPeerLocked"),
        );
        return;
      default:
        toast.err(res.message);
    }
  };

  const submit = async () => {
    const req = pinFormRequest(form);
    if (!req) return;
    setBusy(true);
    try {
      const res = await setDashboardPin(req.pin, req.currentPin);
      if (res.ok) {
        toast.ok(t(pinSet ? "cockpit.settingsSections.security.pinChanged" : "cockpit.settingsSections.security.pinSaved"));
        setCurrent("");
        setPin("");
        setConfirm("");
      } else {
        reportFailure(res);
      }
    } finally {
      setBusy(false);
      status.refresh();
    }
  };

  const clear = async () => {
    setBusy(true);
    try {
      await clearDashboardPin();
      toast.ok(t("cockpit.settingsSections.security.pinCleared"));
    } catch (err) {
      toastFromError(err, t("cockpit.settingsSections.security.clearPinFailed"));
    } finally {
      setBusy(false);
      status.refresh();
    }
  };

  return (
    <SectionGroup title={t("cockpit.settingsSections.security.groupPin")}>
      <StateBlock
        title={t("settingsMore.security.pinTitle")}
        badge={pinBadge(state, t)}
        tone={state === "locked" ? "warn" : state === "set" ? "ok" : "muted"}
      >
        {state === "unknown"
          ? t("settingsMore.security.pinReadError")
          : state === "loading"
            ? t("common.loading")
            : pinSet
              ? t("cockpit.settingsSections.security.pinSetHere")
              : t("cockpit.settingsSections.security.pinNotSetHere")}
      </StateBlock>
      {state === "loading" || state === "unknown" ? null : (
        <>
          {pinSet ? (
            <PinInput label={t("cockpit.settingsSections.security.currentPin")} value={current} onChange={setCurrent} />
          ) : null}
          <PinInput label={t("access.splash.newPin")} value={pin} onChange={setPin} />
          <PinInput label={t("access.splash.confirmPin")} value={confirm} onChange={setConfirm} />
          {touched && problem ? <Hint>{problemText(problem)}</Hint> : null}
          <div className="flex flex-wrap gap-[0.5rem] px-[0.75rem]">
            <GuardedButton
              label={t(pinSet ? "cockpit.settingsSections.security.changePin" : "access.splash.setPin")}
              guardLabel={t(
                pinSet ? "cockpit.settingsSections.security.changePinGuard" : "cockpit.settingsSections.security.setPinGuard",
              )}
              icon={KeyRound}
              disabled={busy || problem !== null}
              onFired={() => void submit()}
            />
            {pinSet ? (
              <GuardedButton
                label={t("cockpit.settingsSections.security.clearPin")}
                guardLabel={t("cockpit.settingsSections.security.clearPinGuard")}
                icon={Trash2}
                disabled={busy}
                onFired={() => void clear()}
              />
            ) : null}
          </div>
        </>
      )}
    </SectionGroup>
  );
}

function AuthGroup({ wsEnforce, hmac, setupToken }: { wsEnforce: boolean; hmac: boolean; setupToken: boolean }) {
  const { t } = useTranslation();
  const { save, busy } = useLeafWriter();
  const [gate, setGate] = useState<ConfirmGate<true>>(CLOSED_GATE);

  const run = async (step: WsEnforceStep) => {
    setGate(step.gate);
    if (step.write === null) return;
    if (await save(WS_ENFORCE_KEY, step.write, { quiet: true })) {
      toast.ok(t(step.write ? "settingsMore.security.wsEnforceOn" : "settingsMore.security.wsEnforceOff"));
    }
  };

  return (
    <SectionGroup title={t("cockpit.settingsSections.security.groupAuth")}>
      <SwitchSettingRow
        title={t("settingsMore.security.wsEnforceLabel")}
        checked={wsEnforce}
        enabled={!busy}
        onChange={(next) => void run(requestWsEnforce(next))}
      />
      <Hint>{t("settingsMore.security.wsEnforceHint")}</Hint>
      <SwitchSettingRow
        title={t("settingsMore.security.hmacLabel")}
        checked={hmac}
        enabled={!busy}
        onChange={(next) => void save("security.hmac_enabled", next)}
      />
      <Hint>{t("settingsMore.security.hmacHint")}</Hint>
      <SwitchSettingRow
        title={t("settingsMore.security.setupTokenLabel")}
        checked={setupToken}
        enabled={!busy}
        onChange={(next) => void save("security.setup_token_required", next)}
      />
      <Hint>{t("settingsMore.security.setupTokenHint")}</Hint>
      {gate.open ? (
        <SlideConfirmDialog
          title={t("settingsMore.security.wsEnforceConfirmTitle")}
          slideLabel={t("cockpit.settingsSections.security.wsEnforceSlide")}
          onConfirmed={() => void run(settleWsEnforce(gate, true))}
          onDismiss={() => void run(settleWsEnforce(gate, false))}
        >
          {t("settingsMore.security.wsEnforceConfirmBody")}
        </SlideConfirmDialog>
      ) : null}
    </SectionGroup>
  );
}

export function SecuritySection() {
  const { t } = useTranslation();
  const { config, ready, get } = useAgentConfig();

  if (!ready) return <EmptyNote>{t("settingsMore.security.readingConfig")}</EmptyNote>;
  if (config == null) return <EmptyNote>{t("settingsMore.security.readError")}</EmptyNote>;

  return (
    <div className="flex flex-col gap-[0.9rem]">
      <ApiKeyGroup configuredApiKey={get<unknown>("security.api.api_key", null)} />
      <PinGroup />
      {config.security ? (
        <AuthGroup
          wsEnforce={get<boolean>(WS_ENFORCE_KEY, false)}
          hmac={get<boolean>("security.hmac_enabled", false)}
          setupToken={get<boolean>("security.setup_token_required", false)}
        />
      ) : (
        <EmptyNote>{t("settingsMore.security.flagsUnavailable")}</EmptyNote>
      )}
    </div>
  );
}
