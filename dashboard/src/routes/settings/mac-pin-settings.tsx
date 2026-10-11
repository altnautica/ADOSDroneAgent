import { useQuery } from "@tanstack/react-query";
import { useState } from "react";
import { useTranslation } from "react-i18next";

import { ConfigToggle } from "@/components/settings/config-fields";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { useConfig } from "@/hooks/use-config";
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
  const { t } = useTranslation();
  const state = adapter.state ?? "stable";
  const iface = adapter.name ?? null;
  const busy = iface !== null && busyIface === iface;
  const title = adapter.name || adapter.vidpid || t("settingsMore.macPin.unknownAdapter");

  return (
    <Card>
      <CardContent className="pt-4 pb-4 space-y-2">
        <div className="flex items-center justify-between gap-3">
          <span className="font-mono text-sm">{title}</span>
          <StateBadge state={state} />
        </div>
        <div className="space-y-1">
          {adapter.vidpid && <StatRow label={t("settingsMore.macPin.chipset")} value={adapter.vidpid} />}
          {adapter.pinnedMac && <StatRow label={t("settingsMore.macPin.pinnedMac")} value={adapter.pinnedMac} />}
          {adapter.lastSeenMac && adapter.lastSeenMac !== adapter.pinnedMac && (
            <StatRow label={t("settingsMore.macPin.currentMac")} value={adapter.lastSeenMac} />
          )}
          {adapter.source && <StatRow label={t("settingsMore.macPin.source")} value={adapter.source} mono={false} />}
        </div>
        {adapter.deferredReason && (
          <p className="text-[11px] text-warn">{t("settingsMore.macPin.deferred", { reason: adapter.deferredReason })}</p>
        )}
        {state === "candidate" && (
          <div className="flex items-center gap-3 pt-1">
            <Button
              variant="outline"
              size="sm"
              disabled={!iface || busy}
              onClick={() => iface && onPin(iface)}
            >
              {busy ? t("settingsMore.macPin.pinning") : t("settingsMore.macPin.pinNextBoot")}
            </Button>
            <span className="text-[11px] text-muted-foreground">
              {t("settingsMore.macPin.pinHint")}
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
              {busy ? t("settingsMore.macPin.removing") : t("settingsMore.macPin.unpin")}
            </Button>
            <span className="text-[11px] text-muted-foreground">
              {t("settingsMore.macPin.unpinHint")}
            </span>
          </div>
        )}
      </CardContent>
    </Card>
  );
}

export function MacPinSettings() {
  const { t } = useTranslation();
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
      toast.ok(res.note || t("settingsMore.macPin.pinned"));
      await adapters.refetch();
    } catch (err) {
      toastFromError(err, t("settingsMore.macPin.pinError"));
    } finally {
      setBusyIface(null);
    }
  }

  async function onUnpin(iface: string) {
    setBusyIface(iface);
    try {
      const res = await unpinMac(iface);
      // The unpin answers 200 even when half of it failed: a config the agent
      // could not write, or a next-boot .link it could not delete (networkd
      // would re-apply the old MAC at the next boot).
      const failure = res.link_error ?? res.persist_error;
      if (failure) {
        toast.err(t("settingsMore.macPin.stillPinned"), failure);
      } else {
        toast.ok(res.note || t("settingsMore.macPin.unpinned"));
      }
      await adapters.refetch();
    } catch (err) {
      toastFromError(err, t("settingsMore.macPin.unpinError"));
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
            label={t("settingsMore.macPin.autoPinLabel")}
            hint={t("settingsMore.macPin.autoPinHint")}
            value={macPin?.enabled}
          />
          <div className="border-t border-border pt-5">
            <ConfigToggle
              configKey="network.mac_pin.apply_live_allowed"
              label={t("settingsMore.macPin.liveRetagLabel")}
              hint={t("settingsMore.macPin.liveRetagHint")}
              value={macPin?.apply_live_allowed}
            />
          </div>
        </CardContent>
      </Card>

      <div className="space-y-2">
        <div className="text-xs font-medium uppercase tracking-wider text-muted-foreground">
          {t("settingsMore.macPin.trackedAdapters")}
        </div>
        {adapters.isError ? (
          <div className="rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-[11px] text-destructive">
            {t("settingsMore.macPin.readError")}
          </div>
        ) : adapters.isLoading ? (
          <p className="text-[11px] text-muted-foreground/70">{t("settingsMore.macPin.reading")}</p>
        ) : !list || list.length === 0 ? (
          <p className="text-[11px] text-muted-foreground/70">
            {t("settingsMore.macPin.empty")}
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
                {t("settingsMore.macPin.pinnedFooter")}
              </p>
            )}
          </div>
        )}
      </div>
    </div>
  );
}
