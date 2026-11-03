// Settings → MAC pin: the auto-pin and live re-tag config leaves, and the
// tracked adapters (polled every 15 s) with their pin / unpin actions. Pin,
// unpin and allowing a live re-tag (which drops connections over that
// interface) are hold-guarded; auto-pin is a plain switch.

import { useState } from "react";
import { useTranslation } from "react-i18next";
import { Pin, PinOff, ShieldAlert } from "lucide-react";

import { Toggle } from "@/components/settings/widgets";
import { EmptyNote, Row } from "@/components/ui/data";
import { GuardedButton } from "@/components/ui/guarded-button";
import { SettingRow } from "@/components/ui/setting-row";
import {
  MAC_ADAPTERS_POLL_MS,
  adapterAction,
  adapterTone,
  runMacPinAction,
  type MacPinAction,
} from "@/lib/settings/mac-pin";
import { toneClass } from "@/lib/tone";
import { getMacAdapters, pinMac, unpinMac, type MacAdapter } from "@/shared/api/mac-pin";
import { toast, toastFromError } from "@/shared/api/toast";
import { useResource } from "@/shared/use-resource";

import { SectionGroup } from "./section-kit";
import { useAgentConfig, useLeafWriter } from "./section-hooks";

export function MacPinSection() {
  const { t } = useTranslation();
  const { get } = useAgentConfig();
  const { save, busy: saving } = useLeafWriter();
  const adapters = useResource(() => getMacAdapters(), MAC_ADAPTERS_POLL_MS);
  const [busyIface, setBusyIface] = useState<string | null>(null);

  const autoPin = get<boolean>("network.mac_pin.enabled", false);
  const liveAllowed = get<boolean>("network.mac_pin.apply_live_allowed", false);

  const fire = async (action: MacPinAction) => {
    setBusyIface(action.iface);
    try {
      const out = await runMacPinAction(action, { pinMac, unpinMac });
      if (out.ok) toast.ok(out.note ?? t(out.key));
      else toast.err(t(out.key), out.detail);
    } catch (err) {
      toastFromError(
        err,
        t(action.kind === "pin" ? "settingsMore.macPin.pinError" : "settingsMore.macPin.unpinError"),
      );
    } finally {
      setBusyIface(null);
      adapters.refresh();
    }
  };

  const list = adapters.data?.adapters ?? null;

  return (
    <div className="flex flex-col gap-[0.6rem]">
      <SectionGroup>
        <SettingRow title={t("settingsMore.macPin.autoPinLabel")} summary={t("settingsMore.macPin.autoPinHint")}>
          <Toggle
            on={autoPin}
            disabled={saving}
            label={t("settingsMore.macPin.autoPinLabel")}
            onChange={(next) => void save("network.mac_pin.enabled", next)}
          />
        </SettingRow>
        <SettingRow
          title={
            <span className="flex items-center gap-[0.4rem]">
              {t("settingsMore.macPin.liveRetagLabel")}
              <span className={liveAllowed ? "text-[0.75rem] text-warn" : "text-[0.75rem] text-muted-foreground"}>
                {liveAllowed ? t("cockpit.settingsSections.macPin.liveOnState") : t("cockpit.settingsSections.macPin.liveOffState")}
              </span>
            </span>
          }
          summary={t("settingsMore.macPin.liveRetagHint")}
        >
          <GuardedButton
            label={liveAllowed ? t("cockpit.settingsSections.macPin.liveOff") : t("cockpit.settingsSections.macPin.liveOn")}
            guardLabel={t("cockpit.settingsSections.macPin.holdLive")}
            icon={ShieldAlert}
            disabled={saving}
            onFired={() => void save("network.mac_pin.apply_live_allowed", !liveAllowed)}
          />
        </SettingRow>
      </SectionGroup>

      <SectionGroup title={t("settingsMore.macPin.trackedAdapters")}>
        {adapters.ready && list === null ? (
          <EmptyNote>{t("settingsMore.macPin.readError")}</EmptyNote>
        ) : !adapters.ready ? (
          <EmptyNote>{t("settingsMore.macPin.reading")}</EmptyNote>
        ) : list === null || list.length === 0 ? (
          <EmptyNote>{t("settingsMore.macPin.empty")}</EmptyNote>
        ) : (
          <>
            {list.map((a, idx) => (
              <AdapterRows
                key={a.usbPath || a.name || a.vidpid || String(idx)}
                adapter={a}
                busy={a.name != null && busyIface === a.name}
                onFire={(action) => void fire(action)}
              />
            ))}
            {list.some((a) => a.pinnedMac) ? (
              <p className="px-[0.75rem] text-[0.75rem] text-muted-foreground">
                {t("settingsMore.macPin.pinnedFooter")}
              </p>
            ) : null}
          </>
        )}
      </SectionGroup>
    </div>
  );
}

function AdapterRows({
  adapter,
  busy,
  onFire,
}: {
  adapter: MacAdapter;
  busy: boolean;
  onFire: (action: MacPinAction) => void;
}) {
  const { t } = useTranslation();
  const state = adapter.state ?? "stable";
  const action = adapterAction(adapter);
  const title = adapter.name || adapter.vidpid || t("settingsMore.macPin.unknownAdapter");

  return (
    <div className="flex flex-col gap-[0.2rem] rounded-md border border-border py-[0.3rem]">
      <SettingRow
        title={
          <span className="flex items-center gap-[0.5rem]">
            <span className="font-mono">{title}</span>
            <span className={`text-[0.75rem] ${toneClass(adapterTone(state))}`}>{state}</span>
          </span>
        }
        summary={
          action?.kind === "pin"
            ? t("settingsMore.macPin.pinHint")
            : action?.kind === "unpin"
              ? t("settingsMore.macPin.unpinHint")
              : undefined
        }
      >
        {action ? (
          <GuardedButton
            label={
              busy
                ? t(action.kind === "pin" ? "settingsMore.macPin.pinning" : "settingsMore.macPin.removing")
                : t(action.kind === "pin" ? "settingsMore.macPin.pinNextBoot" : "settingsMore.macPin.unpin")
            }
            guardLabel={t(
              action.kind === "pin" ? "cockpit.settingsSections.macPin.holdPin" : "cockpit.settingsSections.macPin.holdUnpin",
            )}
            icon={action.kind === "pin" ? Pin : PinOff}
            disabled={busy}
            onFired={() => onFire(action)}
          />
        ) : null}
      </SettingRow>
      <div className="flex flex-col gap-[0.2rem] px-[0.4rem]">
        {adapter.vidpid ? <Row label={t("settingsMore.macPin.chipset")} value={adapter.vidpid} /> : null}
        {adapter.pinnedMac ? <Row label={t("settingsMore.macPin.pinnedMac")} value={adapter.pinnedMac} /> : null}
        {adapter.lastSeenMac && adapter.lastSeenMac !== adapter.pinnedMac ? (
          <Row label={t("settingsMore.macPin.currentMac")} value={adapter.lastSeenMac} />
        ) : null}
        {adapter.source ? <Row label={t("settingsMore.macPin.source")} value={adapter.source} mono={false} /> : null}
      </div>
      {adapter.deferredReason ? (
        <p className="px-[0.75rem] text-[0.75rem] text-warn">
          {t("settingsMore.macPin.deferred", { reason: adapter.deferredReason })}
        </p>
      ) : null}
    </div>
  );
}
