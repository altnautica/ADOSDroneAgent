// Pair — local-first pairing over the LAN/RF. Four facets, all honest reads:
//   1. Mission Control owner: whether a GCS has claimed this node, its owner,
//      and the pairing code + mDNS reach a GCS uses to claim it
//      (`GET /api/pairing/info`). An unpaired node also accepts a code issued by
//      Mission Control (`POST /api/pairing/accept`; the route answers 200 with
//      `ok:false` on refusal), and a paired one can be released
//      (`POST /api/pairing/unpair`, guarded).
//   2. The drone RF link: the WFB pair state (peer device, key fingerprint,
//      auto-pair) from `GET /api/wfb/pair`, plus a Pair-drone action that opens a
//      local bind window (`POST /api/wfb/pair/local-bind`) and an Unpair action
//      (`POST /api/wfb/pair/unpair`, guarded — it wipes the key and disables
//      auto-bind). The open mesh pair window (`GET .../pair/pending`) surfaces as
//      a countdown strip while a window is open.
//   3. Phone receivers (ground station only): phones with their own radio
//      waiting for the fleet receive keys (`GET /api/v1/ground-station/wfb/invite`),
//      approved or rejected after the operator compares fingerprints.
//   4. Browser access (PIN).

import { bindOutcome, bindView, type BindSession } from "@/lib/bind-phase";
import { useCallback, useState } from "react";
import { Check, KeyRound, Link2, Smartphone, Unlink, X } from "lucide-react";
import { useTranslation } from "react-i18next";

import { SetUpAccess } from "@/components/shell/set-up-access";
import { ConsoleScaffold } from "@/components/ui/console-scaffold";
import { ActionButton, Dot, EmptyNote, ErrorNote, Row, SectionHeader, StaleBadge } from "@/components/ui/data";
import { readFailed } from "@/lib/read-failed";
import { GuardedButton } from "@/components/ui/guarded-button";
import { TextSettingRow } from "@/components/ui/setting-row";
import { fmtCountdown } from "@/lib/format-status";
import { useArmedConfirm } from "@/components/settings/sections/section-hooks";
import { acceptPairingCode, bindCancellable, normalizePairingCode, PAIR_ACTION_GUARD, unpair } from "@/lib/pairing-api";
import { getArmedReading } from "@/lib/radio-gate";
import type { Tone } from "@/lib/tone";
import { apiFetch } from "@/shared/api-fetch";
import { toast, toastFromError } from "@/shared/api/toast";
import { DASH } from "@/shared/format";
import "@/shared/i18n";
import { useProfile } from "@/shared/use-profile";
import { useResource } from "@/shared/use-resource";

interface PairingInfo {
  device_id?: string | null;
  name?: string | null;
  paired?: boolean;
  owner_id?: string | null;
  /** Unix seconds. */
  paired_at?: number | null;
  pairing_code?: string | null;
  mdns_host?: string | null;
}
interface WfbPair {
  paired?: boolean;
  paired_with_device_id?: string | null;
  paired_at?: string | null;
  fingerprint?: string | null;
  auto_pair_enabled?: boolean;
  role?: string | null;
}
interface PairPending {
  open?: boolean;
  opened_at_ms?: number;
  closes_at_ms?: number;
  pending?: unknown[];
}
interface PhoneInvite {
  invite_id: string;
  label: string;
  phone_fingerprint: string;
  expires_at_ms: number;
}

