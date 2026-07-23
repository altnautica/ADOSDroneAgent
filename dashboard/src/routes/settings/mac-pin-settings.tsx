import { useQuery } from "@tanstack/react-query";
import { useEffect, useState } from "react";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Switch } from "@/components/ui/switch";
import { useConfig } from "@/hooks/use-config";
import { putConfigChecked } from "@/lib/apply-actions";
import { getMacAdapters, pinMac, unpinMac, type MacAdapter } from "@/lib/mac-pin";
import { toast, toastFromError } from "@/lib/toast";

const ADAPTERS_KEY = ["mac-adapters"] as const;

function StateBadge({ state }: { state: string }) {
  const variant =
    state === "pinned"
      ? "ok"
      : state === "candidate" || state === "deferred"
        ? "warn"
        : "default";
  return (
    <Badge variant={variant} className="font-normal">
      {state}
    </Badge>
  );
}

function StatRow({ label, value, mono = true }: { label: string; value: string; mono?: boolean }) {
  return (
    <div className="flex items-baseline justify-between gap-3">
      <span className="text-[11px] text-muted-foreground">{label}</span>
      <span className={`shrink-0 text-xs ${mono ? "font-mono" : ""}`}>{value}</span>
    </div>
  );
}

function AdapterCard({
  adapter,
  onPin,
  onUnpin,
  busyIface,
}: {
  adapter: MacAdapter;
  onPin: (iface: string) => void;
  onUnpin: (iface: string) => void;
  busyIface: string | null;
}) {
  const state = adapter.state ?? "stable";
  const iface = adapter.name ?? null;
  const busy = iface !== null && busyIface === iface;
  const title = adapter.name || adapter.vidpid || "unknown adapter";

  return (
    <Card>
      <CardContent className="pt-4 pb-4 space-y-2">
        <div className="flex items-center justify-between gap-3">
          <span className="font-mono text-sm">{title}</span>
          <StateBadge state={state} />
        </div>
        <div className="space-y-1">
          {adapter.vidpid && <StatRow label="Chipset" value={adapter.vidpid} />}
          {adapter.pinnedMac && <StatRow label="Pinned MAC" value={adapter.pinnedMac} />}
          {adapter.lastSeenMac && adapter.lastSeenMac !== adapter.pinnedMac && (
            <StatRow label="Current MAC" value={adapter.lastSeenMac} />
          )}
          {adapter.source && <StatRow label="Source" value={adapter.source} mono={false} />}
        </div>
        {adapter.deferredReason && (
          <p className="text-[11px] text-warn">Deferred: {adapter.deferredReason}</p>
        )}
        {state === "candidate" && (
          <div className="flex items-center gap-3 pt-1">
            <Button
              variant="outline"
              size="sm"
              disabled={!iface || busy}
              onClick={() => iface && onPin(iface)}
            >
              {busy ? "Pinning…" : "Pin (next boot)"}
            </Button>
            <span className="text-[11px] text-muted-foreground">
              Pins the learned MAC so the address stops moving. Applies on the
              next reboot.
            </span>
          </div>
        )}
        {state === "pinned" && (
          <div className="flex items-center gap-3 pt-1">
            <Button
              variant="outline"
              size="sm"
              disabled={!iface || busy}
              onClick={() => iface && onUnpin(iface)}
            >
              {busy ? "Removing…" : "Unpin"}
            </Button>
            <span className="text-[11px] text-muted-foreground">
              Removes the pin. Takes effect on the next reboot; the agent re-pins
              a known random-MAC adapter unless auto-pinning is off.
            </span>
          </div>
        )}
      </CardContent>
    </Card>
  );
}

/** One config toggle seeded from live config, written with read-back. */
function ConfigToggle({
  configKey,
  label,
  hint,
  value,
}: {
  configKey: string;
  label: string;
  hint: string;
  value: boolean | undefined;
}) {
  const config = useConfig();
  const [checked, setChecked] = useState(value ?? false);

  useEffect(() => {
    setChecked(value ?? false);
  }, [value]);

  async function apply(next: boolean) {
    const previous = checked;
    setChecked(next);
    try {
      await putConfigChecked(configKey, String(next));
      toast.ok("Saved.");
      config.refetch();
    } catch (err) {
      setChecked(previous);
      toastFromError(err, "Could not save the change.");
    }
  }

  return (
    <div className="flex items-start justify-between gap-4">
      <div className="space-y-1 min-w-0">
        <div className="text-sm font-medium">{label}</div>
        <p className="text-xs text-muted-foreground leading-relaxed">{hint}</p>
      </div>
      <Switch
        checked={checked}
        onCheckedChange={(v) => void apply(v)}
        aria-label={label}
      />
    </div>
  );
}

export function MacPinSettings() {
  const config = useConfig();
  const macPin = config.data?.network?.mac_pin;
  const [busyIface, setBusyIface] = useState<string | null>(null);

  const adapters = useQuery({
    queryKey: ADAPTERS_KEY,
    queryFn: () => getMacAdapters(),
    refetchInterval: 15_000,
  });

  async function onPin(iface: string) {
    setBusyIface(iface);
    try {
      const res = await pinMac(iface);
      toast.ok(res.note || "Pinned.");
      await adapters.refetch();
    } catch (err) {
      toastFromError(err, "Could not pin the adapter.");
    } finally {
      setBusyIface(null);
    }
  }

  async function onUnpin(iface: string) {
    setBusyIface(iface);
    try {
      const res = await unpinMac(iface);
      toast.ok(res.note || "Unpinned.");
      await adapters.refetch();
    } catch (err) {
      toastFromError(err, "Could not unpin the adapter.");
    } finally {
      setBusyIface(null);
    }
  }

  const list = adapters.data?.adapters;

  return (
    <div className="space-y-6">
      <Card>
        <CardContent className="pt-5 pb-5 space-y-5">
          <ConfigToggle
            configKey="network.mac_pin.enabled"
            label="Auto-pin a stable MAC"
            hint="Detect an onboard adapter with no hardware MAC (it randomizes each boot, churning this node's IP) and pin a stable address. The pin is written for the next boot and never touches the live link."
            value={macPin?.enabled}
          />
          <div className="border-t border-border pt-5">
            <ConfigToggle
              configKey="network.mac_pin.apply_live_allowed"
              label="Allow live re-tag"
              hint="Let the agent re-tag the live interface to fix the IP this session without a reboot. Off by default because re-tagging drops any connection over that interface."
              value={macPin?.apply_live_allowed}
            />
          </div>
        </CardContent>
      </Card>

      <div className="space-y-2">
        <div className="text-xs font-medium uppercase tracking-wider text-muted-foreground">
          Tracked adapters
        </div>
        {adapters.isError ? (
          <div className="rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-[11px] text-destructive">
            Could not read adapter stability from this node.
          </div>
        ) : adapters.isLoading ? (
          <p className="text-[11px] text-muted-foreground/70">Reading adapters…</p>
        ) : !list || list.length === 0 ? (
          <p className="text-[11px] text-muted-foreground/70">
            No adapters need pinning on this node.
          </p>
        ) : (
          <div className="space-y-2">
            {list.map((a, idx) => (
              <AdapterCard
                key={a.usbPath || a.name || a.vidpid || String(idx)}
                adapter={a}
                onPin={onPin}
                onUnpin={onUnpin}
                busyIface={busyIface}
              />
            ))}
            {list.some((a) => a.pinnedMac) && (
              <p className="text-[11px] text-muted-foreground">
                A pinned MAC keeps this node's DHCP lease and IP stable across
                reboots.
              </p>
            )}
          </div>
        )}
      </div>
    </div>
  );
}
