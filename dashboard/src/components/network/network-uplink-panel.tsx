import { ArrowDown, ArrowUp } from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import { Trans, useTranslation } from "react-i18next";

import { ConfirmDialog } from "@/components/settings/confirm-dialog";
import { Badge } from "@/components/ui/badge";
import { Card, CardContent } from "@/components/ui/card";
import { Switch } from "@/components/ui/switch";
import { COARSE_HIT_AREA } from "@/lib/hit-area";
import { cellularCapDetail, legKey, legLabel, moveEntry } from "@/lib/uplink-view";
import { cn } from "@/lib/utils";
import { useNodeIdentity } from "@/hooks/use-node-identity";
import {
  getGsNetwork,
  setShareUplink,
  setUplinkPriority,
  type GsNetworkView,
} from "@/shared/api/uplink";
import { toast, toastFromError } from "@/shared/api/toast";

const POLL_MS = 5_000;

// The order arrows grow to a 44 px square on a phone.
const ARROW_CLASS =
  "inline-flex items-center justify-center rounded border border-border p-1 text-muted-foreground hover:text-foreground focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-ring disabled:opacity-40 max-md:h-11 max-md:w-11";

interface LegState {
  state: string;
  /** false renders the state in the muted "not reported" tone. */
  known: boolean;
  detail: string | null;
}

function LegRow({
  label,
  leg,
  active,
  stale,
}: {
  label: string;
  leg: LegState;
  active: boolean;
  /** The last read failed: the values are the last known ones, dimmed. */
  stale: boolean;
}) {
  const { t } = useTranslation();
  return (
    <li className="flex items-center justify-between gap-3 rounded-md border border-border bg-card px-3 py-2">
      <div className="flex min-w-0 items-center gap-2">
        <span className="text-sm text-foreground">{label}</span>
        {active && !stale && (
          <Badge variant="success">
            {t("uplink.state.active")}
          </Badge>
        )}
      </div>
      <div className={cn("flex shrink-0 items-baseline gap-2", stale && "opacity-50")}>
        {leg.detail && (
          <span className="font-mono text-[11px] text-muted-foreground">
            {leg.detail}
          </span>
        )}
        <span className="text-xs text-muted-foreground">{leg.state}</span>
      </div>
    </li>
  );
}

function joinDetail(parts: Array<string | null | undefined>): string | null {
  const joined = parts.filter(Boolean).join(" · ");
  return joined.length > 0 ? joined : null;
}

