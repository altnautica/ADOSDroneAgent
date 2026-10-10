// Pair — local-first pairing over the LAN/RF. Three facets, all honest reads:
//   1. This ground station: whether a GCS has claimed the box, and the pairing
//      code + mDNS reach a GCS uses to claim it (from `GET /api/pairing/info`).
//   2. The drone RF link: the WFB pair state (peer device, key fingerprint,
//      auto-pair) from `GET /api/wfb/pair`, plus a Pair-drone action that opens a
//      local bind window (`POST /api/wfb/pair/local-bind`) and an Unpair action
//      (`POST /api/wfb/pair/unpair`, two-tap confirmed — it wipes the key and
//      disables auto-bind). The open mesh pair window (`GET .../pair/pending`)
//      surfaces as a countdown strip while a window is open.
//   3. Phone receivers: phones with their own radio waiting for the fleet
//      receive keys (`GET /api/v1/ground-station/wfb/invite`), approved or
//      rejected after the operator compares the fingerprint with the phone's.

import { useCallback, useState } from "react";
import { Check, Link2, Smartphone, Unlink, X } from "lucide-react";

import { Panel, PanelHeader } from "@/components/ui/panel";
import {
  ActionButton,
  ConfirmButton,
  Dot,
  EmptyNote,
  Row,
  SectionHeader,
  StaleBadge,
  type Tone,
} from "@/components/ui/data";
import { SetUpAccess } from "@/components/shell/set-up-access";
import { useProfile } from "@/shared/use-profile";
import { useResource } from "@/shared/use-resource";
import { apiFetch } from "@/shared/api-fetch";
import { DASH } from "@/shared/format";
import { fmtCountdown } from "@/lib/format-status";