function PhoneReceivers() {
  const { t } = useTranslation();
  const invites = useResource<{ pending: PhoneInvite[] }>(
    useCallback((s) => apiFetch<{ pending: PhoneInvite[] }>("/api/v1/ground-station/wfb/invite", { signal: s }), []),
    2000,
  );
  const [busyId, setBusyId] = useState<string | null>(null);

  const decide = async (id: string, decision: "approve" | "reject") => {
    if (busyId) return;
    setBusyId(id);
    try {
      await apiFetch(`/api/v1/ground-station/wfb/invite/${encodeURIComponent(id)}/${decision}`, {
        method: "POST",
        body: {},
      });
    } catch (err) {
      toastFromError(err, t("pairing.phones.decideError"));
    } finally {
      setBusyId(null);
      invites.refresh();
    }
  };

  const pending = invites.data?.pending ?? [];
  return (
    <>
      <div className="flex items-center justify-between">
        <SectionHeader>{t("pairing.phones.title")}</SectionHeader>
        <StaleBadge stale={invites.stale && invites.data !== null} />
      </div>
      {invites.data === null && invites.error ? (
        <EmptyNote>{t("pairing.phones.readError")}</EmptyNote>
      ) : invites.data === null ? (
        <EmptyNote>{t("pairing.page.loading")}</EmptyNote>
      ) : pending.length === 0 ? (
        <EmptyNote>{t("pairing.phones.empty")}</EmptyNote>
      ) : (
        pending.map((p) => (
          <div key={p.invite_id} className="flex flex-col gap-[0.3rem] rounded-md bg-input/40 px-[0.6rem] py-[0.5rem]">
            <div className="flex items-center gap-[0.4rem]">
              <Smartphone className="h-[1rem] w-[1rem] text-muted-foreground" aria-hidden />
              <span className="text-[0.85rem] text-surface-foreground">{p.label}</span>
            </div>
            <span className="select-text font-mono text-[1rem] tracking-[0.08em] text-hud-primary">
              {p.phone_fingerprint}
            </span>
            <div className="flex gap-[0.4rem]">
              <ActionButton
                label={t("pairing.phones.reject")}
                icon={X}
                onClick={() => void decide(p.invite_id, "reject")}
                disabled={busyId !== null}
                full
              />
              <ActionButton
                label={t("pairing.phones.approve")}
                icon={Check}
                variant="primary"
                onClick={() => void decide(p.invite_id, "approve")}
                busy={busyId === p.invite_id}
                disabled={busyId !== null}
                full
              />
            </div>
          </div>
        ))
      )}
      <p className="mt-[0.3rem] px-[0.2rem] text-[0.75rem] text-muted-foreground">{t("pairing.phones.description")}</p>
    </>
  );
}

/** An unpair rendered through its declared confirm gate (`PAIR_ACTION_GUARD`,
 *  pinned by the pairing test): `guarded` arms then holds; a gate set to `none`
 *  would fall back to a plain danger button. */
function UnpairAction({
  gate,
  label,
  guardLabel,
  onFired,
  disabled,
}: {
  gate: (typeof PAIR_ACTION_GUARD)[keyof typeof PAIR_ACTION_GUARD];
  label: string;
  guardLabel: string;
  onFired: () => void;
  disabled: boolean;
}) {
  if (gate === "guarded") {
    return <GuardedButton label={label} guardLabel={guardLabel} icon={Unlink} onFired={onFired} disabled={disabled} full />;
  }
  return <ActionButton label={label} icon={Unlink} variant="danger" onClick={onFired} disabled={disabled} full />;
}

/** The arm state for wording a declined unpair; null when it cannot be read. */
const readArmed = () => getArmedReading().catch(() => null);

