import type { TFunction } from "i18next";
import { ArrowDown, ArrowUp } from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import { Trans, useTranslation } from "react-i18next";

import { Badge } from "@/components/ui/badge";
import { Card, CardContent } from "@/components/ui/card";
import { Switch } from "@/components/ui/switch";
import { useNodeIdentity } from "@/hooks/use-node-identity";
import {
  getGsNetwork,
  setShareUplink,
  setUplinkPriority,
  type GsNetworkView,
} from "@/lib/uplink";
import { toast, toastFromError } from "@/lib/toast";

const POLL_MS = 5_000;

const LEG_LABEL_KEYS = {
  eth0: "uplink.leg.ethernet",
  ethernet: "uplink.leg.ethernet",
  wlan0_client: "uplink.leg.wifi",
  wifi_client: "uplink.leg.wifi",
  wwan0: "uplink.leg.cellular",
  modem_4g: "uplink.leg.cellular",
  usb0: "uplink.leg.usb",
  usb: "uplink.leg.usb",
  ap: "uplink.leg.ap",
} as const;

function legLabel(token: string, t: TFunction): string {
  const key = (LEG_LABEL_KEYS as Record<string, (typeof LEG_LABEL_KEYS)[keyof typeof LEG_LABEL_KEYS]>)[token];
  return key ? t(key) : token;
}

/** Normalise an uplink token to one of the five matrix rows, or null when it is
 * a token we do not render a dedicated row for. */
function legKey(token: string): "ethernet" | "wifi" | "cellular" | "usb" | "ap" | null {
  switch (token) {
    case "eth0":
    case "ethernet":
      return "ethernet";
    case "wlan0_client":
    case "wifi_client":
      return "wifi";
    case "wwan0":
    case "modem_4g":
      return "cellular";
    case "usb0":
    case "usb":
      return "usb";
    case "ap":
      return "ap";
    default:
      return null;
  }
}

/** Move `list[index]` by `delta`; null when out of range (nothing to write). */
function moveEntry(list: readonly string[], index: number, delta: -1 | 1): string[] | null {
  const target = index + delta;
  if (index < 0 || index >= list.length) return null;
  if (target < 0 || target >= list.length) return null;
  const next = [...list];
  const [moved] = next.splice(index, 1);
  next.splice(target, 0, moved);
  return next;
}

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
}: {
  label: string;
  leg: LegState;
  active: boolean;
}) {
  const { t } = useTranslation();
  return (
    <li className="flex items-center justify-between gap-3 rounded-md border border-border bg-card px-3 py-2">
      <div className="flex min-w-0 items-center gap-2">
        <span className="text-sm text-foreground">{label}</span>
        {active && (
          <Badge variant="ok" className="font-normal">
            {t("uplink.state.active")}
          </Badge>
        )}
      </div>
      <div className="flex shrink-0 items-baseline gap-2">
        {leg.detail && (
          <span className="font-mono text-[11px] text-muted-foreground">
            {leg.detail}
          </span>
        )}
        <span
          className={
            leg.known
              ? "text-xs text-muted-foreground"
              : "text-xs text-muted-foreground/70"
          }
        >
          {leg.state}
        </span>
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
    const pct = typeof modem.percent === "number" ? `${modem.percent.toFixed(0)}%` : null;
    return {
      state: raw ?? notReported,
      known: raw !== null,
      detail: pct,
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

  async function onMovePriority(index: number, delta: -1 | 1) {
    if (savingPriority || !priority) return;
    const next = moveEntry(priority, index, delta);
    if (!next) return;
    setSavingPriority(true);
    try {
      const res = await setUplinkPriority(next);
      setNet((n) => (n ? { ...n, priority: res.priority } : n));
      toast.ok(t("uplink.toast.orderSaved"));
    } catch (err) {
      toastFromError(err, t("uplink.toast.orderFailed"));
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
    } finally {
      setSavingShare(false);
    }
  }

  return (
    <Card>
      <CardContent className="pt-5 pb-5 space-y-5">
        <div className="space-y-1">
          <div className="text-sm font-semibold">{t("uplink.matrix.title")}</div>
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
              <span className="text-muted-foreground/70">{t("uplink.active.none")}</span>
            )}
          </div>
        </div>

        {/* The matrix, one row per leg. */}
        <ul className="flex flex-col gap-1">
          <LegRow label={t("uplink.leg.ethernet")} leg={ethernetLeg} active={activeLeg === "ethernet"} />
          <LegRow label={t("uplink.leg.wifi")} leg={wifiLeg} active={activeLeg === "wifi"} />
          <LegRow label={t("uplink.leg.cellular")} leg={cellularLeg} active={activeLeg === "cellular"} />
          <LegRow label={t("uplink.leg.usb")} leg={usbLeg} active={activeLeg === "usb"} />
          <LegRow label={t("uplink.leg.ap")} leg={apLeg} active={activeLeg === "ap"} />
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
                  <span className="font-mono text-[10px] text-muted-foreground/70">
                    {token}
                  </span>
                  <button
                    type="button"
                    onClick={() => void onMovePriority(idx, -1)}
                    disabled={savingPriority || idx === 0}
                    aria-label={t("uplink.failover.moveUp", { name: legLabel(token, t) })}
                    className="rounded border border-border p-1 text-muted-foreground hover:text-foreground focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-ring disabled:opacity-40"
                  >
                    <ArrowUp className="h-3 w-3" aria-hidden="true" />
                  </button>
                  <button
                    type="button"
                    onClick={() => void onMovePriority(idx, 1)}
                    disabled={savingPriority || idx === priority.length - 1}
                    aria-label={t("uplink.failover.moveDown", { name: legLabel(token, t) })}
                    className="rounded border border-border p-1 text-muted-foreground hover:text-foreground focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-ring disabled:opacity-40"
                  >
                    <ArrowDown className="h-3 w-3" aria-hidden="true" />
                  </button>
                </li>
              ))}
            </ol>
          ) : (
            <p className="text-[11px] text-muted-foreground/70">
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
              onCheckedChange={(v) => void onShareToggle(v)}
              disabled={savingShare}
              aria-label={t("uplink.share.title")}
            />
          </div>
        )}
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