interface PairingInfo {
  device_id?: string | null;
  name?: string | null;
  paired?: boolean;
  owner_id?: string | null;
  paired_at?: string | null;
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
interface BindSession {
  state?: string;
  phase?: string;
  status?: string;
  active?: boolean;
  [k: string]: unknown;
}

function bindPhase(b: BindSession | null): string | null {
  if (!b) return null;
  const p = b.phase ?? b.state ?? b.status;
  return typeof p === "string" && p ? p : null;
}

interface PhoneInvite {
  invite_id: string;
  label: string;
  phone_fingerprint: string;
  expires_at_ms: number;
}

function PhoneReceivers() {
  const invites = useResource<{ pending: PhoneInvite[] }>(
    useCallback((s) => apiFetch<{ pending: PhoneInvite[] }>("/api/v1/ground-station/wfb/invite", { signal: s }), []),
    2000,
  );
  const [busyId, setBusyId] = useState<string | null>(null);
  const [failed, setFailed] = useState(false);

  const decide = async (id: string, decision: "approve" | "reject") => {
    if (busyId) return;
    setBusyId(id);
    setFailed(false);
    try {
      await apiFetch(`/api/v1/ground-station/wfb/invite/${encodeURIComponent(id)}/${decision}`, {
        method: "POST",
        body: {},
      });
    } catch {
      setFailed(true);
    } finally {
      setBusyId(null);
      invites.refresh();
    }
  };

  const pending = invites.data?.pending ?? [];
  return (
    <>
      <SectionHeader>Phone receivers</SectionHeader>
      {pending.length === 0 ? (
        <EmptyNote>No phone is waiting for approval.</EmptyNote>
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
                label="Reject"
                icon={X}
                onClick={() => void decide(p.invite_id, "reject")}
                disabled={busyId !== null}
                full
              />
              <ActionButton
                label="Approve"
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
      {failed ? <Row label="Last decision" value="refused (invite expired?)" tone="warn" mono={false} /> : null}
      <p className="mt-[0.3rem] px-[0.2rem] text-[0.75rem] text-muted-foreground">
        Approve only when this fingerprint matches the one on the phone. A phone receives video and
        telemetry; it never transmits.
      </p>
    </>
  );
}

export function PairScreen() {
  const isGroundStation = useProfile() === "ground_station";
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
  );
  const bind = useResource<BindSession>(
    useCallback((s) => apiFetch<BindSession>("/api/wfb/pair/local-bind", { signal: s }), []),
    1500,
  );

  const [busy, setBusy] = useState<null | "pair" | "unpair">(null);

  const startBind = async () => {
    if (busy) return;
    setBusy("pair");
    try {
      await apiFetch("/api/wfb/pair/local-bind", { method: "POST", body: {} });
    } catch {
      // the next poll reflects the agent's real bind state; nothing is faked.
    } finally {
      setBusy(null);
      rf.refresh();
      bind.refresh();
    }
  };

  const unpair = async () => {
    if (busy) return;
    setBusy("unpair");
    try {
      await apiFetch("/api/wfb/pair/unpair", { method: "POST", body: {} });
    } catch {
      // the next poll keeps the paired state honest.
    } finally {
      setBusy(null);
      rf.refresh();
    }
  };

  const rfPaired = rf.data?.paired ?? false;
  const rfTone: Tone = rfPaired ? "ok" : "muted";
  const gcsClaimed = info.data?.paired ?? false;
  const windowOpen = pending.data?.open ?? false;
  const phase = bindPhase(bind.data);

  return (
    <Panel>
      <PanelHeader
        title="Pair"
        right={
          <div className="flex items-center gap-[0.5rem]">
            <Dot tone={rfTone} />
            <span className="text-[0.8rem] text-surface-foreground">{rfPaired ? "linked" : "unpaired"}</span>
            <StaleBadge stale={rf.stale} />
          </div>
        }
      />

      {windowOpen ? (
        <div className="mb-[0.3rem] flex items-center justify-between rounded-md bg-primary/15 px-[0.6rem] py-[0.4rem]">
          <span className="text-[0.8rem] text-hud-primary">Pair window open</span>
          <span className="font-mono text-[0.85rem] text-hud-primary">{fmtCountdown(pending.data?.closes_at_ms)}</span>
        </div>
      ) : null}

      <div className="flex flex-col gap-[0.15rem]">
        <SectionHeader>Drone RF link</SectionHeader>
        {rfPaired ? (
          <>
            <Row
              label="Paired drone"
              left={<Dot tone="ok" />}
              value={rf.data?.paired_with_device_id ?? "linked"}
              tone="ok"
            />
            {rf.data?.fingerprint ? <Row label="Key fingerprint" value={rf.data.fingerprint} /> : null}
            <Row label="Role" value={rf.data?.role ?? DASH} />
            <Row
              label="Auto-pair"
              value={rf.data?.auto_pair_enabled == null ? DASH : rf.data.auto_pair_enabled ? "on" : "off"}
            />
            <div className="mt-[0.5rem]">
              <ConfirmButton
                label="Unpair drone"
                confirmLabel="Tap again to unpair"
                icon={Unlink}
                onConfirm={unpair}
                busy={busy === "unpair"}
                full
              />
              <p className="mt-[0.3rem] px-[0.2rem] text-[0.75rem] text-muted-foreground">
                Wipes the RF key and disables auto-bind. The drone must be re-paired to reconnect.
              </p>
            </div>
          </>
        ) : (
          <>
            <EmptyNote>No drone is paired to this ground station over the radio link.</EmptyNote>
            {phase ? <Row label="Bind session" value={phase} tone="warn" /> : null}
            <div className="mt-[0.3rem]">
              <ActionButton
                label={busy === "pair" ? "Opening bind window…" : "Pair a drone"}
                icon={Link2}
                onClick={startBind}
                variant="primary"
                busy={busy === "pair"}
                full
              />
              <p className="mt-[0.3rem] px-[0.2rem] text-[0.75rem] text-muted-foreground">
                Opens a local bind window over the radio. Power the drone nearby; auto-bind completes the pair.
              </p>
            </div>
          </>
        )}

        {isGroundStation ? <PhoneReceivers /> : null}

        <SectionHeader>This ground station</SectionHeader>
        <Row
          label="Claimed by a GCS"
          left={<Dot tone={gcsClaimed ? "ok" : "muted"} />}
          value={gcsClaimed ? "yes" : "no"}
          tone={gcsClaimed ? "ok" : "muted"}
        />
        {gcsClaimed && info.data?.owner_id ? <Row label="Owner" value={info.data.owner_id} /> : null}
        {info.data?.name ? <Row label="Name" value={info.data.name} mono={false} /> : null}
        {info.data?.mdns_host ? <Row label="Reach" value={info.data.mdns_host} /> : null}

        {!gcsClaimed && info.data?.pairing_code ? (
          <div className="mt-[0.3rem] flex flex-col items-center rounded-md bg-input/40 px-[0.6rem] py-[0.6rem]">
            <span className="text-[0.75rem] uppercase tracking-wide text-muted-foreground">
              Pairing code — enter in Mission Control
            </span>
            <span className="select-text font-mono text-[1.7rem] font-semibold tracking-[0.2em] text-hud-primary">
              {info.data.pairing_code}
            </span>
            {info.data?.mdns_host ? (
              <span className="select-text text-[0.75rem] text-muted-foreground">{info.data.mdns_host}</span>
            ) : null}
          </div>
        ) : null}

        <SectionHeader>Access from a browser</SectionHeader>
        <p className="mb-[0.2rem] px-[0.15rem] text-[0.75rem] text-muted-foreground">
          Unlock this node&apos;s data (status, video, command) from a browser on the
          trusted LAN by setting or entering its dashboard PIN.
        </p>
        <SetUpAccess />
      </div>
    </Panel>
  );
}