function CloudOwner({
  info,
  failed,
  onChanged,
  confirmArmed,
}: {
  info: PairingInfo | null;
  /** The owner read failed with nothing to show. */
  failed: boolean;
  onChanged: () => void;
  confirmArmed: () => Promise<boolean>;
}) {
  const { t } = useTranslation();
  const profile = useProfile();
  const [code, setCode] = useState("");
  const [busy, setBusy] = useState<null | "accept" | "unpair">(null);
  const [refusal, setRefusal] = useState<string | null>(null);

  const subject =
    profile === "ground_station"
      ? t("pairing.subject.groundStation")
      : profile === "visor"
        ? t("pairing.subject.visor")
        : t("pairing.subject.drone");
  const claimed = info?.paired ?? false;
  const normalized = normalizePairingCode(code);

  const accept = () => {
    if (busy || !normalized) return;
    setBusy("accept");
    setRefusal(null);
    acceptPairingCode(normalized, t("pairing.accept.failedDefault"))
      .then(() => {
        toast.ok(t("pairing.accept.success"));
        setCode("");
      })
      .catch((err: unknown) => setRefusal(err instanceof Error ? err.message : t("pairing.accept.failed")))
      .finally(() => {
        setBusy(null);
        onChanged();
      });
  };

  const unpairOwner = () => {
    if (busy) return;
    setBusy("unpair");
    unpair("cloud", confirmArmed, readArmed)
      .then((res) => (res === "done" ? toast.ok(t("cockpit.pair.unpaired")) : toast.err(t(`cockpit.radio.${res}`))))
      .catch((err: unknown) => toastFromError(err, t("pairing.page.unpairFailed")))
      .finally(() => {
        setBusy(null);
        onChanged();
      });
  };

  return (
    <>
      <SectionHeader>{t("cockpit.pair.owner")}</SectionHeader>
      <p className="mb-[0.2rem] px-[0.15rem] text-[0.75rem] text-muted-foreground">
        {t("pairing.page.blurb", { subject })}
      </p>
      {info == null ? (
        failed ? (
          <ErrorNote onRetry={onChanged}>{t("cockpit.readFailed.pairing")}</ErrorNote>
        ) : (
          <EmptyNote>{t("pairing.page.loading")}</EmptyNote>
        )
      ) : (
        <>
          <Row
            label={t("cockpit.pair.claimed")}
            left={<Dot tone={claimed ? "ok" : "muted"} />}
            value={claimed ? t("pairing.page.paired") : t("pairing.page.awaiting")}
            tone={claimed ? "ok" : "muted"}
          />
          {claimed && info.owner_id ? <Row label={t("pairing.page.owner")} value={info.owner_id} /> : null}
          {claimed && info.paired_at ? (
            <Row label={t("cockpit.pair.since")} value={new Date(info.paired_at * 1000).toLocaleString()} mono={false} />
          ) : null}
          {info.name ? <Row label={t("cockpit.pair.name")} value={info.name} mono={false} /> : null}
          {info.mdns_host ? <Row label={t("cockpit.pair.reach")} value={info.mdns_host} /> : null}
        </>
      )}

      {info && claimed ? (
        <div className="mt-[0.4rem]">
          <UnpairAction
            gate={PAIR_ACTION_GUARD.cloudUnpair}
            label={t("cockpit.pair.unpairOwner")}
            guardLabel={t("cockpit.pair.holdToUnpair")}
            onFired={unpairOwner}
            disabled={busy !== null}
          />
          <p className="mt-[0.3rem] px-[0.2rem] text-[0.75rem] text-muted-foreground">{t("pairing.confirm.description")}</p>
        </div>
      ) : null}

      {info && !claimed ? (
        <>
          {info.pairing_code ? (
            <div className="mt-[0.3rem] flex flex-col items-center rounded-md bg-input/40 px-[0.6rem] py-[0.6rem]">
              <span className="text-[0.75rem] uppercase tracking-wide text-muted-foreground">{t("cockpit.pair.codeLabel")}</span>
              <span className="select-text font-mono text-[1.7rem] font-semibold tracking-[0.2em] text-hud-primary">
                {info.pairing_code}
              </span>
              {info.mdns_host ? (
                <span className="select-text text-[0.75rem] text-muted-foreground">{info.mdns_host}</span>
              ) : null}
            </div>
          ) : (
            <EmptyNote>{t("pairing.page.codeHidden")}</EmptyNote>
          )}
          <SectionHeader>{t("pairing.accept.cardTitle")}</SectionHeader>
          <TextSettingRow
            title={t("pairing.accept.label")}
            value={code}
            onChange={setCode}
            placeholder={t("pairing.accept.placeholder")}
            // The code is six letters and digits: a digits-only keypad cannot
            // type it.
            inputMode="text"
            autoCapitalize="characters"
            maxLength={6}
            enabled={busy === null}
          />
          <ActionButton
            label={t("pairing.accept.pair")}
            icon={KeyRound}
            variant="primary"
            onClick={accept}
            busy={busy === "accept"}
            disabled={!normalized || busy !== null}
            full
          />
          {refusal ? <Row label={t("pairing.accept.failed")} value={refusal} tone="err" mono={false} /> : null}
        </>
      ) : null}
    </>
  );
}

