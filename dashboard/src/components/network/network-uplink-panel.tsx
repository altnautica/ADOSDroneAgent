import { ArrowDown, ArrowUp } from "lucide-react";
import { useCallback, useEffect, useState } from "react";

import { Badge } from "@/components/ui/badge";
import { Card, CardContent } from "@/components/ui/card";
import { Switch } from "@/components/ui/switch";
import { useConfig } from "@/hooks/use-config";
import {
  getGsNetwork,
  setShareUplink,
  setUplinkPriority,
  type GsNetworkView,
} from "@/lib/uplink";
import { toast, toastFromError } from "@/lib/toast";

const POLL_MS = 5_000;

const LEG_LABELS: Record<string, string> = {
  eth0: "Ethernet",
  ethernet: "Ethernet",
  wlan0_client: "Wi-Fi",
  wifi_client: "Wi-Fi",
  wwan0: "Cellular",
  modem_4g: "Cellular",
  usb0: "USB tether",
  usb: "USB tether",
  ap: "Access point",
};

function legLabel(token: string): string {
  return LEG_LABELS[token] ?? token;
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
  return (
    <li className="flex items-center justify-between gap-3 rounded-md border border-border bg-card px-3 py-2">
      <div className="flex min-w-0 items-center gap-2">
        <span className="text-sm text-foreground">{label}</span>
        {active && (
          <Badge variant="ok" className="font-normal">
            active
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

const NOT_REPORTED = "not reported";

function joinDetail(parts: Array<string | null | undefined>): string | null {
  const joined = parts.filter(Boolean).join(" · ");
  return joined.length > 0 ? joined : null;
}

function GsUplinkMatrix() {
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
      ? { state: "carrying traffic", known: true, detail: null }
      : { state: NOT_REPORTED, known: false, detail: null };

  const wifi = net?.wifi_client ?? null;
  const wifiLeg: LegState = (() => {
    if (!wifi || typeof wifi.connected !== "boolean")
      return { state: NOT_REPORTED, known: false, detail: null };
    if (!wifi.connected)
      return { state: "not connected", known: true, detail: null };
    return {
      state: "connected",
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
      return { state: NOT_REPORTED, known: false, detail: null };
    if (!modem.enabled) return { state: "disabled", known: true, detail: null };
    const raw = typeof modem.state === "string" ? modem.state : null;
    const pct = typeof modem.percent === "number" ? `${modem.percent.toFixed(0)}%` : null;
    return {
      state: raw ?? NOT_REPORTED,
      known: raw !== null,
      detail: pct,
    };
  })();

  const usbLeg: LegState =
    activeLeg === "usb"
      ? { state: "carrying traffic", known: true, detail: null }
      : { state: NOT_REPORTED, known: false, detail: null };

  const ap = net?.ap ?? null;
  const apLeg: LegState = (() => {
    if (!ap || typeof ap.enabled !== "boolean")
      return { state: NOT_REPORTED, known: false, detail: null };
    if (ap.standing_down === true)
      return {
        state: `standing down (${ap.standdown_reason ?? "unknown"})`,
        known: true,
        detail: ap.ssid ?? null,
      };
    return ap.enabled
      ? { state: "broadcasting", known: true, detail: ap.ssid ?? null }
      : { state: "off", known: true, detail: null };
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
      toast.ok("Failover order saved.");
    } catch (err) {
      toastFromError(err, "Could not save the failover order.");
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
          `Saved, but not applied to the live uplink: ${res.apply_error ?? "unknown"}`,
        );
      } else {
        toast.ok(enabled ? "Uplink sharing enabled." : "Uplink sharing disabled.");
      }
    } catch (err) {
      toastFromError(err, "Could not update uplink sharing.");
    } finally {
      setSavingShare(false);
    }
  }

  return (
    <Card>
      <CardContent className="pt-5 pb-5 space-y-5">
        <div className="space-y-1">
          <div className="text-sm font-semibold">Uplink matrix</div>
          <p className="text-xs text-muted-foreground">
            Every internet path this ground station can carry the fleet's cloud
            relay over. The active uplink is the agent's own report.
          </p>
        </div>

        {loadFailed && !net && (
          <div className="rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-[11px] text-destructive">
            Could not read the uplink matrix from this node.
          </div>
        )}

        {/* Active uplink — the agent's authoritative selection. */}
        <div className="flex items-baseline justify-between gap-3">
          <div className="min-w-0">
            <div className="text-xs text-muted-foreground">Active uplink</div>
            <p className="mt-0.5 text-[11px] text-muted-foreground/80">
              The path carrying the cloud relay right now.
            </p>
          </div>
          <div className="shrink-0 font-mono text-sm">
            {activeToken ? (
              legLabel(activeToken)
            ) : (
              <span className="text-muted-foreground/70">none</span>
            )}
          </div>
        </div>

        {/* The matrix, one row per leg. */}
        <ul className="flex flex-col gap-1">
          <LegRow label="Ethernet" leg={ethernetLeg} active={activeLeg === "ethernet"} />
          <LegRow label="Wi-Fi" leg={wifiLeg} active={activeLeg === "wifi"} />
          <LegRow label="Cellular" leg={cellularLeg} active={activeLeg === "cellular"} />
          <LegRow label="USB tether" leg={usbLeg} active={activeLeg === "usb"} />
          <LegRow label="Access point" leg={apLeg} active={activeLeg === "ap"} />
        </ul>

        {/* Failover priority ladder. */}
        <div>
          <div className="text-xs text-muted-foreground">Failover order</div>
          <p className="mb-2 mt-0.5 text-[11px] text-muted-foreground/80">
            The order the agent tries uplinks in. It falls to the next when the
            one above drops.
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
                  <span className="flex-1 text-sm">{legLabel(token)}</span>
                  <span className="font-mono text-[10px] text-muted-foreground/70">
                    {token}
                  </span>
                  <button
                    type="button"
                    onClick={() => void onMovePriority(idx, -1)}
                    disabled={savingPriority || idx === 0}
                    aria-label={`Move ${legLabel(token)} up`}
                    className="rounded border border-border p-1 text-muted-foreground hover:text-foreground focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-ring disabled:opacity-40"
                  >
                    <ArrowUp className="h-3 w-3" aria-hidden="true" />
                  </button>
                  <button
                    type="button"
                    onClick={() => void onMovePriority(idx, 1)}
                    disabled={savingPriority || idx === priority.length - 1}
                    aria-label={`Move ${legLabel(token)} down`}
                    className="rounded border border-border p-1 text-muted-foreground hover:text-foreground focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-ring disabled:opacity-40"
                  >
                    <ArrowDown className="h-3 w-3" aria-hidden="true" />
                  </button>
                </li>
              ))}
            </ol>
          ) : (
            <p className="text-[11px] text-muted-foreground/70">
              The agent has not reported a failover order yet.
            </p>
          )}
        </div>

        {/* Share uplink — rendered only once the live view reports a real value. */}
        {shareKnown && (
          <div className="flex items-start justify-between gap-4 border-t border-border pt-4">
            <div className="space-y-1 min-w-0">
              <div className="text-sm font-medium">Share uplink with AP clients</div>
              <p className="text-xs text-muted-foreground leading-relaxed">
                NAT the active uplink out to devices joined to this node's access
                point, so a phone or laptop on the AP reaches the internet through
                the ground station.
              </p>
            </div>
            <Switch
              checked={net?.share_uplink === true}
              onCheckedChange={(v) => void onShareToggle(v)}
              disabled={savingShare}
              aria-label="Share uplink with AP clients"
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
  const config = useConfig();
  const profile = config.data?.agent?.profile;

  if (!profile) return null;
  if (profile !== "ground_station") {
    return (
      <Card>
        <CardContent className="pt-5 pb-5 text-sm text-muted-foreground">
          The uplink matrix, failover order and uplink sharing run on the
          ground-station uplink manager. This node is a{" "}
          <span className="font-mono">{profile}</span>; its network settings are
          the hotspot and Wi-Fi client below.
        </CardContent>
      </Card>
    );
  }

  return <GsUplinkMatrix />;
}