function GsUplinkMatrix() {
  const { t } = useTranslation();
  const [net, setNet] = useState<GsNetworkView | null>(null);
  const [loadFailed, setLoadFailed] = useState(false);
  const [savingPriority, setSavingPriority] = useState(false);
  const [savingShare, setSavingShare] = useState(false);
  // A write that can move or cut this page's own link waits for a confirm.
  const [pending, setPending] = useState<
    | { kind: "order"; next: string[]; name: string; position: number }
    | { kind: "share"; enabled: boolean }
    | null
  >(null);

  const refresh = useCallback(async () => {
    try {
      setNet(await getGsNetwork());
      setLoadFailed(false);
    } catch {
      setLoadFailed(true);
    }
  }, []);

  useEffect(() => {
    let cancelled = false;
    const tick = () => {
      if (!cancelled) void refresh();
    };
    tick();
    const timer = setInterval(tick, POLL_MS);
    return () => {
      cancelled = true;
      clearInterval(timer);
    };
  }, [refresh]);

  const notReported = t("uplink.state.notReported");
  const carrying = t("uplink.state.carryingTraffic");

  const activeToken =
    typeof net?.active_uplink === "string" && net.active_uplink.length > 0
      ? net.active_uplink
      : null;
  const activeLeg = activeToken ? legKey(activeToken) : null;

  // Ethernet: the agent's aggregate view carries a placeholder link leg on the
  // current front (no live ethernet seam), so a hard "no link" would be a false
  // negative. Lead with the authoritative active-uplink report; otherwise it is
  // genuinely not reported.
  const ethernetLeg: LegState =
    activeLeg === "ethernet"
      ? { state: carrying, known: true, detail: null }
      : { state: notReported, known: false, detail: null };

  const wifi = net?.wifi_client ?? null;
  const wifiLeg: LegState = (() => {
    if (!wifi || typeof wifi.connected !== "boolean")
      return { state: notReported, known: false, detail: null };
    if (!wifi.connected)
      return { state: t("uplink.state.notConnected"), known: true, detail: null };
    return {
      state: t("uplink.state.connected"),
      known: true,
      detail: joinDetail([
        wifi.ssid,
        typeof wifi.signal === "number" ? `${wifi.signal}%` : null,
        wifi.ip,
      ]),
    };
  })();

  const modem = net?.modem_4g ?? null;
  const cellularLeg: LegState = (() => {
    if (!modem || typeof modem.enabled !== "boolean")
      return { state: notReported, known: false, detail: null };
    if (!modem.enabled)
      return { state: t("uplink.state.disabled"), known: true, detail: null };
    const raw = typeof modem.state === "string" ? modem.state : null;
    return {
      state: raw ?? notReported,
      known: raw !== null,
      detail: cellularCapDetail(modem.percent, t),
    };
  })();

  const usbLeg: LegState =
    activeLeg === "usb"
      ? { state: carrying, known: true, detail: null }
      : { state: notReported, known: false, detail: null };

  const ap = net?.ap ?? null;
  const apLeg: LegState = (() => {
    if (!ap || typeof ap.enabled !== "boolean")
      return { state: notReported, known: false, detail: null };
    if (ap.standing_down === true)
      return {
        state: t("uplink.state.standingDown", {
          reason: ap.standdown_reason ?? t("uplink.state.unknown"),
        }),
        known: true,
        detail: ap.ssid ?? null,
      };
    return ap.enabled
      ? { state: t("uplink.state.broadcasting"), known: true, detail: ap.ssid ?? null }
      : { state: t("uplink.state.off"), known: true, detail: null };
  })();

  const priority = net?.priority ?? null;
  const shareKnown = net != null && typeof net.share_uplink === "boolean";
  // Values from before the last failed read stay on screen, marked stale.
  const stale = loadFailed && net != null;

  function askMove(index: number, delta: -1 | 1) {
    if (savingPriority || !priority) return;
    const next = moveEntry(priority, index, delta);
    if (!next) return;
    setPending({ kind: "order", next, name: legLabel(priority[index], t), position: index + delta + 1 });
  }

  async function onSavePriority(next: string[]) {
    setSavingPriority(true);
    try {
      const res = await setUplinkPriority(next);
      setNet((n) => (n ? { ...n, priority: res.priority } : n));
      toast.ok(t("uplink.toast.orderSaved"));
    } catch (err) {
      toastFromError(err, t("uplink.toast.orderFailed"));
      throw err;
    } finally {
      setSavingPriority(false);
    }
  }

  async function onShareToggle(enabled: boolean) {
    if (savingShare) return;
    setSavingShare(true);
    try {
      const res = await setShareUplink(enabled);
      setNet((n) => (n ? { ...n, share_uplink: res.enabled } : n));
      if (res.applied === false) {
        toast.err(
          t("uplink.toast.notApplied", {
            error: res.apply_error ?? t("uplink.state.unknown"),
          }),
        );
      } else {
        toast.ok(
          enabled ? t("uplink.toast.shareEnabled") : t("uplink.toast.shareDisabled"),
        );
      }
    } catch (err) {
      toastFromError(err, t("uplink.toast.shareFailed"));
      throw err;
    } finally {
      setSavingShare(false);
    }
  }

  return (
    <Card>
      <CardContent className="pt-5 pb-5 space-y-5">
        <div className="space-y-1">
          <div className="flex items-center gap-2 text-sm font-semibold">
            {t("uplink.matrix.title")}
            {stale && <Badge variant="warning">{t("cockpit.stale")}</Badge>}
          </div>
          <p className="text-xs text-muted-foreground">
            {t("uplink.matrix.description")}
          </p>
        </div>

        {loadFailed && !net && (
          <div className="rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-[11px] text-destructive">
            {t("uplink.matrix.loadFailed")}
          </div>
        )}

        {/* Active uplink — the agent's authoritative selection. */}
        <div className="flex items-baseline justify-between gap-3">
          <div className="min-w-0">
            <div className="text-xs text-muted-foreground">{t("uplink.active.title")}</div>
            <p className="mt-0.5 text-[11px] text-muted-foreground/80">
              {t("uplink.active.description")}
            </p>
          </div>
          <div className="shrink-0 font-mono text-sm">
            {activeToken ? (
              legLabel(activeToken, t)
            ) : (
              <span className="text-muted-foreground">{t("uplink.active.none")}</span>
            )}
          </div>
        </div>

        {/* The matrix, one row per leg. */}
        <ul className="flex flex-col gap-1">
          <LegRow label={t("uplink.leg.ethernet")} leg={ethernetLeg} active={activeLeg === "ethernet"} stale={stale} />
          <LegRow label={t("uplink.leg.wifi")} leg={wifiLeg} active={activeLeg === "wifi"} stale={stale} />
          <LegRow label={t("uplink.leg.cellular")} leg={cellularLeg} active={activeLeg === "cellular"} stale={stale} />
          <LegRow label={t("uplink.leg.usb")} leg={usbLeg} active={activeLeg === "usb"} stale={stale} />
          <LegRow label={t("uplink.leg.ap")} leg={apLeg} active={activeLeg === "ap"} stale={stale} />
        </ul>

        {/* Failover priority ladder. */}
        <div>
          <div className="text-xs text-muted-foreground">{t("uplink.failover.title")}</div>
          <p className="mb-2 mt-0.5 text-[11px] text-muted-foreground/80">
            {t("uplink.failover.description")}
          </p>
          {priority && priority.length > 0 ? (
            <ol className="flex flex-col gap-1">
              {priority.map((token, idx) => (
                <li
                  key={token}
                  className="flex items-center gap-2 rounded-md border border-border bg-card px-3 py-1.5"
                >
                  <span className="w-4 shrink-0 text-right font-mono text-[11px] text-muted-foreground/80">
                    {idx + 1}
                  </span>
                  <span className="flex-1 text-sm">{legLabel(token, t)}</span>
                  <span className="font-mono text-[10px] text-muted-foreground">
                    {token}
                  </span>
                  <button
                    type="button"
                    onClick={() => askMove(idx, -1)}
                    disabled={savingPriority || idx === 0}
                    aria-label={t("uplink.failover.moveUp", { name: legLabel(token, t) })}
                    className={cn(ARROW_CLASS, COARSE_HIT_AREA)}
                  >
                    <ArrowUp className="h-3 w-3" aria-hidden="true" />
                  </button>
                  <button
                    type="button"
                    onClick={() => askMove(idx, 1)}
                    disabled={savingPriority || idx === priority.length - 1}
                    aria-label={t("uplink.failover.moveDown", { name: legLabel(token, t) })}
                    className={cn(ARROW_CLASS, COARSE_HIT_AREA)}
                  >
                    <ArrowDown className="h-3 w-3" aria-hidden="true" />
                  </button>
                </li>
              ))}
            </ol>
          ) : (
            <p className="text-[11px] text-muted-foreground">
              {t("uplink.failover.empty")}
            </p>
          )}
        </div>

        {/* Share uplink — rendered only once the live view reports a real value. */}
        {shareKnown && (
          <div className="flex items-start justify-between gap-4 border-t border-border pt-4">
            <div className="space-y-1 min-w-0">
              <div className="text-sm font-medium">{t("uplink.share.title")}</div>
              <p className="text-xs text-muted-foreground leading-relaxed">
                {t("uplink.share.description")}
              </p>
            </div>
            <Switch
              checked={net?.share_uplink === true}
              onCheckedChange={(v) => setPending({ kind: "share", enabled: v })}
              disabled={savingShare}
              aria-label={t("uplink.share.title")}
            />
          </div>
        )}
        <ConfirmDialog
          open={pending !== null}
          onOpenChange={(open) => {
            if (!open) setPending(null);
          }}
          title={
            pending?.kind === "order"
              ? t("uplink.failover.confirmTitle")
              : pending?.enabled
                ? t("uplink.share.confirmOnTitle")
                : t("uplink.share.confirmOffTitle")
          }
          description={
            pending?.kind === "order"
              ? t("uplink.failover.confirmBody", { name: pending.name, position: pending.position })
              : pending?.enabled
                ? t("uplink.share.confirmOnBody")
                : t("uplink.share.confirmOffBody")
          }
          destructive={pending?.kind === "share" && !pending.enabled}
          onConfirm={async () => {
            if (pending?.kind === "order") await onSavePriority(pending.next);
            else if (pending?.kind === "share") await onShareToggle(pending.enabled);
          }}
        />
      </CardContent>
    </Card>
  );
}

/** The uplink matrix, failover ladder and uplink-sharing toggle. These are the
 * ground-station uplink daemon's surface; other profiles have no such daemon,
 * so the panel says so honestly instead of showing an empty matrix. */
export function NetworkUplinkPanel() {
  // Gate on the resolved profile from the pairing-info probe, the same source
  // the sidebar and route gates use (never the raw config value, whose "auto"
  // default misreads an uncommitted ground station as a non-GS node).
  const { profile } = useNodeIdentity();

  if (profile === "auto") return null;
  if (profile !== "ground_station") {
    return (
      <Card>
        <CardContent className="pt-5 pb-5 text-sm text-muted-foreground">
          <Trans
            i18nKey="uplink.notGroundStation"
            values={{ profile }}
            components={{ code: <span className="font-mono" /> }}
          />
        </CardContent>
      </Card>
    );
  }

  return <GsUplinkMatrix />;
}