export function PairScreen() {
  const { t } = useTranslation();
  const isGroundStation = useProfile() === "ground_station";
  // The far end of the radio link: a ground station binds a drone, every
  // other node binds with a ground station.
  const peer = t(isGroundStation ? "pairing.subject.drone" : "pairing.subject.groundStation");
  const info = useResource<PairingInfo>(
    useCallback((s) => apiFetch<PairingInfo>("/api/pairing/info", { signal: s }), []),
    2000,
  );
  const rf = useResource<WfbPair>(
    useCallback((s) => apiFetch<WfbPair>("/api/wfb/pair", { signal: s }), []),
    1500,
  );
  const pending = useResource<PairPending>(
    useCallback((s) => apiFetch<PairPending>("/api/v1/ground-station/pair/pending", { signal: s }), []),
    2000,
    // The pair window is a ground-station route; other profiles answer 404.
    { enabled: isGroundStation },
  );
  const bind = useResource<BindSession>(
    useCallback((s) => apiFetch<BindSession>("/api/wfb/pair/local-bind", { signal: s }), []),
    1500,
  );

  const [busy, setBusy] = useState<null | "pair" | "unpair">(null);
  const armedConfirm = useArmedConfirm();

  const startBind = async () => {
    if (busy) return;
    setBusy("pair");
    try {
      // The POST holds until the rendezvous ends and answers with the
      // finished session, so its reply is the bind's result.
      const done = bindOutcome(await apiFetch<BindSession>("/api/wfb/pair/local-bind", { method: "POST", body: {} }));
      if (done?.kind === "paired") toast.ok(t("cockpit.pair.bindResult.paired", { peer }));
      else if (done?.kind === "failed") toast.err(t("cockpit.pair.bindResult.failed"), done.reason ?? undefined);
      else if (done?.kind === "aborted") toast.err(t("cockpit.pair.bindResult.aborted"));
    } catch (err) {
      toastFromError(err, t("cockpit.pair.bindFailed"));
    } finally {
      setBusy(null);
      rf.refresh();
      bind.refresh();
    }
  };

  // Stops the waiting rendezvous; the pending POST then answers with the
  // aborted session and its toast.
  const cancelBind = async () => {
    try {
      await apiFetch("/api/wfb/pair/local-bind/cancel", { method: "POST", body: {} });
    } catch (err) {
      toastFromError(err, t("cockpit.pair.bindCancelFailed"));
    }
  };

  const unpairRf = async () => {
    if (busy) return;
    setBusy("unpair");
    try {
      const res = await unpair("rf", armedConfirm.confirm, readArmed);
      if (res === "done") toast.ok(t("cockpit.pair.rfUnpaired", { peer }));
      else toast.err(t(`cockpit.radio.${res}`));
    } catch (err) {
      toastFromError(err, t("pairing.page.unpairFailed"));
    } finally {
      setBusy(null);
      rf.refresh();
    }
  };

  // Unknown until /api/wfb/pair has answered: a failed read is never "unpaired".
  const rfKnown = rf.data != null;
  const rfPaired = rf.data?.paired ?? false;
  const rfTone: Tone = !rfKnown ? "warn" : rfPaired ? "ok" : "muted";
  const windowOpen = pending.data?.open ?? false;
  const phase = bindView(bind.data);

  return (
    <ConsoleScaffold stale={rf.stale || info.stale}
      title={t("pairing.page.title")}
      actions={
        <span className="flex items-center gap-[0.5rem]">
          <Dot tone={rfTone} />
          <span className="text-[0.8rem] text-surface-foreground">
            {t("cockpit.pair.radioState", {
              state: !rfKnown ? DASH : rfPaired ? t("cockpit.pair.linked") : t("cockpit.pair.unpairedState"),
            })}
          </span>
          <StaleBadge stale={rf.stale || info.stale} />
        </span>
      }
    >
      {windowOpen ? (
        <div className="mb-[0.3rem] flex items-center justify-between rounded-md bg-primary/15 px-[0.6rem] py-[0.4rem]">
          <span className="text-[0.8rem] text-hud-primary">{t("cockpit.pair.windowOpen")}</span>
          <span className="font-mono text-[0.85rem] text-hud-primary">{fmtCountdown(pending.data?.closes_at_ms)}</span>
        </div>
      ) : null}

      <div className="flex flex-col gap-[0.15rem]">
        <CloudOwner info={info.data} failed={readFailed(info)} onChanged={info.refresh} confirmArmed={armedConfirm.confirm} />

        <SectionHeader>{t("cockpit.pair.rfLink")}</SectionHeader>
        {!rfKnown ? (
          readFailed(rf) ? (
            <ErrorNote onRetry={rf.refresh}>{t("cockpit.readFailed.wfbPair")}</ErrorNote>
          ) : (
            <EmptyNote>{t("common.loading")}</EmptyNote>
          )
        ) : rfPaired ? (
          <>
            <Row
              label={t("cockpit.pair.pairedDrone", { peer })}
              left={<Dot tone="ok" />}
              value={rf.data?.paired_with_device_id ?? t("cockpit.pair.linked")}
              tone="ok"
            />
            {rf.data?.fingerprint ? <Row label={t("cockpit.pair.fingerprint")} value={rf.data.fingerprint} /> : null}
            <Row label={t("cockpit.pair.role")} value={rf.data?.role ?? DASH} />
            <Row
              label={t("cockpit.pair.autoPair")}
              value={rf.data?.auto_pair_enabled == null ? DASH : rf.data.auto_pair_enabled ? t("cockpit.pair.on") : t("cockpit.pair.off")}
            />
            <div className="mt-[0.5rem]">
              <UnpairAction
                gate={PAIR_ACTION_GUARD.rfUnpair}
                label={t("cockpit.pair.unpairDrone", { peer })}
                guardLabel={t("cockpit.pair.holdToUnpair")}
                onFired={() => void unpairRf()}
                disabled={busy !== null}
              />
              <p className="mt-[0.3rem] px-[0.2rem] text-[0.75rem] text-muted-foreground">{t("cockpit.pair.rfUnpairNote", { peer })}</p>
            </div>
          </>
        ) : (
          <>
            <EmptyNote>{t("cockpit.pair.noDrone", { peer })}</EmptyNote>
            {phase ? (
              <Row
                label={t("cockpit.pair.bindSession")}
                value={t(phase.phaseKey)}
                hint={phase.ageS != null ? t("cockpit.pair.bindAge", { s: phase.ageS }) : undefined}
                tone="warn"
                mono={false}
              />
            ) : null}
            {phase?.error ? <Row label={t("cockpit.pair.bindError")} value={phase.error} tone="err" mono={false} /> : null}
            <div className="mt-[0.3rem]">
              <ActionButton
                label={busy === "pair" ? t("cockpit.pair.opening") : t("cockpit.pair.pairDrone", { peer })}
                icon={Link2}
                onClick={() => void startBind()}
                variant="primary"
                busy={busy === "pair"}
                full
              />
              {busy === "pair" || bindCancellable(bind.data) ? (
                <div className="mt-[0.3rem]">
                  <ActionButton label={t("cockpit.pair.bindCancel")} icon={X} onClick={() => void cancelBind()} full />
                </div>
              ) : null}
              <p className="mt-[0.3rem] px-[0.2rem] text-[0.75rem] text-muted-foreground">{t("cockpit.pair.bindNote", { peer })}</p>
            </div>
          </>
        )}

        {isGroundStation ? <PhoneReceivers /> : null}

        <SectionHeader>{t("cockpit.pair.browserAccess")}</SectionHeader>
        <p className="mb-[0.2rem] px-[0.15rem] text-[0.75rem] text-muted-foreground">{t("cockpit.pair.browserAccessNote")}</p>
        <SetUpAccess />
      </div>
      {armedConfirm.dialog}
    </ConsoleScaffold>
  );
}
